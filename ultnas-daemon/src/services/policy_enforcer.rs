//! PolicyEnforcer — periodic policy compliance scanner.
//!
//! Quarantine state is owned by `IntegrityGuard` and synced from the journal
//! incrementally before every watcher check (the watcher reacts to the
//! journal changing), so CLI-issued `QuarantineLift` operations never wait
//! for this 600 s cycle.
//! This service only reports the current state.
//!
//! Sequence:
//!   1. In `spawn_blocking`: lock the guard, sync from the journal,
//!      snapshot quarantined namespaces + health
//!   2. Log them
//!   3. Sleep 600 s and repeat

use std::sync::{Arc, Mutex};
use tokio::time::{interval, Duration};
use tracing::{error, info, warn};

use super::integrity_guard::{lock, IntegrityGuard};

pub struct PolicyEnforcer {
    guard: Arc<Mutex<IntegrityGuard>>,
}

impl PolicyEnforcer {
    pub fn new(guard: Arc<Mutex<IntegrityGuard>>) -> Self {
        Self { guard }
    }

    pub async fn run(self) {
        info!("PolicyEnforcer: started (cycle = 600 s)");
        let mut tick = interval(Duration::from_secs(600));

        loop {
            tick.tick().await;
            self.scan_cycle().await;
        }
    }

    async fn scan_cycle(&self) {
        info!("PolicyEnforcer: running compliance scan");

        let guard = self.guard.clone();
        let snapshot = tokio::task::spawn_blocking(move || {
            let mut g = lock(&guard);
            g.sync_quarantine();
            (g.quarantined(), g.is_degraded(), g.dropped_alerts())
        })
        .await;

        let (quarantined, degraded, dropped_alerts) = match snapshot {
            Ok(s) => s,
            Err(e) => {
                error!("PolicyEnforcer: quarantine sync panicked: {}", e);
                return;
            }
        };

        if quarantined.is_empty() {
            info!("PolicyEnforcer: no namespaces currently quarantined");
        } else {
            warn!(
                "PolicyEnforcer: {} namespace(s) still quarantined: {}",
                quarantined.len(),
                quarantined.join(", ")
            );
        }
        if degraded {
            warn!(
                "PolicyEnforcer: journal is unwritable — IntegrityGuard auto-restore is suspended"
            );
        }
        if dropped_alerts > 0 {
            warn!(
                "PolicyEnforcer: {} integrity alert(s) dropped since startup",
                dropped_alerts
            );
        }

        // TODO v0.5: full policy-compliance sweep (retention age checks,
        // seal-required enforcement, tag validation)
    }
}
