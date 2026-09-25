//! Unified error type for all `ultnas-core` operations.

use std::path::PathBuf;
use thiserror::Error;

/// The unified error type returned by all fallible `ultnas-core` operations.
#[derive(Debug, Error)]
pub enum UltnasCoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid namespace path: {reason}")]
    InvalidNamespace { reason: String },

    #[error("invalid content ID: {0}")]
    InvalidContentId(String),

    #[error("content integrity check failed — expected {expected}, got {actual}")]
    IntegrityFailure { expected: String, actual: String },

    #[error("record is already sealed")]
    AlreadySealed,

    #[error("record is not sealed")]
    NotSealed,

    #[error("seal verification failed: {0}")]
    SealVerificationFailed(String),

    #[error("policy violation: rule `{rule}` — {detail}")]
    PolicyViolation { rule: String, detail: String },

    #[error("vault not found at path: {0}")]
    VaultNotFound(PathBuf),

    #[error("vault already exists at path: {0}")]
    VaultAlreadyExists(PathBuf),

    #[error("vault is locked by PID {pid}")]
    VaultLocked { pid: u32 },

    #[error("record not found: {0}")]
    RecordNotFound(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("TOML parse error: {0}")]
    TomlParse(#[from] toml::de::Error),

    #[error("invalid policy: {0}")]
    InvalidPolicy(String),

    #[error("journal error: {0}")]
    Journal(String),

    /// A path the daemon must never track, such as a kernel pseudo-filesystem
    /// where "stripping" a character would mean writing a kernel setting.
    #[error("{path} can't be tracked: {reason}")]
    Untrackable { path: PathBuf, reason: String },

    /// A live file changed between being inspected and being replaced; the
    /// replacement was abandoned so the newer write isn't lost.
    #[error("{0} changed while it was being replaced")]
    ChangedDuringWrite(PathBuf),

    /// Emitted by IntegrityGuard when all restore tiers are exhausted.
    #[error("restore failed for {id}: {reason}")]
    RestoreFailed { id: String, reason: String },
}
