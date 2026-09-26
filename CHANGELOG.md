# Changelog

All notable changes to Ultnas will be documented here.

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
Versioning: [Semantic Versioning](https://semver.org/spec/v2.0.0.html)

---

## [Unreleased]

### Added
- Signed APT repository at https://thestickybullgod.github.io/ultnas (key `0AB5 B2EA 07F2 0A83 504D 9ED0 3F9D D92C BF9B 17E8`), rebuilt from every release's `.deb` and verified by installing from the live address. Each release now updates it

## [0.1.0] — 2026-09-25

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
- `ultnas track --recursive <dir>` tracks every text file under a directory, and the daemon adopts files created there later (stripping any invisible characters first). Hidden names, editor scratch files, `--exclude` names, binary files, and files over 16 MiB are skipped. Deleting a file tracked this way is accepted in `automatic` mode and restored in `approved` mode; `untrack` works on directories and keeps an untracked file from being re-adopted
- `ultnas setup`: an interactive checklist of recommended places to protect (shell startup files, SSH and Git config, script and source directories, `~/.config`, and `/etc` as root), showing only what exists, each group in its own namespace; `--list` and `--yes` for scripting
- Sealed records are watched too: events under the vault's `objects/` and `records/` name the content id they concern, so a tampered sealed object is verified (and, at the threshold, restored) at once instead of at the next full scan
- IPC between CLI and daemon: newline-delimited JSON over `<vault>/.ultnas.sock` (mode 0600), or a local-only named pipe on Windows. `ultnas daemon status` shows live state (journal health, dropped alerts, quarantines, watcher mode and watch counts, tracked counts, last full scan, cache use, recent alerts; `--json` for raw), and `ultnas daemon stop` shuts the daemon down cleanly
- Mirror: `mirror = "<dir>"` under `[global.integrity]` (or `ultnasd --mirror`) keeps a second, hash-checked copy of every sealed object and tracked-file version. Full scans fill and repair it; restores fall back to it last, so a damaged sealed record that isn't cached is repaired instead of quarantined. The daemon warns if the mirror shares the vault's filesystem
- Retention: the daemon's scheduler (hourly) and `ultnas purge --rotate [--dry-run]` apply `keep_days` and `keep_versions`, per series (one tracked file's history, or one label), journaling each purge first and removing it from the mirror too. A tracked file's current versions are never purged. `ultnas purge <id>` removes one record, refusing a current version
- Journal rotation: past `journal_max_bytes` (default 64 MiB) the daemon rotates `journal.log` to `journal.log.1` …, keeping `journal_keep` (default 5). The new file starts with a quarantine checkpoint, so quarantines, lifts, and the stale-escalation rule carry over exactly. CLI writes take a short lock on `journal.log.lock` and reopen the file each time, so none land in a just-archived file
- `ultnas completions <shell>` prints a completion script (bash, zsh, fish, elvish, PowerShell); hidden `ultnas manpage` and `ultnasd --manpage` print man pages for packaging
- Debian package: `.github/workflows/deb.yml` builds `ultnas_<version>-1_amd64.deb` on Ubuntu 22.04 (manually, and for version tags), inspects it, installs it, smoke-tests a real attack and repair, removes it, and uploads it. It installs both binaries, systemd user and system services (not enabled), man pages, bash/zsh/fish completions, docs, and example policies. Vaults are never removed, even on purge
- `TESTING_LINUX.md`: a step-by-step guide to testing the package
- `track -r` (and `setup`) show a live progress counter while reading and tracking a large directory

### Changed
- Bulk tracking (`track -r`, `setup`) flushes to disk once at the end instead of several times per file (`with_deferred_sync`), which dominated the time for directories like `/etc`
- At the setup checklist, `y` means go ahead, like Enter
- The vault defaults to `$ULTNAS_VAULT`, else `~/.local/share/ultnas` (`$XDG_DATA_HOME/ultnas`), `/var/lib/ultnas` as root, or `%LOCALAPPDATA%\ultnas` on Windows, instead of the current directory, for both `ultnas` and `ultnasd`. `ultnas setup` creates the vault if it doesn't exist, and a missing vault says how to make one
- A write that arrived with invisible characters and also changed visible text is held as pending, even in `automatic` mode, so a clean-looking attacker edit can't slip in with it
- `ultnasd` loads its policy (`--policy`, else the manifest's `policy_path`, else defaults); integrity flags now override the policy instead of ignoring it
- `restore_source` is honored for tracked files; `"remote"` falls back to `"memory_then_store"`
- IntegrityGuard is now synchronous and runs, with the whole watcher scan, inside `spawn_blocking`
- `VerifiedCache` is shared via `std::sync::Mutex`, is true LRU, stores `Arc<[u8]>`, and re-hashes on insert
- Quarantine state is owned by IntegrityGuard and synced from the journal every watcher scan; CLI lifts reset violation counts
- `atomic_write` fsyncs the file (and parent directory on Unix) before/after rename
- `ultnas integrity status` / `lift-quarantine` use the same quarantine fold as the daemon; lift validates the namespace

### Security
- Kernel pseudo-filesystems (`/proc`, `/sys`, `/dev`, `/run`, and on Linux any proc/sysfs/cgroup/debugfs/… mount, found by filesystem type) can't be tracked: repairing a file there would write a kernel setting. Tracked directories never cross into another filesystem when walking, watching, or adopting
- Tracking `/`, a directory outside your home, or a very large directory previews the file count and size and asks first (`--yes` to skip); without a terminal it refuses
- Tracked directories are watched one directory at a time, skipping hidden, excluded, and ignored subtrees, instead of with a recursive OS watch that spent watches on `node_modules` and would descend into other filesystems; watch failures (e.g. inotify's limit) are reported
- Tracked files are opened without following symbolic links, and without blocking on FIFOs; a link or other non-regular file at a tracked path is a violation, replaced by a regular file without touching the link's target or copying its permissions
- `atomic_write` never opens an existing temp file, so a link planted at the predictable temp name can't redirect a write
- Replacing a live file checks it is unchanged just before the rename, so an edit made while the daemon was inspecting it isn't overwritten

### Fixed
- While a namespace was quarantined, every full scan re-reported the same unchanged file, adding a log line and journal entry every 5 minutes and bumping its count. An unchanged observation is now reported once
- Every event was logged twice: once in words, and again as an `IntegrityAlert { … }` dump. The dump is now debug-level
- A write made while the daemon was starting could go unrepaired until the next full scan (up to 5 minutes): watches were registered only after the first full scan. They are now registered first
- A quarantine lift issued while the daemon's journal was failing could be undone: the buffered escalation was written after the lift and "last entry wins" re-quarantined the namespace. An escalation older than a lift already applied is now ignored, by the daemon and the CLI alike
- A violation within `debounce_ms` of the previous one was ignored entirely instead of only sharing its count, so a rapid burst of writes went unrepaired
- Restore could rewrite a tampered object onto itself and report success (removed the object-store "L2" tier)
- Cache warm-up admitted unverified bytes as restore sources
- First violation for a record was always debounced away; the rolling violation window never reset
- Escalated records re-escalated on every scan, and lifts were immediately undone
- Journal write failures were silently discarded; a poisoned journal lock disabled the journal permanently
- CLI used `chrono` without declaring the dependency
- A second daemon silently overwrote `.ultnas-lock` and ran against the same vault; the lock is now an OS file lock (MSRV raised to 1.89 for `File::try_lock`)
- CLI panicked on every invocation: `--vault` and `--verbose` both claimed `-v`; `-v` is now `--verbose` only, matching `ultnasd`
