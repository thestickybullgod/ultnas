//! Ultnas Daemon — background archiving, watching, and policy enforcement.

use anyhow::Result;
use clap::Parser;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use ultnas_core::{Journal, Vault};

mod services;
use services::{
    integrity_guard::IntegrityGuard, ipc::IpcServer, policy_enforcer::PolicyEnforcer,
    scheduler::Scheduler, verified_cache::VerifiedCache, watcher::WatcherService,
};

#[derive(Parser)]
#[command(name = "ultnasd", about = "Ultnas background daemon", version)]
struct Args {
    #[arg(long, default_value = ".")]
    vault: PathBuf,
    #[arg(long)]
    policy: Option<PathBuf>,
    /// In-memory VerifiedCache budget in bytes (default 256 MiB)
    #[arg(long, default_value_t = 256 * 1024 * 1024)]
    cache_bytes: u64,
    #[arg(long, default_value_t = 5)]
    violation_threshold: u32,
    #[arg(long, default_value_t = 300)]
    violation_window_secs: u64,
    #[arg(long, default_value_t = 100)]
    debounce_ms: u64,
    #[arg(long, default_value_t = 3)]
    escalate_after_restores: u32,
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let log_level = if args.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(log_level))
        .init();

    info!("ultnasd starting — vault: {}", args.vault.display());

    let lock_path = args.vault.join(".ultnas-lock");
    std::fs::write(&lock_path, std::process::id().to_string())?;
    info!("vault lock acquired (PID {})", std::process::id());

    let vault = Arc::new(Vault::open(&args.vault)?);
    let journal = Arc::new(Journal::open(&args.vault.join("journal.log"))?);

    // ── Fix 1: warm VerifiedCache off the tokio executor via spawn_blocking ──
    // warm_from_vault() does synchronous directory traversal + file I/O.
    // Running it directly on the async executor would starve other tasks.
    // We build the cache in a dedicated blocking thread, then wrap it.
    let vault_for_warmup = vault.clone();
    let cache_bytes = args.cache_bytes;
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
            args.violation_threshold,
            args.violation_window_secs,
            args.debounce_ms,
            args.escalate_after_restores,
        );
        tokio::task::spawn_blocking(move || {
            IntegrityGuard::new(
                v, j, c, alert_tx, threshold, window, debounce, true, escalate,
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
        tokio::spawn(async move {
            WatcherService::new(v2, g2).run().await;
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

    let _ = std::fs::remove_file(&lock_path);
    info!("vault lock released — goodbye");
    Ok(())
}
