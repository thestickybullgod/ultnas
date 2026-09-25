//! Ultnas Daemon — background archiving, watching, and policy enforcement.

use anyhow::{Context, Result};
use clap::Parser;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;
use tracing::{info, warn};
use ultnas_core::{Journal, Policy, Vault};

mod logging;
mod services;
use services::{
    integrity_guard::{IntegrityGuard, RestoreOrder},
    ipc::{IpcServer, StatusSources},
    policy_enforcer::PolicyEnforcer,
    scheduler::Scheduler,
    vault_lock::VaultLock,
    verified_cache::VerifiedCache,
    watcher::WatcherService,
};

#[derive(Parser)]
#[command(
    name = "ultnasd",
    about = "Ultnas daemon: watches tracked files and undoes invisible-character writes",
    version
)]
struct Args {
    /// Vault to guard, one daemon per vault [default: $ULTNAS_VAULT, else
    /// ~/.local/share/ultnas, or /var/lib/ultnas as root]
    #[arg(long, env = "ULTNAS_VAULT")]
    vault: Option<PathBuf>,
    /// Policy TOML (default: the vault manifest's `policy_path`, else built-in defaults)
    #[arg(long)]
    policy: Option<PathBuf>,
    // The flags below override the policy's `[global.integrity]` values.
    /// In-memory VerifiedCache budget in bytes (policy default 256 MiB)
    #[arg(long)]
    cache_bytes: Option<u64>,
    /// Violations within the window before a file is deleted and recreated
    /// (below it, each is stripped in place)
    #[arg(long)]
    violation_threshold: Option<u32>,
    /// Seconds of quiet after which a file's violation count resets
    #[arg(long)]
    violation_window_secs: Option<u64>,
    /// Violations this close together share one count (each is still repaired)
    #[arg(long)]
    debounce_ms: Option<u64>,
    /// Restores in a namespace before it is quarantined
    #[arg(long)]
    escalate_after_restores: Option<u32>,
    /// Full re-hash of every sealed object and tracked file, as a backstop
    /// to file-system events
    #[arg(long, default_value_t = 300)]
    scan_interval_secs: u64,
    /// Mirror directory: a second copy of every sealed object and tracked
    /// version, used when a restore finds nothing else (overrides the policy)
    #[arg(long)]
    mirror: Option<PathBuf>,
    /// Log at debug level
    #[arg(short, long)]
    verbose: bool,
    /// Directory for the daily-rotated log file (default: <vault>/logs)
    #[arg(long)]
    log_dir: Option<PathBuf>,
    /// How many daily log files to keep
    #[arg(long, default_value_t = 14)]
    log_keep_days: usize,
    /// Log to stdout only (e.g. when a service manager already captures it)
    #[arg(long, conflicts_with = "log_dir")]
    no_log_file: bool,
}

/// `--policy`, else the manifest's `policy_path`, else built-in defaults.
fn load_policy(explicit: Option<&Path>, vault: &Vault) -> Result<Policy> {
    match vault.load_policy(explicit).context("loading the policy")? {
        Some((policy, path)) => {
            info!("policy loaded from {}", path.display());
            Ok(policy)
        }
        None => {
            info!("no policy configured — using built-in defaults");
            Ok(Policy::default())
        }
    }
}

/// Whether two paths are on one filesystem (Unix; elsewhere unknown: false).
fn same_device(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        matches!(
            (std::fs::metadata(a), std::fs::metadata(b)),
            (Ok(x), Ok(y)) if x.dev() == y.dev()
        )
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        false
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Open (and so validate) the vault before creating anything in it —
    // log directory or lock file — but lock before anything else can write.
    let vault_root = args.vault.clone().unwrap_or_else(Vault::default_root);
    let vault = Arc::new(Vault::open(&vault_root).with_context(|| {
        format!(
            "no usable vault at {} (create one with `ultnas setup`)",
            vault_root.display()
        )
    })?);

    let log_level = if args.verbose { "debug" } else { "info" };
    let log_dir = args
        .log_dir
        .clone()
        .unwrap_or_else(|| vault.root().join("logs"));
    let file = (!args.no_log_file).then_some((log_dir.as_path(), args.log_keep_days));
    // Flushes the log file when main returns.
    let _log_guard = logging::init(log_level, file)?;

    info!("ultnasd starting — vault: {}", vault_root.display());
    if !args.no_log_file {
        info!(
            "logging to {} (daily, keeping {} files)",
            log_dir.display(),
            args.log_keep_days
        );
    }

    // Held until main returns; the OS also releases it if we crash.
    let vault_lock = VaultLock::acquire(&vault_root)?;
    info!(
        "vault lock acquired: {} (PID {})",
        vault_lock.path().display(),
        std::process::id()
    );

    let journal = Arc::new(Journal::open(&vault_root.join("journal.log"))?);

    let policy = Arc::new(load_policy(args.policy.as_deref(), &vault)?);
    let ip = &policy.global.integrity;
    if ip.restore_source == "remote" {
        warn!("restore_source \"remote\" is not implemented yet — using memory_then_store");
    }
    info!(
        "tracked files: approval = {} (namespaces may override)",
        ip.approval.as_str()
    );

    // ── Fix 1: warm VerifiedCache off the tokio executor via spawn_blocking ──
    // warm_from_vault() does synchronous directory traversal + file I/O.
    // Running it directly on the async executor would starve other tasks.
    // We build the cache in a dedicated blocking thread, then wrap it.
    let vault_for_warmup = vault.clone();
    let cache_bytes = args.cache_bytes.unwrap_or(ip.cache_budget_bytes);
    let warmed_cache = tokio::task::spawn_blocking(move || {
        let mut cache = VerifiedCache::new(cache_bytes);
        match cache.warm_from_vault(&vault_for_warmup) {
            Ok(n) => info!(
                "VerifiedCache: warmed {} sealed records ({} bytes)",
                n,
                cache.used_bytes()
            ),
            Err(e) => warn!("VerifiedCache: warm-up error — {}", e),
        }
        cache
    })
    .await?;

    // std::sync::Mutex throughout: IntegrityGuard and VerifiedCache are only
    // locked inside spawn_blocking, never across an `.await`.
    let cache = Arc::new(Mutex::new(warmed_cache));

    let (alert_tx, mut alert_rx) = mpsc::channel(256);

    // IntegrityGuard::new replays quarantine state from the journal (file I/O).
    let guard = {
        let (v, j, c) = (vault.clone(), journal.clone(), cache.clone());
        let (threshold, window, debounce, escalate) = (
            args.violation_threshold
                .unwrap_or(ip.write_violation_threshold),
            args.violation_window_secs
                .unwrap_or(ip.violation_window_secs),
            args.debounce_ms.unwrap_or(ip.debounce_ms),
            args.escalate_after_restores
                .unwrap_or(ip.escalate_after_restores),
        );
        let order = RestoreOrder::from_policy(&ip.restore_source);
        tokio::task::spawn_blocking(move || {
            IntegrityGuard::new(
                v, j, c, alert_tx, threshold, window, debounce, true, escalate, order,
            )
        })
        .await?
    };
    let quarantined = guard.quarantined();
    if !quarantined.is_empty() {
        warn!(
            "{} namespace(s) quarantined at startup: {}",
            quarantined.len(),
            quarantined.join(", ")
        );
    }
    let mut guard = guard;
    let mirror_path = args
        .mirror
        .clone()
        .or_else(|| policy.mirror_path(vault.root()));
    if let Some(path) = mirror_path {
        let mirror = Arc::new(
            ultnas_core::Mirror::open(&path)
                .with_context(|| format!("opening mirror {}", path.display()))?,
        );
        info!("mirror: {}", path.display());
        if same_device(vault.root(), &path) {
            warn!(
                "mirror {} is on the same filesystem as the vault — it won't survive a \
                 disk failure; put it on another disk",
                path.display()
            );
        }
        guard.set_mirror(mirror);
    }
    let guard = Arc::new(Mutex::new(guard));

    // Alert logger task
    tokio::spawn(async move {
        while let Some(alert) = alert_rx.recv().await {
            info!("IntegrityAlert: {:?}", alert);
        }
    });

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);

    // WatcherService
    let watcher_status = {
        let watcher = WatcherService::new(
            vault.clone(),
            guard.clone(),
            policy.clone(),
            Duration::from_secs(args.scan_interval_secs.max(1)),
        );
        let status = watcher.status_handle();
        tokio::spawn(watcher.run());
        status
    };

    // Scheduler — applies retention
    tokio::spawn(
        Scheduler::new(
            vault.clone(),
            journal.clone(),
            policy.clone(),
            guard.clone(),
        )
        .run(),
    );

    // PolicyEnforcer — reports quarantine state held by IntegrityGuard
    {
        let g2 = guard.clone();
        tokio::spawn(async move {
            PolicyEnforcer::new(g2).run().await;
        });
    }

    // IpcServer — the endpoint is derived from the canonical vault path,
    // as the CLI derives it.
    {
        let sources = StatusSources {
            vault: std::fs::canonicalize(vault.root())?,
            started_at: chrono::Utc::now(),
            guard: guard.clone(),
            cache: cache.clone(),
            watcher: watcher_status,
        };
        tokio::spawn(IpcServer::new(sources, shutdown_tx.clone()).run());
    }

    tokio::select! {
        _ = tokio::signal::ctrl_c() => { info!("received SIGINT — shutting down"); }
        _ = shutdown_rx.recv()      => { info!("shutdown requested via IPC"); }
    }

    drop(vault_lock);
    info!("vault lock released — goodbye");
    Ok(())
}
