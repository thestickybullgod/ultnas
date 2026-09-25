//! Scheduler service — runs periodic tasks (rotation, cleanup, integrity checks).

use std::path::PathBuf;
use tracing::{debug, info};

pub struct Scheduler {
    #[allow(dead_code)] // used by retention/rotation tasks (v0.3)
    vault_path: PathBuf,
}

impl Scheduler {
    pub fn new(vault_path: PathBuf) -> Self {
        Self { vault_path }
    }

    pub async fn run(self) {
        info!("Scheduler started");
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3600));
        loop {
            interval.tick().await;
            debug!("Scheduler tick — running scheduled tasks");
            // TODO(v0.3): dispatch retention, rotation, and cleanup tasks
        }
    }
}
