//! IPC server — answers the CLI over the vault's local endpoint.
//!
//! Protocol and endpoint naming live in `ultnas_core::ipc`; this binds the
//! endpoint (a Unix socket, or a named pipe on Windows) and serves:
//!
//! - `status`: live state — journal health, dropped alerts, quarantines,
//!   watcher mode and watch counts, cache use, and recent alerts.
//! - `stop`: shut the daemon down cleanly (the vault lock is released).
//!
//! Requests are handled on their own tasks; gathering status locks the
//! guard, so that runs in `spawn_blocking` like every other guard call.

use chrono::{DateTime, Utc};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    sync::broadcast::Sender,
};
use tracing::{debug, error, info, warn};
use ultnas_core::ipc::{self, CacheStatus, DaemonStatus, Endpoint, Request, Response};

use super::{
    integrity_guard::{lock, IntegrityGuard},
    verified_cache::SharedCache,
    watcher::SharedWatcherStatus,
};

/// Where `status` reads its answer from.
#[derive(Clone)]
pub struct StatusSources {
    pub vault: PathBuf,
    pub started_at: DateTime<Utc>,
    pub guard: Arc<Mutex<IntegrityGuard>>,
    pub cache: SharedCache,
    pub watcher: SharedWatcherStatus,
}

pub struct IpcServer {
    sources: StatusSources,
    shutdown_tx: Sender<()>,
}

impl IpcServer {
    pub fn new(sources: StatusSources, shutdown_tx: Sender<()>) -> Self {
        Self {
            sources,
            shutdown_tx,
        }
    }

    pub async fn run(self) {
        let ep = match ipc::endpoint(&self.sources.vault) {
            Ok(ep) => ep,
            Err(e) => {
                warn!("IpcServer: disabled — {}", e);
                return;
            }
        };
        if let Err(e) = self.serve(&ep).await {
            error!("IpcServer: stopped serving {}: {}", ep, e);
        }
    }

    #[cfg(unix)]
    async fn serve(&self, ep: &Endpoint) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let Endpoint::Socket(path) = ep else {
            unreachable!("Unix endpoints are sockets")
        };
        // We hold the vault lock, so any socket file here is a dead daemon's.
        let _ = std::fs::remove_file(path);
        let listener = tokio::net::UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        info!("IpcServer: listening on {}", path.display());
        loop {
            let (stream, _) = listener.accept().await?;
            self.spawn_handler(stream);
        }
    }

    #[cfg(windows)]
    async fn serve(&self, ep: &Endpoint) -> std::io::Result<()> {
        use tokio::net::windows::named_pipe::ServerOptions;
        let Endpoint::Pipe(name) = ep else {
            unreachable!("Windows endpoints are pipes")
        };
        // First instance: if something else already owns the name, fail
        // rather than share it.
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(name)?;
        info!("IpcServer: listening on {}", name);
        loop {
            server.connect().await?;
            let client = server;
            server = ServerOptions::new()
                .reject_remote_clients(true)
                .create(name)?;
            self.spawn_handler(client);
        }
    }

    fn spawn_handler<S>(&self, stream: S)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (sources, shutdown) = (self.sources.clone(), self.shutdown_tx.clone());
        tokio::spawn(async move {
            if let Err(e) = handle(stream, sources, shutdown).await {
                debug!("IpcServer: connection ended: {}", e);
            }
        });
    }
}

/// Serve requests on one connection until the client hangs up.
async fn handle<S>(stream: S, sources: StatusSources, shutdown: Sender<()>) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read, mut write) = tokio::io::split(stream);
    let mut reader = BufReader::new(read);
    loop {
        let mut line = String::new();
        let n = (&mut reader)
            .take(ipc::MAX_LINE as u64)
            .read_line(&mut line)
            .await?;
        if n == 0 {
            return Ok(());
        }
        if !line.ends_with('\n') {
            let resp = Response::err("", "TOO_LONG", "request line too long");
            write_line(&mut write, &resp).await?;
            return Ok(());
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => answer(&req, &sources, &shutdown).await,
            Err(e) => Response::err("", "BAD_REQUEST", e.to_string()),
        };
        write_line(&mut write, &resp).await?;
    }
}

async fn write_line<W: AsyncWrite + Unpin>(w: &mut W, resp: &Response) -> std::io::Result<()> {
    let mut out = serde_json::to_string(resp).map_err(std::io::Error::other)?;
    out.push('\n');
    w.write_all(out.as_bytes()).await?;
    w.flush().await
}

async fn answer(req: &Request, sources: &StatusSources, shutdown: &Sender<()>) -> Response {
    match req.command.as_str() {
        "status" => {
            let s = sources.clone();
            match tokio::task::spawn_blocking(move || status(&s)).await {
                Ok(status) => Response::ok(&req.id, status),
                Err(e) => Response::err(&req.id, "INTERNAL", e.to_string()),
            }
        }
        "stop" => {
            info!("IpcServer: stop requested");
            let _ = shutdown.send(());
            Response::ok(&req.id, serde_json::json!({ "stopping": true }))
        }
        other => Response::err(
            &req.id,
            "UNKNOWN_COMMAND",
            format!("unknown command `{other}` (try `status` or `stop`)"),
        ),
    }
}

fn status(s: &StatusSources) -> DaemonStatus {
    let (journal, dropped_alerts, quarantined, recent_alerts) = {
        let g = lock(&s.guard);
        (
            g.journal_health(),
            g.dropped_alerts(),
            g.quarantined(),
            g.recent_alerts(),
        )
    };
    let cache = {
        let c = lock(&s.cache);
        CacheStatus {
            used_bytes: c.used_bytes(),
            budget_bytes: c.max_bytes(),
            entries: c.entries(),
        }
    };
    DaemonStatus {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        vault: s.vault.clone(),
        started_at: s.started_at,
        journal,
        dropped_alerts,
        quarantined,
        watcher: lock(&s.watcher).clone(),
        cache,
        recent_alerts,
    }
}

#[cfg(test)]
mod tests {
    use super::super::integrity_guard::RestoreOrder;
    use super::super::verified_cache::VerifiedCache;
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use ultnas_core::{Journal, UltnasCoreError, Vault};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_stop_and_unknown_commands() {
        let dir = TempDir::new().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let vault = Arc::new(Vault::init(&root, "t").unwrap());
        let journal = Arc::new(Journal::open(&root.join("journal.log")).unwrap());
        let cache: SharedCache = Arc::new(Mutex::new(VerifiedCache::new(1024)));
        let (tx, _rx) = mpsc::channel(8);
        let guard = IntegrityGuard::new(
            vault,
            journal,
            cache.clone(),
            tx,
            5,
            300,
            0,
            true,
            3,
            RestoreOrder::MemoryThenStore,
        );
        let sources = StatusSources {
            vault: root.clone(),
            started_at: Utc::now(),
            guard: Arc::new(Mutex::new(guard)),
            cache,
            watcher: Default::default(),
        };
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::broadcast::channel(1);
        let server = tokio::spawn(IpcServer::new(sources, shutdown_tx).run());

        let ask = |cmd: &'static str| {
            let root = root.clone();
            tokio::task::spawn_blocking(move || {
                // The server may still be binding.
                for _ in 0..100 {
                    match ipc::request(&root, cmd, Duration::from_secs(5)) {
                        Err(UltnasCoreError::DaemonNotRunning(_)) => {
                            std::thread::sleep(Duration::from_millis(20))
                        }
                        other => return other,
                    }
                }
                ipc::request(&root, cmd, Duration::from_secs(5))
            })
        };

        let resp = ask("status").await.unwrap().unwrap();
        assert!(resp.ok, "{resp:?}");
        let status: DaemonStatus = serde_json::from_value(resp.data.unwrap()).unwrap();
        assert_eq!(status.pid, std::process::id());
        assert_eq!(status.vault, root);
        assert!(!status.journal.degraded);
        assert_eq!(status.cache.budget_bytes, 1024);

        let resp = ask("frobnicate").await.unwrap().unwrap();
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "UNKNOWN_COMMAND");

        let resp = ask("stop").await.unwrap().unwrap();
        assert!(resp.ok);
        tokio::time::timeout(Duration::from_secs(5), shutdown_rx.recv())
            .await
            .expect("stop must signal shutdown")
            .unwrap();
        server.abort();
    }
}
