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

### Changed
- IntegrityGuard is now synchronous and runs, with the whole watcher scan, inside `spawn_blocking`
- `VerifiedCache` is shared via `std::sync::Mutex`, is true LRU, stores `Arc<[u8]>`, and re-hashes on insert
- Quarantine state is owned by IntegrityGuard and synced from the journal every watcher scan; CLI lifts reset violation counts
- `atomic_write` fsyncs the file (and parent directory on Unix) before/after rename
- `ultnas integrity status` / `lift-quarantine` use the same quarantine fold as the daemon; lift validates the namespace

### Fixed
- Restore could rewrite a tampered object onto itself and report success (removed the object-store "L2" tier)
- Cache warm-up admitted unverified bytes as restore sources
- First violation for a record was always debounced away; the rolling violation window never reset
- Escalated records re-escalated on every scan, and lifts were immediately undone
- Journal write failures were silently discarded; a poisoned journal lock disabled the journal permanently
- CLI used `chrono` without declaring the dependency
- A second daemon silently overwrote `.ultnas-lock` and ran against the same vault; the lock is now an OS file lock (MSRV raised to 1.89 for `File::try_lock`)
