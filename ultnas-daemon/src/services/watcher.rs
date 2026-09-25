//! File-system watcher service.
//!
//! Monitors the vault's `objects/` and `records/` directories for unauthorized
//! write events on sealed records and forwards detected violations to the
//! `IntegrityGuard`.
//!
//! Current implementation: polling on a 30-second interval (stub).
//! v0.3 will integrate the `notify` crate for true inotify/FSEvents/RDCW support.
//!
//! Each scan reads and hashes every sealed object, so the whole scan — and the
//! guard calls it makes — runs in `spawn_blocking`, off the async workers.

use std::sync::{Arc, Mutex};
use tracing::{debug, error, info, warn};
use ultnas_core::Vault;

use super::integrity_guard::{lock, IntegrityGuard, Violation};

pub struct WatcherService {
    vault: Arc<Vault>,
    guard: Arc<Mutex<IntegrityGuard>>,
    poll_interval_s: u64,
}

impl WatcherService {
    pub fn new(vault: Arc<Vault>, guard: Arc<Mutex<IntegrityGuard>>) -> Self {
        Self {
            vault,
            guard,
            poll_interval_s: 30,
        }
    }

    /// Main service loop. Polls for integrity violations on sealed records.
    pub async fn run(self) {
        info!(
            "WatcherService started (poll interval: {}s)",
            self.poll_interval_s
        );
        let mut interval =
            tokio::time::interval(tokio::time::Duration::from_secs(self.poll_interval_s));
        loop {
            interval.tick().await;
            debug!("WatcherService: scanning sealed records for write violations");
            let (vault, guard) = (self.vault.clone(), self.guard.clone());
            if let Err(e) = tokio::task::spawn_blocking(move || scan(&vault, &guard)).await {
                error!("WatcherService: scan task panicked: {}", e);
            }
        }
    }
}

/// Apply pending quarantine lifts, then report every sealed record whose
/// object no longer matches its ContentId (including missing objects).
fn scan(vault: &Vault, guard: &Mutex<IntegrityGuard>) {
    lock(guard).sync_quarantine();

    let records = match vault.all_records() {
        Ok(records) => records,
        Err(e) => {
            warn!("WatcherService: could not list records: {}", e);
            return;
        }
    };

    for record in records.into_iter().filter(|r| r.is_sealed()) {
        if let Err(e) = vault.verify(&record.id) {
            warn!(
                "WatcherService: integrity violation detected on {}: {}",
                record.id, e
            );
            lock(guard).record_violation(Violation {
                id: record.id,
                namespace: record.namespace.as_str(),
            });
        }
    }
}
