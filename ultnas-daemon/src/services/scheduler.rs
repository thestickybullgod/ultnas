//! Scheduler service — applies the policy's retention on a schedule.
//!
//! Once an hour (first a minute after startup), it purges what
//! `[namespaces.retention]` says to: records past `keep_days`, and versions
//! beyond `keep_versions` per series. A tracked file's current versions are
//! never purged (see `ultnas_core::retention`). While the journal is
//! unwritable it skips, so nothing is deleted that can't be recorded.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tracing::{debug, error, info, warn};
use ultnas_core::{retention, Journal, Policy, Vault};

use super::integrity_guard::{lock, IntegrityGuard};

const PERIOD: Duration = Duration::from_secs(3600);
const FIRST_RUN_DELAY: Duration = Duration::from_secs(60);

pub struct Scheduler {
    vault: Arc<Vault>,
    journal: Arc<Journal>,
    policy: Arc<Policy>,
    guard: Arc<Mutex<IntegrityGuard>>,
}

impl Scheduler {
    pub fn new(
        vault: Arc<Vault>,
        journal: Arc<Journal>,
        policy: Arc<Policy>,
        guard: Arc<Mutex<IntegrityGuard>>,
    ) -> Self {
        Self {
            vault,
            journal,
            policy,
            guard,
        }
    }

    pub async fn run(self) {
        if !self.policy.namespaces.iter().any(|n| n.retention.is_some()) {
            info!("Scheduler: no retention configured — nothing to schedule");
            return;
        }
        info!("Scheduler started (retention every {}s)", PERIOD.as_secs());
        let start = tokio::time::Instant::now() + FIRST_RUN_DELAY;
        let mut interval = tokio::time::interval_at(start, PERIOD);
        loop {
            interval.tick().await;
            let (v, j, p, g) = (
                self.vault.clone(),
                self.journal.clone(),
                self.policy.clone(),
                self.guard.clone(),
            );
            if let Err(e) = tokio::task::spawn_blocking(move || rotate_once(&v, &j, &p, &g)).await {
                error!("Scheduler: retention task panicked: {}", e);
            }
        }
    }
}

/// One retention pass. Returns how many records were purged.
fn rotate_once(
    vault: &Vault,
    journal: &Journal,
    policy: &Policy,
    guard: &Mutex<IntegrityGuard>,
) -> usize {
    let mirror = {
        let g = lock(guard);
        if g.is_degraded() {
            warn!("Scheduler: journal unavailable — skipping retention");
            return 0;
        }
        g.mirror()
    };
    match retention::rotate(vault, policy, journal, mirror.as_deref(), false) {
        Ok(purged) if purged.is_empty() => {
            debug!("Scheduler: retention — nothing to purge");
            0
        }
        Ok(purged) => {
            for p in &purged {
                debug!(
                    "Scheduler: purged {} ({} / {}): {}",
                    p.id, p.namespace, p.label, p.reason
                );
            }
            info!("Scheduler: retention purged {} record(s)", purged.len());
            purged.len()
        }
        Err(e) => {
            warn!("Scheduler: retention stopped: {}", e);
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{integrity_guard::RestoreOrder, verified_cache::VerifiedCache};
    use super::*;
    use chrono::Utc;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use ultnas_core::{NamespacePath, RecordBuilder};

    #[test]
    fn a_pass_purges_what_retention_says() {
        let dir = TempDir::new().unwrap();
        let vault = Arc::new(Vault::init(dir.path(), "t").unwrap());
        let journal = Arc::new(Journal::open(&dir.path().join("journal.log")).unwrap());
        let policy = Policy::from_toml(
            "version = 1\n[[namespaces]]\npath = \"logs\"\n  [namespaces.retention]\n  keep_days = 7\n",
        )
        .unwrap();
        let mut old = RecordBuilder::new(NamespacePath::parse("logs").unwrap(), "l")
            .build(b"old")
            .unwrap();
        old.created_at = Utc::now() - chrono::Duration::days(8);
        vault.write_record(&old, b"old").unwrap();
        let fresh = RecordBuilder::new(NamespacePath::parse("logs").unwrap(), "l")
            .build(b"fresh")
            .unwrap();
        vault.write_record(&fresh, b"fresh").unwrap();

        let (tx, _rx) = mpsc::channel(8);
        let guard = Mutex::new(IntegrityGuard::new(
            vault.clone(),
            journal.clone(),
            Arc::new(Mutex::new(VerifiedCache::new(0))),
            tx,
            5,
            300,
            0,
            true,
            3,
            RestoreOrder::MemoryThenStore,
        ));
        assert_eq!(rotate_once(&vault, &journal, &policy, &guard), 1);
        assert!(vault.get_record(&old.id).is_err());
        assert!(vault.get_record(&fresh.id).is_ok());
        assert_eq!(rotate_once(&vault, &journal, &policy, &guard), 0);
    }
}
