# Changelog

All notable changes to Ultnas will be documented here.

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
Versioning: [Semantic Versioning](https://semver.org/spec/v2.0.0.html)

---

## [Unreleased]

### Added
- Initial workspace structure with `ultnas-core`, `ultnas-cli`, and `ultnas-daemon` crates
- BLAKE3 content addressing (`address` module)
- Hierarchical namespace management (`namespace` module)
- Record creation, sealing, and content verification (`record` module)
- TOML policy parsing and evaluation engine (`policy` module)
- Atomic vault read/write with `objects/`, `records/`, and `seals/` layout (`vault` module)
- Append-only structured operation journal (`journal` module)
- CLI commands: `init`, `add`, `inspect`, `verify`, `ls`, `policy validate`
- Daemon skeleton with `WatcherService`, `Scheduler`, `PolicyEnforcer`, and `IpcServer` stubs
- Full documentation suite: ARCHITECTURE, MODULE_SPECS, POLICY_GUIDE, API_REFERENCE, ONBOARDING, SECURITY
- Example policy files: `default.toml`, `compliance.toml`, `personal.toml`
- CI workflow (format, clippy, test, security audit, coverage)
- Release workflow (cross-platform binary builds)
- `Vault::restore_object` (hash-checked), `Vault::object_path`, `Vault::all_records`
- `Journal::read_from` (incremental, torn-line safe) and `Journal::quarantined_namespaces`
- Journal ops `IntegrityRestoreIntent` (write-ahead) and `IntegrityRestoreFailed`
- IntegrityGuard degraded mode: buffers journal entries and suspends auto-restore while the journal is unwritable

- Tracked files (`ultnas track`, `untrack`, `approve`, `tracked`): live text files protected in place against invisible-character writes. Each write below `write_violation_threshold` is prevented by stripping the characters it added; at the threshold the file is deleted and recreated from its stable version (memory, then the vault's copy); repeated restores quarantine the namespace
- `invisible` module: detects zero-width, bidi, tag, filler, and other format characters, with context rules for legitimate non-ASCII uses (emoji ZWJ, ZWNJ, variation selectors, subdivision flags)
- Policy `approval = "automatic" | "approved"` under `[global.integrity]`, overridable per namespace: clean edits become the stable version immediately, or stay pending until `ultnas approve`
- Journal ops `TrackFile`, `UntrackFile`, `VersionAccepted`, `VersionPending`, `VersionApproved`, `IntegritySanitize`; entries carry an optional `path`
- `ultnasd` logs to a daily-rotated file, `<vault>/logs/ultnasd.<date>.log` by default, as well as stdout; `--log-dir`, `--log-keep-days` (default 14), and `--no-log-file` control it. The file writer is lossless
- `WatcherService` reacts to file-system events (inotify / FSEvents / ReadDirectoryChangesW, via `notify`): each write to a tracked file is inspected within milliseconds and counts as one attempt, and CLI `track` / `untrack` / `lift-quarantine` take effect immediately. A full scan every `--scan-interval-secs` (default 300) remains as a backstop and also runs whenever the OS drops events; if events are unavailable, the daemon polls every 30 s

### Changed
- A write that arrived with invisible characters and also changed visible text is held as pending, even in `automatic` mode, so a clean-looking attacker edit can't slip in with it
- `ultnasd` loads its policy (`--policy`, else the manifest's `policy_path`, else defaults); integrity flags now override the policy instead of ignoring it
- `restore_source` is honored for tracked files; `"remote"` falls back to `"memory_then_store"`
- IntegrityGuard is now synchronous and runs, with the whole watcher scan, inside `spawn_blocking`
- `VerifiedCache` is shared via `std::sync::Mutex`, is true LRU, stores `Arc<[u8]>`, and re-hashes on insert
- Quarantine state is owned by IntegrityGuard and synced from the journal every watcher scan; CLI lifts reset violation counts
- `atomic_write` fsyncs the file (and parent directory on Unix) before/after rename
- `ultnas integrity status` / `lift-quarantine` use the same quarantine fold as the daemon; lift validates the namespace

### Security
- Tracked files are opened without following symbolic links, and without blocking on FIFOs; a link or other non-regular file at a tracked path is a violation, replaced by a regular file without touching the link's target or copying its permissions
- `atomic_write` never opens an existing temp file, so a link planted at the predictable temp name can't redirect a write
- Replacing a live file checks it is unchanged just before the rename, so an edit made while the daemon was inspecting it isn't overwritten

### Fixed
- A violation within `debounce_ms` of the previous one was ignored entirely instead of only sharing its count, so a rapid burst of writes went unrepaired
- Restore could rewrite a tampered object onto itself and report success (removed the object-store "L2" tier)
- Cache warm-up admitted unverified bytes as restore sources
- First violation for a record was always debounced away; the rolling violation window never reset
- Escalated records re-escalated on every scan, and lifts were immediately undone
- Journal write failures were silently discarded; a poisoned journal lock disabled the journal permanently
- CLI used `chrono` without declaring the dependency
- A second daemon silently overwrote `.ultnas-lock` and ran against the same vault; the lock is now an OS file lock (MSRV raised to 1.89 for `File::try_lock`)
- CLI panicked on every invocation: `--vault` and `--verbose` both claimed `-v`; `-v` is now `--verbose` only, matching `ultnasd`
