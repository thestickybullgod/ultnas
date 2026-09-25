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
| `purge` | Remove a record by id, or apply retention with `--rotate` (`--dry-run` to preview) |
| `policy validate` | Validate a policy TOML file |
| `setup` | Choose recommended files and directories to protect (shell startup, SSH, Git, scripts, source, `/etc` as root) |
| `track` / `untrack` | Protect a live text file (or, with `--recursive`, a directory) in place, or stop |
| `approve` | Promote a tracked file's pending edit to its stable version |
| `tracked` | List tracked files and their status |
| `daemon status` / `daemon stop` | Show the running daemon's live state (`--json` for raw), or stop it, over IPC |

### ultnas-daemon Services

| Service | Description |
|---|---|
| `WatcherService` | Checks tracked files on file-system events (inotify / FSEvents / ReadDirectoryChangesW), with a periodic full scan of tracked files and sealed objects as backstop |
| `Scheduler` | Hourly retention: purges records past `keep_days` and versions beyond `keep_versions`, never a tracked file's current version |
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
├── journal.log.1 …     # Rotated journal files (journal_keep of them)
├── seals/              # Detached seal files
│   └── <ContentId>.seal
├── logs/               # ultnasd.<date>.log, rotated daily (default: 14 kept)
├── tracked/            # Tracked live files (stable + pending version ids)
│   ├── .lock           # Held briefly for every read-modify-write
│   ├── <hash of path>.json   # a tracked file
│   └── <hash of path>.tdir   # a tracked directory
└── .ultnas-lock        # OS-locked while a daemon owns the vault (holds owner PID)
```

---

## IPC Protocol

Newline-delimited JSON over a Unix socket at `<vault-root>/.ultnas.sock`
(mode `0600`), or on Windows the named pipe `\\.\pipe\ultnas-<hash of vault path>`,
which refuses remote clients. The daemon and the CLI both derive the
endpoint from the canonical vault path. One request per line, one response
per line; lines are capped at 1 MiB.

```json
// Request
{ "id": "1234-1727300000000000000", "command": "status", "params": null }

// Success response
{ "id": "1234-1727300000000000000", "ok": true, "data": { "pid": 1234, "journal": { "degraded": false } } }

// Error response
{ "id": "1234-1727300000000000000", "ok": false, "error": { "code": "UNKNOWN_COMMAND", "message": "..." } }
```

| Command | Answer |
|---|---|
| `status` | `DaemonStatus`: PID, version, start time, journal health (degraded, buffered and lost entries), dropped alerts, quarantined namespaces, watcher mode and watch counts, tracked counts, last full scan, cache use, the 20 most recent alerts |
| `stop` | `{ "stopping": true }`, then the daemon exits and releases the vault lock |

The vault lock means only one daemon can own a vault's endpoint: it clears a
stale socket before binding, and on Windows claims the pipe as its first
instance, so a squatter makes it fail rather than share. If the socket path
would exceed the Unix limit (about 100 bytes), IPC is disabled with a warning.

---

## Security Boundaries

| Boundary | Mechanism |
|---|---|
| Process isolation | CLI and daemon are separate OS processes |
| Vault lock | OS file lock on `.ultnas-lock` (flock / LockFileEx) allows one daemon per vault |
| Record integrity | BLAKE3 hash verification on every read |
| Seal authenticity | Ed25519 signature verification |
| Policy integrity | Policy file hash embedded in every seal |
| IPC authentication | Unix: socket permissions `0600`, owned by the vault owner. Windows: named pipe, local clients only |

---

## Future Directions

- Remote vault sync (encrypted S3-compatible / WebDAV replication)
- M-of-N Ed25519 multisig seals
- Redaction proofs without invalidating seals
- WASM policy plugins (user-defined, sandboxed)
- Tauri-based desktop GUI wrapping the daemon IPC
