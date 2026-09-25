# Ultnas — Module Specifications

> Version: 0.1 | Crate: `ultnas-core` | Stability: Unstable (pre-alpha)

---

## Table of Contents
1. [address](#1-address-module)
2. [namespace](#2-namespace-module)
3. [record](#3-record-module)
4. [policy](#4-policy-module)
5. [vault](#5-vault-module)
6. [journal](#6-journal-module)
7. [error](#7-error-module)

---

## 1. `address` Module

**Purpose:** Content-addressing using BLAKE3. Provides the `ContentId` type used everywhere in the system.

### Public Types

```rust
/// A 32-byte BLAKE3 content digest. The canonical identifier for any record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentId(blake3::Hash);
```

### Public Functions

```rust
/// Hash arbitrary bytes and return a ContentId.
pub fn hash_bytes(data: &[u8]) -> ContentId;

/// Hash a file at `path` in streaming fashion (no full read into memory).
pub fn hash_file(path: &Path) -> Result<ContentId, UltnasCoreError>;

/// Hash a readable stream.
pub fn hash_reader<R: Read>(reader: &mut R) -> Result<ContentId, UltnasCoreError>;
```

### `ContentId` Methods

```rust
impl ContentId {
    /// Hex-encoded string representation (64 lowercase hex chars).
    pub fn to_hex(&self) -> String;

    /// Parse from a hex string.
    pub fn from_hex(s: &str) -> Result<Self, UltnasCoreError>;

    /// Raw 32-byte array.
    pub fn as_bytes(&self) -> &[u8; 32];
}
```

### Invariants
- `ContentId` is always 32 bytes.
- Two `ContentId` values are equal iff the underlying BLAKE3 hashes are equal.
- `to_hex()` and `from_hex()` are always round-trippable.

---

## 2. `namespace` Module

**Purpose:** Hierarchical namespace management. Provides `NamespacePath` and `NamespaceTree`.

### Public Types

```rust
/// A validated, hierarchical namespace path (e.g. "projects/ultnas/docs").
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NamespacePath(Vec<String>);

/// An in-memory view of the namespace tree backed by a sorted BTreeMap.
pub struct NamespaceTree { /* private */ }
```

### Public Functions & Methods

```rust
impl NamespacePath {
    /// Parse from a `/`-separated string. Returns error on invalid segments.
    pub fn parse(s: &str) -> Result<Self, UltnasCoreError>;

    /// The root namespace (empty path).
    pub fn root() -> Self;

    /// The number of path segments.
    pub fn depth(&self) -> usize;

    /// Returns the parent path, or `None` if already root.
    pub fn parent(&self) -> Option<NamespacePath>;

    /// Appends a segment to produce a child path.
    pub fn child(&self, segment: &str) -> Result<NamespacePath, UltnasCoreError>;

    /// Full path as a `/`-separated string.
    pub fn as_str(&self) -> String;
}

impl NamespaceTree {
    /// Create an empty tree.
    pub fn new() -> Self;

    /// Insert a namespace, creating intermediate nodes as needed.
    pub fn insert(&mut self, path: &NamespacePath) -> Result<(), UltnasCoreError>;

    /// Check whether a namespace exists.
    pub fn contains(&self, path: &NamespacePath) -> bool;

    /// List immediate children of a path.
    pub fn children(&self, path: &NamespacePath) -> Vec<NamespacePath>;

    /// List all namespaces that are descendants of `path`.
    pub fn descendants(&self, path: &NamespacePath) -> Vec<NamespacePath>;
}
```

### Validation Rules
- Segments: `[a-zA-Z0-9_-]+`, 1–128 characters each.
- Max depth: 32 segments.
- Reserved: `__ultnas__` at any level.
- Empty path is the root namespace and is always valid.

---

## 3. `record` Module

**Purpose:** Record lifecycle — creation, sealing, verification, and deserialization.

### Public Types

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub id:          ContentId,
    pub created_at:  chrono::DateTime<chrono::Utc>,
    pub namespace:   NamespacePath,
    pub label:       String,
    pub tags:        Vec<String>,
    pub media_type:  String,
    pub size_bytes:  u64,
    pub seal:        Option<RecordSeal>,
    pub metadata:    std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordSeal {
    pub sealed_at:   chrono::DateTime<chrono::Utc>,
    pub sealed_by:   ed25519_dalek::VerifyingKey,
    pub signature:   ed25519_dalek::Signature,
    pub policy_hash: ContentId,
}

/// Builder for constructing a Record before writing to the vault.
pub struct RecordBuilder { /* private */ }
```

### Public Functions & Methods

```rust
impl RecordBuilder {
    pub fn new(namespace: NamespacePath, label: impl Into<String>) -> Self;
    pub fn tag(self, tag: impl Into<String>) -> Self;
    pub fn media_type(self, mt: impl Into<String>) -> Self;
    pub fn metadata(self, key: impl Into<String>, value: impl Into<String>) -> Self;
    /// Finalise the record. Hashes content and assigns ContentId.
    pub fn build(self, content: &[u8]) -> Result<Record, UltnasCoreError>;
}

impl Record {
    /// Returns `true` if this record has been sealed.
    pub fn is_sealed(&self) -> bool;

    /// Seal this record with the given signing key and policy hash.
    /// Returns error if already sealed.
    pub fn seal(
        &mut self,
        signing_key: &ed25519_dalek::SigningKey,
        policy_hash: ContentId,
    ) -> Result<(), UltnasCoreError>;

    /// Verify the seal signature. Returns error if not sealed or sig invalid.
    pub fn verify_seal(&self) -> Result<(), UltnasCoreError>;

    /// Verify that `content` matches this record's ContentId.
    pub fn verify_content(&self, content: &[u8]) -> Result<(), UltnasCoreError>;

    /// Canonical byte serialization used for hashing and signing.
    pub fn canonical_bytes(&self) -> Vec<u8>;
}
```

---

## 4. `policy` Module

**Purpose:** TOML policy parsing, validation, and evaluation.

### Public Types

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    pub version:    u32,
    pub namespaces: Vec<NamespacePolicy>,
    pub global:     GlobalPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalPolicy {
    pub conflict:    ConflictStrategy,
    pub max_record_size_bytes: Option<u64>,
    pub require_seal_before_rotation: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespacePolicy {
    pub path:       String,
    pub conflict:   Option<ConflictStrategy>,
    pub retention:  Option<RetentionPolicy>,
    pub tags_required: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConflictStrategy { Reject, Version, Replace }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionPolicy {
    pub keep_versions: Option<u32>,
    pub keep_days:     Option<u32>,
}

pub struct PolicyEvaluator { /* private */ }
```

### Public Functions

```rust
impl Policy {
    /// Parse from a TOML string. Returns validation errors.
    pub fn from_toml(s: &str) -> Result<Self, UltnasCoreError>;

    /// Validate the policy without evaluating it against a record.
    pub fn validate(&self) -> Result<(), UltnasCoreError>;
}

impl PolicyEvaluator {
    pub fn new(policy: Policy) -> Self;

    /// Evaluate the policy against an incoming record. Returns Ok or a policy violation error.
    pub fn evaluate_ingest(&self, record: &Record) -> Result<(), UltnasCoreError>;

    /// Determine which records in `candidates` should be purged under retention policy.
    pub fn evaluate_retention(
        &self,
        namespace: &NamespacePath,
        candidates: &[Record],
    ) -> Vec<ContentId>;
}
```

---

## 5. `vault` Module

**Purpose:** On-disk vault layout; atomic read/write primitives for objects, records, seals, and the namespace tree.

### Public Types

```rust
pub struct Vault { /* private */ }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultManifest {
    pub name:        String,
    pub created_at:  chrono::DateTime<chrono::Utc>,
    pub version:     u32,
    pub policy_path: Option<PathBuf>,
}
```

### Public Functions

```rust
impl Vault {
    /// Initialize a new vault at `root`. Fails if `root` is already a vault.
    pub fn init(root: &Path, name: &str) -> Result<Self, UltnasCoreError>;

    /// Open an existing vault at `root`. Fails if not a valid vault.
    pub fn open(root: &Path) -> Result<Self, UltnasCoreError>;

    /// Write `content` bytes and store the record metadata atomically.
    pub fn write_record(&self, record: &Record, content: &[u8]) -> Result<(), UltnasCoreError>;

    /// Read the content bytes for a record by ContentId.
    pub fn read_content(&self, id: &ContentId) -> Result<Vec<u8>, UltnasCoreError>;

    /// Fetch a record's metadata by ContentId.
    pub fn get_record(&self, id: &ContentId) -> Result<Record, UltnasCoreError>;

    /// List all records in a namespace (non-recursive).
    pub fn list_records(&self, namespace: &NamespacePath) -> Result<Vec<Record>, UltnasCoreError>;

    /// Verify a record's on-disk content matches its ContentId.
    pub fn verify(&self, id: &ContentId) -> Result<(), UltnasCoreError>;

    /// Verify all records in the vault.
    pub fn verify_all(&self) -> Result<Vec<ContentId>, UltnasCoreError>;

    /// Returns the vault's root path.
    pub fn root(&self) -> &Path;
}
```

---

## 6. `journal` Module

**Purpose:** Append-only structured operation journal. Every mutation to the vault is recorded.

### Entry Format (newline-delimited JSON)

```json
{
  "ts":    "2026-09-25T10:39:00Z",
  "op":    "WRITE_RECORD",
  "id":    "abcdef1234...",
  "ns":    "projects/ultnas/docs",
  "label": "architecture-doc",
  "size":  42891
}
```

### Operation Types

| `op` | Trigger |
|---|---|
| `VAULT_INIT` | `Vault::init` |
| `WRITE_RECORD` | `Vault::write_record` |
| `SEAL_RECORD` | `Record::seal` |
| `VERIFY_RECORD` | `Vault::verify` |
| `PURGE_RECORD` | Retention policy enforcement |
| `METADATA_UPDATE` | Mutable metadata mutation |
| `POLICY_LOAD` | Policy file evaluated |

### Public API

```rust
pub struct Journal { /* private */ }

impl Journal {
    pub fn open(path: &Path) -> Result<Self, UltnasCoreError>;
    pub fn write(&self, entry: JournalEntry) -> Result<(), UltnasCoreError>;
    pub fn iter(&self) -> Result<impl Iterator<Item = Result<JournalEntry, UltnasCoreError>>, UltnasCoreError>;
}
```

---

## 7. `error` Module

**Purpose:** Unified error hierarchy for all `ultnas-core` operations.

```rust
#[derive(Debug, thiserror::Error)]
pub enum UltnasCoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid namespace path: {reason}")]
    InvalidNamespace { reason: String },

    #[error("invalid content ID: {0}")]
    InvalidContentId(String),

    #[error("content integrity check failed: expected {expected}, got {actual}")]
    IntegrityFailure { expected: String, actual: String },

    #[error("record is already sealed")]
    AlreadySealed,

    #[error("record is not sealed")]
    NotSealed,

    #[error("seal verification failed: {0}")]
    SealVerificationFailed(String),

    #[error("policy violation: {rule} — {detail}")]
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
}
```
