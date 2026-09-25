//! # ultnas-core
//!
//! Core library for the Ultnas sovereign archiving system.
//!
//! Provides content addressing, namespace management, record sealing,
//! policy evaluation, vault I/O, invisible-character detection, tracked-file
//! state, and the unified error type.
//!
//! ## Example
//!
//! ```rust,no_run
//! use ultnas_core::{Vault, NamespacePath, RecordBuilder};
//! use std::path::Path;
//!
//! # fn main() -> Result<(), ultnas_core::UltnasCoreError> {
//! let vault = Vault::init(Path::new("./my-vault"), "personal-archive")?;
//! let ns = NamespacePath::parse("documents/2026")?;
//! let content = b"Hello, sovereign archive!";
//! let record = RecordBuilder::new(ns, "hello-doc")
//!     .tag("example")
//!     .media_type("text/plain")
//!     .build(content)?;
//! vault.write_record(&record, content)?;
//! # Ok(())
//! # }
//! ```

pub mod address;
pub mod error;
pub mod invisible;
pub mod journal;
pub mod namespace;
pub mod policy;
pub mod record;
pub mod tracking;
pub mod vault;

// ── Convenience re-exports at crate root ─────────────────────────────────────

// address
pub use address::{hash_bytes, hash_file, hash_reader, ContentId};

// error
pub use error::UltnasCoreError;

// journal
pub use journal::{
    apply_quarantine_entry, Journal, JournalEntry, JournalOp, JournalTail, QuarantineChange,
};

// namespace
pub use namespace::{NamespacePath, NamespaceTree};

// policy
pub use policy::{
    ApprovalMode, ConflictStrategy, GlobalPolicy, IntegrityPolicy, NamespacePolicy, Policy,
    PolicyEvaluator, RetentionPolicy,
};

// record
pub use record::{Record, RecordBuilder, RecordSeal};

// vault
pub use vault::{recreate_file, rewrite_file, Vault, VaultManifest};

// tracking
pub use tracking::{canonical_path, TrackedFile};
