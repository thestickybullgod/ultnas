//! IPC server — Unix socket server for CLI ↔ daemon communication.
//!
//! Protocol: newline-delimited JSON request/response.
//! Alert types live in `integrity_guard::IntegrityAlert`.

use std::path::PathBuf;
use tokio::sync::broadcast::Sender;
use tracing::{debug, info};

// ── IPC Server ────────────────────────────────────────────────────────────────

pub struct IpcServer {
    vault_path: PathBuf,
    #[allow(dead_code)] // used by the `shutdown` request handler (v0.3)
    shutdown_tx: Sender<()>,
}

impl IpcServer {
    pub fn new(vault_path: PathBuf, shutdown_tx: Sender<()>) -> Self {
        Self {
            vault_path,
            shutdown_tx,
        }
    }

    pub async fn run(self) {
        let socket_path = self.vault_path.join(".ultnas.sock");
        // TODO(v0.3): bind tokio::net::UnixListener and accept connections
        info!("IpcServer would listen at {}", socket_path.display());
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            debug!("IpcServer heartbeat (socket not yet bound — planned v0.3)");
        }
    }
}
