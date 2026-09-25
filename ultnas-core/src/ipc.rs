//! CLI ↔ daemon IPC: newline-delimited JSON over a local endpoint.
//!
//! The endpoint is the Unix socket `<vault>/.ultnas.sock` (mode 0600), or on
//! Windows the named pipe `\\.\pipe\ultnas-<hash of vault path>`, which
//! refuses remote clients. Each request is one JSON line answered by one
//! JSON line; a connection may carry several. Lines over [`MAX_LINE`] bytes
//! are refused.
//!
//! Only one daemon can hold a vault's lock, so only one can own its
//! endpoint: the daemon clears a stale socket before binding, and on
//! Windows claims the pipe as its first instance, so a squatter makes it
//! fail loudly rather than share.

use crate::{hash_bytes, UltnasCoreError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

/// Longest request or response line accepted, in bytes.
pub const MAX_LINE: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<IpcError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcError {
    pub code: String,
    pub message: String,
}

impl Response {
    pub fn ok(id: &str, data: impl Serialize) -> Self {
        Self {
            id: id.to_string(),
            ok: true,
            data: serde_json::to_value(data).ok(),
            error: None,
        }
    }

    pub fn err(id: &str, code: &str, message: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            ok: false,
            data: None,
            error: Some(IpcError {
                code: code.to_string(),
                message: message.into(),
            }),
        }
    }
}

/// Answer to `status`: the daemon's live state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub pid: u32,
    pub version: String,
    pub vault: PathBuf,
    pub started_at: DateTime<Utc>,
    pub journal: JournalHealth,
    /// Alerts dropped because the alert channel was full.
    pub dropped_alerts: u64,
    pub quarantined: Vec<String>,
    pub watcher: WatcherStatus,
    pub cache: CacheStatus,
    /// Newest last.
    pub recent_alerts: Vec<AlertRecord>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JournalHealth {
    /// Entries are being buffered in memory and repairs are suspended.
    pub degraded: bool,
    pub buffered_entries: usize,
    /// Entries dropped because the buffer overflowed while degraded.
    pub lost_entries: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WatcherStatus {
    /// `"events"`, `"polling"`, or `"starting"`.
    pub mode: String,
    pub watched_dirs: usize,
    pub unwatchable_dirs: usize,
    pub last_full_scan: Option<DateTime<Utc>>,
    pub tracked_files: usize,
    pub tracked_dirs: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CacheStatus {
    pub used_bytes: u64,
    pub budget_bytes: u64,
    pub entries: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertRecord {
    pub at: DateTime<Utc>,
    pub kind: String,
    pub detail: String,
}

/// Where a vault's daemon listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Socket(PathBuf),
    Pipe(String),
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Endpoint::Socket(p) => write!(f, "{}", p.display()),
            Endpoint::Pipe(name) => f.write_str(name),
        }
    }
}

/// The endpoint for the vault at `vault_root`, which must be canonical so
/// the daemon and CLI derive the same one.
pub fn endpoint(vault_root: &Path) -> Result<Endpoint, UltnasCoreError> {
    if cfg!(windows) {
        let key = hash_bytes(vault_root.to_string_lossy().to_lowercase().as_bytes());
        return Ok(Endpoint::Pipe(format!(
            r"\\.\pipe\ultnas-{}",
            &key.to_hex()[..16]
        )));
    }
    let socket = vault_root.join(".ultnas.sock");
    // sun_path is 104 bytes on macOS, 108 on Linux, including the NUL.
    if socket.as_os_str().len() > 100 {
        return Err(UltnasCoreError::Ipc(format!(
            "{} is too long a path for a Unix socket; move the vault somewhere shorter to use IPC",
            socket.display()
        )));
    }
    Ok(Endpoint::Socket(socket))
}

/// Send one command to the vault's daemon and wait for the answer. Blocking;
/// for the CLI. `vault_root` must be canonical.
pub fn request(
    vault_root: &Path,
    command: &str,
    timeout: Duration,
) -> Result<Response, UltnasCoreError> {
    let ep = endpoint(vault_root)?;
    let req = Request {
        id: format!(
            "{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ),
        command: command.to_string(),
        params: serde_json::Value::Null,
    };
    let mut line =
        serde_json::to_string(&req).map_err(|e| UltnasCoreError::Serialization(e.to_string()))?;
    line.push('\n');

    let mut stream = connect(&ep, timeout)?;
    stream.write_all(line.as_bytes())?;
    stream.flush()?;

    let mut reply = String::new();
    BufReader::new(stream.take(MAX_LINE as u64)).read_line(&mut reply)?;
    if !reply.ends_with('\n') {
        return Err(UltnasCoreError::Ipc(
            "the daemon closed the connection without a complete answer".into(),
        ));
    }
    let resp: Response = serde_json::from_str(&reply)
        .map_err(|e| UltnasCoreError::Ipc(format!("unreadable answer from the daemon: {e}")))?;
    if resp.id != req.id {
        return Err(UltnasCoreError::Ipc(
            "answer was for a different request".into(),
        ));
    }
    Ok(resp)
}

trait Stream: Read + Write {}
impl<T: Read + Write> Stream for T {}

fn not_running(ep: &Endpoint) -> UltnasCoreError {
    UltnasCoreError::DaemonNotRunning(ep.to_string())
}

#[cfg(unix)]
fn connect(ep: &Endpoint, timeout: Duration) -> Result<Box<dyn Stream>, UltnasCoreError> {
    use std::{io::ErrorKind, os::unix::net::UnixStream};
    let Endpoint::Socket(path) = ep else {
        unreachable!("Unix endpoints are sockets")
    };
    let stream = UnixStream::connect(path).map_err(|e| match e.kind() {
        ErrorKind::NotFound | ErrorKind::ConnectionRefused => not_running(ep),
        _ => e.into(),
    })?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(Box::new(stream))
}

#[cfg(windows)]
fn connect(ep: &Endpoint, timeout: Duration) -> Result<Box<dyn Stream>, UltnasCoreError> {
    use std::{fs::OpenOptions, io::ErrorKind, time::Instant};
    const ERROR_PIPE_BUSY: i32 = 231;
    let Endpoint::Pipe(name) = ep else {
        unreachable!("Windows endpoints are pipes")
    };
    let deadline = Instant::now() + timeout;
    loop {
        match OpenOptions::new().read(true).write(true).open(name) {
            Ok(pipe) => return Ok(Box::new(pipe)),
            // Every instance is serving another client; one frees up shortly.
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) if e.kind() == ErrorKind::NotFound => return Err(not_running(ep)),
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_roundtrip_and_omit_empty_fields() {
        let ok = Response::ok("1", serde_json::json!({"a": 1}));
        let json = serde_json::to_string(&ok).unwrap();
        assert!(!json.contains("error"), "{json}");
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), ok);

        let err = Response::err("2", "UNKNOWN_COMMAND", "no such command");
        let json = serde_json::to_string(&err).unwrap();
        assert!(!json.contains("data"), "{json}");
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), err);
    }

    #[test]
    fn requests_accept_missing_params() {
        let r: Request = serde_json::from_str(r#"{"id":"x","command":"status"}"#).unwrap();
        assert_eq!(r.command, "status");
        assert!(r.params.is_null());
    }

    #[test]
    fn endpoint_is_stable_and_distinct_per_vault() {
        let a = endpoint(Path::new("/vaults/a")).unwrap();
        assert_eq!(a, endpoint(Path::new("/vaults/a")).unwrap());
        assert_ne!(a, endpoint(Path::new("/vaults/b")).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn overlong_socket_paths_are_refused() {
        let long = PathBuf::from("/").join("x".repeat(120));
        assert!(matches!(endpoint(&long), Err(UltnasCoreError::Ipc(_))));
    }

    #[test]
    fn no_daemon_means_not_running() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        assert!(matches!(
            request(&root, "status", Duration::from_secs(1)),
            Err(UltnasCoreError::DaemonNotRunning(_))
        ));
    }
}
