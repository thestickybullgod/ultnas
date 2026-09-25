# Ultnas — System Architecture

> Version: 0.1 | Status: Pre-Alpha | Last Updated: 2026-09-25

## Overview

Ultnas is a three-tier system: a **core library**, a **CLI**, and a **background daemon**. All three share a common on-disk vault format.

```
[ User / Scripts ]
       │  CLI invocations
       ▼
[ ultnas-cli ]   Commands: init · add · inspect · verify · ls · export · purge
       │  library calls
       ▼
[ ultnas-core ]  Modules: address · namespace · record · policy · vault · journal · error
       │  direct file I/O          │  IPC socket
       ▼                           ▼
[ On-disk Vault ]          [ ultnas-daemon ]
  content-addressed          Services: watcher · scheduler · policy-enforcer · syncer · ipc-server
  sealed records
```

---

## Design Principles

| Principle | Implementation |
|---|---|
| Offline-first | All core operations work with zero network access |
| No hidden state | Every decision is logged to a structured append-only journal |
| Crash safety | Atomic `write → fsync → rename` for every vault write |
| Content-addressed | Records identified by BLAKE3 hash, not name or path |
| Policy as code | Retention, rotation, access rules live in TOML files |
| Minimal TCB | `ultnas-core` has no async, no network, no platform-specific code |

---

## Component Map

### ultnas-core Modules

| Module | Responsibility |
|---|---|
| `address` | BLAKE3 content hashing; `ContentId` type |
| `namespace` | Hierarchical namespace tree; resolution and conflict detection |
| `record` | Record creation, sealing, and verification |
| `policy` | TOML policy parsing, validation, and evaluation engine |
| `vault` | On-disk vault layout; atomic read/write primitives |
| `invisible` | Invisible-character detection; classifying a write as clean or a violation |
| `tracking` | Tracked live files: stable and pending versions, locked read-modify-write |
| `journal` | Append-only structured operation journal |
| `error` | Unified `UltnasCoreError` type hierarchy |

### ultnas-cli Commands

| Command | Description |
|---|---|
| `init` | Initialize a new vault |
| `add` | Archive a file or directory |
| `inspect` | Display record metadata, hash, and seal status |
| `verify` | Verify content integrity against stored hashes |
| `ls` | List records with namespace and tag filtering |
| `export` | Export records to an external format |
| `purge` | Remove records per policy or explicit ID |
| `policy validate` | Validate a policy TOML file |
| `track` / `untrack` | Protect a live text file in place, or stop |
| `approve` | Promote a tracked file's pending edit to its stable version |
| `tracked` | List tracked files and their status |
| `daemon status` | Query the running daemon over IPC |

### ultnas-daemon Services

| Service | Description |
|---|---|
| `WatcherService` | inotify/FSEvents-based automatic ingestion |
| `SchedulerService` | Cron-style rotation and cleanup scheduling |
| `PolicyEnforcer` | Periodic policy compliance scans |
| `SyncService` | Optional remote vault synchronization (off by default) |
| `IpcServer` | Unix socket IPC server for CLI ↔ daemon communication |

---

## Data Model

```
Record {
    id:          ContentId                    // BLAKE3 hash of canonical content bytes
    created_at:  DateTime<Utc>
    namespace:   NamespacePath               // e.g. "projects/ultnas/docs"
    label:       String
    tags:        Vec<String>
    media_type:  String                      // MIME type
    size_bytes:  u64
    seal:        Option<RecordSeal>
    metadata:    BTreeMap<String, String>
}

RecordSeal {
    sealed_at:   DateTime<Utc>
    sealed_by:   PublicKey                   // Ed25519 public key
    signature:   Signature                   // Ed25519 sig over canonical record bytes
    policy_hash: ContentId                   // Hash of policy file at seal time
}
```

---

## Content Addressing

All records stored under their BLAKE3 content hash:
- Identical files produce the same `ContentId` → natural deduplication
- Tampering is immediately detectable
- Records are addressable without knowing the original filename

```
content_id = BLAKE3(canonical_content_bytes)
```

---

## Namespace Resolution

- Hierarchical `/`-separated paths, case-sensitive
- Segments must match `[a-zA-Z0-9_-]+`
- `__ultnas__` is reserved
- Missing intermediate segments created on demand
- Stored as a flat B-tree keyed by full path for O(log n) lookup
- Conflict policy per-namespace: `reject` | `version` | `replace`

---

## Record Sealing

1. Caller provides an Ed25519 signing key
2. `ultnas-core` serializes the record into canonical bytes
3. A `RecordSeal` is created with timestamp, public key, and Ed25519 signature
4. Policy file hash at seal time is embedded in the seal
5. Seal appended to the record atomically

Once sealed, record content is immutable. Metadata mutations are journaled.

---

## Storage Layout

```
<vault-root>/
├── vault.toml          # Vault manifest
├── objects/            # Content-addressed object store
│   └── ab/             # First 2 hex chars of ContentId
│       └── abcdef...   # Full ContentId filename; raw content bytes
├── records/            # Record metadata JSON (one file per record)
│   └── <ContentId>.json
├── namespace.db        # Namespace B-tree (mmap'd binary format)
├── journal.log         # Append-only operation journal (newline-delimited JSON)
├── seals/              # Detached seal files
│   └── <ContentId>.seal
├── tracked/            # Tracked live files (stable + pending version ids)
│   ├── .lock           # Held briefly for every read-modify-write
│   └── <hash of path>.json
└── .ultnas-lock        # OS-locked while a daemon owns the vault (holds owner PID)
```

---

## IPC Protocol

Newline-delimited JSON over a Unix socket at `<vault-root>/.ultnas.sock`:

```json
// Request
{ "id": "uuid-v4", "command": "status", "params": {} }

// Success response
{ "id": "uuid-v4", "ok": true, "data": { "uptime_secs": 3600 } }

// Error response
{ "id": "uuid-v4", "ok": false, "error": { "code": "VAULT_LOCKED", "message": "..." } }
```

---

## Security Boundaries

| Boundary | Mechanism |
|---|---|
| Process isolation | CLI and daemon are separate OS processes |
| Vault lock | OS file lock on `.ultnas-lock` (flock / LockFileEx) allows one daemon per vault |
| Record integrity | BLAKE3 hash verification on every read |
| Seal authenticity | Ed25519 signature verification |
| Policy integrity | Policy file hash embedded in every seal |
| IPC authentication | Socket file permissions `0600`, owned by vault owner |

---

## Future Directions

- Remote vault sync (encrypted S3-compatible / WebDAV replication)
- M-of-N Ed25519 multisig seals
- Redaction proofs without invalidating seals
- WASM policy plugins (user-defined, sandboxed)
- Tauri-based desktop GUI wrapping the daemon IPC
