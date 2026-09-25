//! Ultnas Daemon — background archiving, watching, and policy enforcement.

use anyhow::{Context, Result};
use clap::Parser;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use ultnas_core::{Journal, Policy, Vault};

mod services;
use services::{
    integrity_guard::{IntegrityGuard, RestoreOrder},
    ipc::IpcServer,
    policy_enforcer::PolicyEnforcer,
    scheduler::Scheduler,
    vault_lock::VaultLock,
    verified_cache::VerifiedCache,
    watcher::WatcherService,
};

#[derive(Parser)]
#[command(name = "ultnasd", about = "Ultnas background daemon", version)]
struct Args {
    #[arg(long, default_value = ".")]
    vault: PathBuf,
    /// Policy TOML (default: the vault manifest's `policy_path`, else built-in defaults)
    #[arg(long)]
    policy: Option<PathBuf>,
    // The flags below override the policy's `[global.integrity]` values.
    /// In-memory VerifiedCache budget in bytes (policy default 256 MiB)
    #[arg(long)]
    cache_bytes: Option<u64>,
    #[arg(long)]
    violation_threshold: Option<u32>,
    #[arg(long)]
    violation_window_secs: Option<u64>,
    #[arg(long)]
    debounce_ms: Option<u64>,
    #[arg(long)]
    escalate_after_restores: Option<u32>,
    #[arg(short, long)]
    verbose: bool,
}

/// `--policy`, else the manifest's `policy_path` (relative to the vault root),
/// else the built-in defaults.
fn load_policy(explicit: Option<&Path>, vault: &Vault) -> Result<Policy> {
    let path = explicit.map(Path::to_path_buf).or_else(|| {
        vault
            .manifest()
            .policy_path
            .as_ref()
            .map(|p| vault.root().join(p))
    });
    let Some(path) = path else {
        info!("no policy configured — using built-in defaults");
        return Ok(Policy::default());
    };
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading policy {}", path.display()))?;
    let policy =
        Policy::from_toml(&raw).with_context(|| format!("invalid policy {}", path.display()))?;
    info!("policy loaded from {}", path.display());
    Ok(policy)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let log_level = if args.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(log_level))
        .init();

    info!("ultnasd starting — vault: {}", args.vault.display());

    // Open (and so validate) the vault before creating the lock file in it,
    // but lock before the journal or anything else can write.
    let vault = Arc::new(Vault::open(&args.vault)?);

    // Held until main returns; the OS also releases it if we crash.
    let vault_lock = VaultLock::acquire(&args.vault)?;
    info!(
        "vault lock acquired: {} (PID {})",
        vault_lock.path().display(),
        std::process::id()
    );

    let journal = Arc::new(Journal::open(&args.vault.join("journal.log"))?);

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
    let guard = Arc::new(Mutex::new(guard));

    // Alert logger task
    tokio::spawn(async move {
        while let Some(alert) = alert_rx.recv().await {
            info!("IntegrityAlert: {:?}", alert);
        }
    });

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);

    // WatcherService
    {
        let v2 = vault.clone();
        let g2 = guard.clone();
        let p2 = policy.clone();
        tokio::spawn(async move {
            WatcherService::new(v2, g2, p2).run().await;
        });
    }

    // Scheduler
    {
        let v2 = vault.clone();
        tokio::spawn(async move {
            Scheduler::new(v2.root().to_path_buf()).run().await;
        });
    }

    // PolicyEnforcer — reports quarantine state held by IntegrityGuard
    {
        let g2 = guard.clone();
        tokio::spawn(async move {
            PolicyEnforcer::new(g2).run().await;
        });
    }

    // IpcServer
    {
        let v2 = vault.clone();
        let sd = shutdown_tx.clone();
        tokio::spawn(async move {
            IpcServer::new(v2.root().to_path_buf(), sd).run().await;
        });
    }

    tokio::select! {
        _ = tokio::signal::ctrl_c() => { info!("received SIGINT — shutting down"); }
        _ = shutdown_rx.recv()      => { info!("shutdown requested via IPC"); }
    }

    drop(vault_lock);
    info!("vault lock released — goodbye");
    Ok(())
}
