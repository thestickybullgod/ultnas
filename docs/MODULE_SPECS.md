# Ultnas — Module Specifications

What each module is responsible for, and the invariants it keeps. For exact
signatures, generate the API docs: `cargo doc --workspace --no-deps --open`.

---

## `ultnas-core` (library)

| Module | Responsibility | Invariants |
|---|---|---|
| `invisible` | Find invisible characters in text; compare a write against a baseline. `scan`, `introduced`, `strip_introduced`, `classify_change` (`Clean` / `Introduced` / `NotText`). | Context-sensitive characters (ZWJ, ZWNJ, variation selectors, LRM/RLM, tag characters) are flagged only with ASCII or the edge on both sides, looking past other invisibles. A leading BOM is allowed. `introduced` never reports an occurrence the baseline already had. |
| `tracking` | Tracked files (`TrackedFile`: path, namespace, stable and pending versions, source directory) and tracked directories (`TrackedDir`: excludes, ignored paths, device). Membership rules: `TrackedDir::covers`, `candidates`, `subdirs`; `check_trackable`. | Every state change is a read-modify-write under the OS lock on `tracked/.lock` (`update_tracked`, `update_tracked_dir`, `with_tracking_lock`). Directory walks never follow links or cross filesystems. Kernel pseudo-filesystems are untrackable. |
| `live` | Read and replace live files safely. `read_live` → `Live::{Missing, NotRegular, File}`; `rewrite_file`, `recreate_file`. | Never follows a final symbolic link, never blocks on a FIFO. A replacement takes the content id the caller observed and fails with `ChangedDuringWrite` if the file changed, checked again just before the rename. Permissions (and owner, on Unix) are carried over; a link's target's are not. |
| `vault` | The on-disk vault: manifest, content objects (`objects/ab/abcd…`), record metadata, `default_root`, `load_policy`, `purge_record`. | Writes are atomic: temp file created with `create_new` (never an existing name), fsync, rename, fsync the directory. A restore or version write only accepts bytes that hash to their id. |
| `journal` | The append-only NDJSON journal, `QuarantineFold` (the one definition of quarantine state), size-based rotation. | Only the daemon owns a journal (`Journal::open`); others use `open_shared`, which writes under the OS lock on `journal.log.lock`. An escalation older than a lift for its namespace is stale. A rotated file starts with a checkpoint that reproduces the quarantine state exactly. |
| `retention` | `plan` and `rotate`: purge per `keep_days` / `keep_versions`. | A tracked file's stable or pending version is never purged. Runs under the tracking lock. Each purge is journaled before anything is removed. |
| `mirror` | A second copy of content objects in another directory. | Every read is hash-checked; every write is atomic. |
| `ipc` | CLI ↔ daemon protocol (`Request`, `Response`, `DaemonStatus`), endpoint naming, a blocking client. | The endpoint is derived from the canonical vault path. Lines are capped at `MAX_LINE`. |
| `policy` | TOML policy: global and integrity settings, namespaces, approval, retention, mirror, journal rotation. `approval_for`, `retention_for`. | Namespace matching is segment-aware (`docs` covers `docs/x`, not `docsX`); the most specific entry wins. `Policy::default()` is a valid version-1 policy. |
| `address` | BLAKE3 `ContentId`. | Hex form is 64 lowercase characters. |
| `namespace` | `NamespacePath`: validated `[a-zA-Z0-9_-]+` segments, depth ≤ 32. | The empty path is the root namespace. |
| `record` | `Record`, `RecordBuilder`, `RecordSeal` for archived content. | A record's id is the hash of its content; a record can be sealed once. |
| `error` | `UltnasCoreError`. | — |

---

## `ultnas-daemon` (`ultnasd`)

| Module | Responsibility |
|---|---|
| `main` | Parse flags, open the vault, take the vault lock, load the policy, start the services, wait for Ctrl-C or IPC `stop`. |
| `services::vault_lock` | `.ultnas-lock` held with an OS file lock: one daemon per vault, released even on a crash. |
| `services::watcher` | File-system events plus a periodic full scan. Classifies tracked-file changes, adopts new files in tracked directories, verifies sealed records named by events, fills the mirror. Watches are registered before the first full scan. |
| `services::integrity_guard` | Decides what happens to a violation: count (debounced), strip below the threshold, delete-and-recreate at it, quarantine after repeated restores. Versions clean edits per the approval mode. Journal-first, with a bounded buffer and repairs suspended while the journal fails. |
| `services::verified_cache` | LRU-bounded in-memory copies of stable versions, admitted only if they hash to their id. |
| `services::scheduler` | Hourly retention pass. |
| `services::ipc` | Serves `status` and `stop` on the vault's endpoint. |
| `services::policy_enforcer` | Periodically logs the quarantine state. |
| `logging` | stdout plus an optional daily-rotated log file. |

---

## `ultnas-cli` (`ultnas`)

One module per command group under `src/commands/`: `setup`, `track`
(track / untrack / approve / tracked), `daemon` (status / stop), `integrity`,
`purge`, `policy`, `prompt` (shared confirmation), and the archiving commands
`init`, `add`, `inspect`, `verify`, `ls`. End-to-end tests live in `tests/`.
