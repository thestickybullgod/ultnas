# Ultnas — API Reference

> Crate: `ultnas-core` v0.1.0 | Edition: 2021 | Stability: Unstable

This document is the complete public API reference for `ultnas-core`. All items listed here are `pub` and available to downstream crates. Items marked ⚠️ are subject to change before v0.5.0.

---

## Modules

| Module | Re-exported at crate root? | Description |
|---|---|---|
| `ultnas_core::address` | Yes | Content addressing (BLAKE3) |
| `ultnas_core::namespace` | Yes | Namespace tree and path types |
| `ultnas_core::record` | Yes | Record lifecycle |
| `ultnas_core::policy` | Yes | Policy parsing and evaluation |
| `ultnas_core::vault` | Yes | On-disk vault operations |
| `ultnas_core::journal` | No | Internal journal (use via `Vault`) |
| `ultnas_core::error` | Yes | Error types |

---

## `address` — Content Addressing

### `ContentId`

```rust
pub struct ContentId(blake3::Hash);
```

A 32-byte BLAKE3 content digest. The canonical identifier for any record or object in the vault.

**Implements:** `Debug`, `Clone`, `Copy`, `PartialEq`, `Eq`, `Hash`, `Serialize`, `Deserialize`, `Display`

```rust
// Display format: lowercase hex, 64 characters
println!("{}", content_id); // e.g. "a3b4c5..."
```

**Methods:**

```rust
impl ContentId {
    /// Hex string (64 lowercase chars).
    pub fn to_hex(&self) -> String;

    /// Parse from hex string. Returns Err on invalid input.
    pub fn from_hex(s: &str) -> Result<Self, UltnasCoreError>;

    /// Raw 32-byte array reference.
    pub fn as_bytes(&self) -> &[u8; 32];
}
```

### Free Functions

```rust
/// Hash a byte slice. Panics only on OOM.
pub fn hash_bytes(data: &[u8]) -> ContentId;

/// Hash a file by streaming. Efficient for large files.
pub fn hash_file(path: &Path) -> Result<ContentId, UltnasCoreError>;

/// Hash any `Read` implementor.
pub fn hash_reader<R: Read>(reader: &mut R) -> Result<ContentId, UltnasCoreError>;
```

---

## `namespace` — Namespace Management

### `NamespacePath`

```rust
pub struct NamespacePath(Vec<String>);
```

A validated, hierarchical namespace path.

**Implements:** `Debug`, `Clone`, `PartialEq`, `Eq`, `Hash`, `Serialize`, `Deserialize`, `Display`

```rust
// Display format: slash-joined segments
println!("{}", path); // e.g. "projects/ultnas/docs"
```

**Methods:**

```rust
impl NamespacePath {
    /// Parse a `/`-delimited path string.
    pub fn parse(s: &str) -> Result<Self, UltnasCoreError>;

    /// The root namespace (zero-length path).
    pub fn root() -> Self;

    /// Number of path segments.
    pub fn depth(&self) -> usize;

    /// Returns true if this is the root namespace.
    pub fn is_root(&self) -> bool;

    /// Parent path, or None if already root.
    pub fn parent(&self) -> Option<NamespacePath>;

    /// Append a validated segment.
    pub fn child(&self, segment: &str) -> Result<NamespacePath, UltnasCoreError>;

    /// `/`-joined string representation.
    pub fn as_str(&self) -> String;

    /// Iterator over path segments.
    pub fn segments(&self) -> impl Iterator<Item = &str>;
}
```

### `NamespaceTree`

```rust
pub struct NamespaceTree { /* private */ }
```

An in-memory B-tree of namespace paths.

```rust
impl NamespaceTree {
    pub fn new() -> Self;
    pub fn insert(&mut self, path: &NamespacePath) -> Result<(), UltnasCoreError>;
    pub fn contains(&self, path: &NamespacePath) -> bool;
    pub fn children(&self, path: &NamespacePath) -> Vec<NamespacePath>;
    pub fn descendants(&self, path: &NamespacePath) -> Vec<NamespacePath>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
```

---

## `record` — Record Lifecycle

### `Record`

```rust
pub struct Record { /* see MODULE_SPECS.md for fields */ }
```

**Methods:**

```rust
impl Record {
    pub fn is_sealed(&self) -> bool;

    pub fn seal(
        &mut self,
        signing_key: &ed25519_dalek::SigningKey,
        policy_hash: ContentId,
    ) -> Result<(), UltnasCoreError>;

    pub fn verify_seal(&self) -> Result<(), UltnasCoreError>;
    pub fn verify_content(&self, content: &[u8]) -> Result<(), UltnasCoreError>;
    pub fn canonical_bytes(&self) -> Vec<u8>;
}
```

### `RecordBuilder`

```rust
pub struct RecordBuilder { /* private */ }

impl RecordBuilder {
    pub fn new(namespace: NamespacePath, label: impl Into<String>) -> Self;
    pub fn tag(self, tag: impl Into<String>) -> Self;
    pub fn media_type(self, mt: impl Into<String>) -> Self;
    pub fn metadata(self, key: impl Into<String>, value: impl Into<String>) -> Self;
    pub fn build(self, content: &[u8]) -> Result<Record, UltnasCoreError>;
}
```

**Example:**

```rust
use ultnas_core::{NamespacePath, RecordBuilder};

let ns = NamespacePath::parse("projects/ultnas/docs")?;
let record = RecordBuilder::new(ns, "architecture-doc")
    .tag("architecture")
    .tag("design")
    .media_type("application/pdf")
    .metadata("author", "Dustin Wayne Deen")
    .build(&pdf_bytes)?;
```

---

## `policy` — Policy Engine

### `Policy`

```rust
pub struct Policy { /* see MODULE_SPECS.md for fields */ }

impl Policy {
    pub fn from_toml(s: &str) -> Result<Self, UltnasCoreError>;
    pub fn validate(&self) -> Result<(), UltnasCoreError>;
    pub fn content_id(&self) -> ContentId; // BLAKE3 of canonical TOML bytes
}
```

### `PolicyEvaluator`

```rust
pub struct PolicyEvaluator { /* private */ }

impl PolicyEvaluator {
    pub fn new(policy: Policy) -> Self;
    pub fn evaluate_ingest(&self, record: &Record) -> Result<(), UltnasCoreError>;
    pub fn evaluate_retention(
        &self,
        namespace: &NamespacePath,
        candidates: &[Record],
    ) -> Vec<ContentId>;
}
```

---

## `vault` — Vault Operations

### `Vault`

```rust
pub struct Vault { /* private */ }

impl Vault {
    pub fn init(root: &Path, name: &str) -> Result<Self, UltnasCoreError>;
    pub fn open(root: &Path) -> Result<Self, UltnasCoreError>;

    pub fn write_record(&self, record: &Record, content: &[u8]) -> Result<(), UltnasCoreError>;
    pub fn read_content(&self, id: &ContentId) -> Result<Vec<u8>, UltnasCoreError>;
    pub fn get_record(&self, id: &ContentId) -> Result<Record, UltnasCoreError>;
    pub fn list_records(&self, namespace: &NamespacePath) -> Result<Vec<Record>, UltnasCoreError>;

    pub fn verify(&self, id: &ContentId) -> Result<(), UltnasCoreError>;
    pub fn verify_all(&self) -> Result<Vec<ContentId>, UltnasCoreError>;

    pub fn root(&self) -> &Path;
    pub fn manifest(&self) -> &VaultManifest;
}
```

**Example:**

```rust
use ultnas_core::{Vault, NamespacePath, RecordBuilder};

let vault = Vault::init(Path::new("./my-vault"), "personal-archive")?;
let ns = NamespacePath::parse("documents/2026")?;
let record = RecordBuilder::new(ns, "tax-return")
    .tag("finance")
    .media_type("application/pdf")
    .build(&file_bytes)?;

vault.write_record(&record, &file_bytes)?;
vault.verify(&record.id)?;
```

---

## `error` — Error Types

### `UltnasCoreError`

See `MODULE_SPECS.md` § 7 for the full variant list. Key variants:

| Variant | Cause |
|---|---|
| `IntegrityFailure` | BLAKE3 hash mismatch on read |
| `AlreadySealed` | Attempt to seal an already-sealed record |
| `SealVerificationFailed` | Ed25519 signature invalid |
| `PolicyViolation` | Record rejected by active policy |
| `VaultLocked` | Another process holds the vault lock |
| `RecordNotFound` | No record with the given ContentId |
