//! IntegrityGuard — write-violation detection, restore pipeline, and quarantine.
//!
//! ## Design overview
//!
//! ```text
//! WatcherService (spawn_blocking) ──► IntegrityGuard::record_violation(v)
//!                          │
//!                    debounce: events within N ms share one count
//!                    (each one is still repaired below)
//!                          │
//!                    namespace quarantined? ── YES ──► detect + journal only
//!                          │
//!                    count within rolling window >= threshold?
//!                          │
//!                          NO ──► tracked file: prevent this write by stripping
//!                          │      the invisible characters it introduced
//!                          │      (sealed object: count only)
//!                         YES
//!                          │
//!                    restore attempts >= escalate_after_restores?
//!                         YES ──► journal IntegrityEscalate, then quarantine
//!                          │
//!                    restore (delete and recreate):
//!                      journal IntegrityRestoreIntent (write-ahead)
//!                      verified copy: VerifiedCache, then the vault (tracked only)
//!                      recreate_file / Vault::restore_object (atomic + fsync)
//!                      journal IntegrityRestore / IntegrityRestoreFailed
//! ```
//!
//! ## Targets
//! A violation is either a sealed object in the vault's object store, or a
//! *tracked* live file (see `ultnas_core::tracking`) that gained invisible
//! characters, stopped being text, was deleted, or was replaced by something
//! other than a regular file (a symbolic link is never followed). Clean
//! edits of tracked files aren't violations;
//! [`IntegrityGuard::record_clean_edit`] versions them per the approval mode.
//!
//! A write that arrived with invisible characters is never trusted as an
//! edit: if stripping them leaves visible changes too, the result is held as
//! *pending* whatever the approval mode, so it needs `ultnas approve`.
//!
//! Every write to a live file passes the content id the guard inspected, and
//! is abandoned if the file changed since, so a concurrent edit isn't lost.
//!
//! ## Threading
//! Everything here is synchronous file I/O, so `IntegrityGuard` is a plain
//! sync struct shared as `Arc<std::sync::Mutex<_>>`. Only call into it from
//! `spawn_blocking` — never lock it on an async worker thread.
//!
//! ## Restore sources
//! For a tracked file, both the VerifiedCache and the vault's copy of the
//! stable version are independent of the live file, tried in the order
//! `restore_source` sets. For a sealed object, the object *is* the file being
//! restored, so only the cache can serve it.
//!
//! ## Journal failures (degraded mode)
//! If the journal can't be written, the guard keeps detecting and keeps
//! quarantining (fail closed) but stops auto-restoring, so it never changes
//! data it can't record. Unwritten entries are buffered (bounded) and retried
//! at the start of every `record_violation` / `sync_quarantine` call.
//!
//! ## QuarantineRegistry
//! Owned by the guard and folded incrementally from the journal (see
//! [`QuarantineRegistry::sync_from_journal`]) before every watcher check, so
//! a CLI-issued lift applies as soon as the watcher sees the journal change.

use chrono::{DateTime, Utc};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use ultnas_core::{
    apply_quarantine_entry, hash_bytes, invisible, read_live, recreate_file, rewrite_file,
    ApprovalMode, ContentId, Journal, JournalEntry, JournalOp, Live, QuarantineChange, TrackedDir,
    TrackedFile, UltnasCoreError, Vault, MAX_ADOPT_BYTES,
};

use super::verified_cache::SharedCache;

/// Upper bound on journal entries held in memory while the journal is unwritable.
const MAX_PENDING_JOURNAL: usize = 1024;

/// Lock a std mutex, recovering from poisoning. A panic in one scan must not
/// take the integrity pipeline down for the rest of the daemon's life.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ─── Alert types ────────────────────────────────────────────────────────────

/// Advisory notifications. The journal, not this channel, is the record of
/// what happened, so alerts are sent with `try_send` and may be dropped.
// Fields are only read through `Debug` by the alert logger until IPC
// subscribers land (v0.3); dead-code analysis ignores derived `Debug`.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum IntegrityAlert {
    ViolationDetected {
        path: PathBuf,
        count: u32,
    },
    RestoreSucceeded {
        path: PathBuf,
        source: RestoreSource,
    },
    RestoreFailed {
        path: PathBuf,
        reason: String,
    },
    Sanitized {
        path: PathBuf,
        removed: usize,
    },
    VersionAccepted {
        path: PathBuf,
    },
    VersionPending {
        path: PathBuf,
    },
    NamespaceQuarantined {
        namespace: String,
    },
    JournalDegraded {
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub enum RestoreSource {
    MemoryCache,
    VaultStore,
}

impl RestoreSource {
    fn as_str(&self) -> &'static str {
        match self {
            RestoreSource::MemoryCache => "memory_cache",
            RestoreSource::VaultStore => "vault_store",
        }
    }
}

/// Which verified copies a restore may use, in order (policy `restore_source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreOrder {
    Memory,
    Store,
    MemoryThenStore,
}

impl RestoreOrder {
    /// Parse a validated policy value. `"remote"` isn't implemented yet and
    /// behaves like the default.
    pub fn from_policy(source: &str) -> Self {
        match source {
            "memory" => RestoreOrder::Memory,
            "store" => RestoreOrder::Store,
            _ => RestoreOrder::MemoryThenStore,
        }
    }
}

/// What a violation is about.
#[derive(Debug, Clone)]
pub enum Target {
    /// A sealed object in the vault's object store.
    Object,
    /// A tracked live file. `baseline` is its newest clean version, which the
    /// write is compared against; [`Violation::id`] is the stable version a
    /// restore recreates. `observed` is the content id the watcher saw
    /// (`None`: missing or not a regular file); a restore only replaces that.
    Tracked {
        path: PathBuf,
        baseline: ContentId,
        observed: Option<ContentId>,
    },
}

/// Content that no longer matches the version it should be.
#[derive(Debug, Clone)]
pub struct Violation {
    pub id: ContentId,
    pub namespace: String,
    pub target: Target,
}

impl Violation {
    fn path(&self, vault: &Vault) -> PathBuf {
        match &self.target {
            Target::Object => vault.object_path(&self.id),
            Target::Tracked { path, .. } => path.clone(),
        }
    }

    fn live_path(&self) -> Option<PathBuf> {
        match &self.target {
            Target::Object => None,
            Target::Tracked { path, .. } => Some(path.clone()),
        }
    }
}

// ─── ViolationTracker ────────────────────────────────────────────────────────

struct ViolationTracker {
    namespace: String,
    count: u32,
    /// Restore attempts, successful or not. Reaching `escalate_after_restores`
    /// quarantines the namespace.
    restores: u32,
    last_seen: Option<Instant>,
    window_start: Instant,
}

// ─── QuarantineRegistry ──────────────────────────────────────────────────────

/// In-memory registry of quarantined namespaces.
///
/// The journal is the source of truth: `journaled` is the fold of every
/// `IntegrityEscalate` / `QuarantineLift` entry, read incrementally from a
/// byte offset. `unjournaled` holds escalations whose journal write failed,
/// so they stay in force until the entry lands or an operator lifts them.
#[derive(Default)]
pub struct QuarantineRegistry {
    journaled: BTreeSet<String>,
    unjournaled: BTreeMap<String, DateTime<Utc>>,
    offset: u64,
}

impl QuarantineRegistry {
    pub fn is_quarantined(&self, namespace: &str) -> bool {
        self.journaled.contains(namespace) || self.unjournaled.contains_key(namespace)
    }

    pub fn all_quarantined(&self) -> Vec<String> {
        self.journaled
            .iter()
            .chain(self.unjournaled.keys())
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Apply journal entries appended since the last call and return the
    /// namespaces that were lifted.
    ///
    /// Only complete lines are consumed, so a lift the CLI is still writing is
    /// picked up on the next call. If the journal shrank (rotated or truncated),
    /// state is rebuilt from the start.
    pub fn sync_from_journal(&mut self, journal: &Journal) -> Vec<String> {
        let tail = match journal.read_from(self.offset) {
            Ok(Some(tail)) => tail,
            Ok(None) => {
                warn!(
                    "QuarantineRegistry: journal is shorter than last read — rebuilding from start"
                );
                self.journaled.clear();
                self.offset = 0;
                match journal.read_from(0) {
                    Ok(Some(tail)) => tail,
                    Ok(None) => return vec![],
                    Err(e) => {
                        warn!("QuarantineRegistry: could not read journal: {}", e);
                        return vec![];
                    }
                }
            }
            Err(e) => {
                warn!("QuarantineRegistry: could not read journal: {}", e);
                return vec![];
            }
        };

        if tail.malformed > 0 {
            warn!(
                "QuarantineRegistry: skipped {} malformed journal line(s)",
                tail.malformed
            );
        }

        let mut lifted = vec![];
        for entry in &tail.entries {
            match apply_quarantine_entry(&mut self.journaled, entry) {
                Some(QuarantineChange::Escalated(ns)) => {
                    // A buffered escalation finally reached the journal.
                    self.unjournaled.remove(&ns);
                }
                Some(QuarantineChange::Lifted(ns)) => {
                    // Only a lift issued after an unjournaled escalation clears it.
                    if self.unjournaled.get(&ns).is_some_and(|at| entry.ts >= *at) {
                        self.unjournaled.remove(&ns);
                    }
                    lifted.push(ns);
                }
                None => {}
            }
        }
        self.offset = tail.next_offset;
        lifted
    }

    fn quarantine_journaled(&mut self, namespace: &str) {
        self.journaled.insert(namespace.to_string());
    }

    fn quarantine_unjournaled(&mut self, namespace: &str, at: DateTime<Utc>) {
        self.unjournaled.insert(namespace.to_string(), at);
    }
}

// ─── IntegrityGuard ──────────────────────────────────────────────────────────

pub struct IntegrityGuard {
    vault: Arc<Vault>,
    journal: Arc<Journal>,
    cache: SharedCache,
    quarantine: QuarantineRegistry,
    alert_tx: mpsc::Sender<IntegrityAlert>,
    /// Keyed by the file being protected (object path or live path).
    trackers: HashMap<PathBuf, ViolationTracker>,
    /// Entries that failed to reach the journal. Non-empty means degraded.
    pending_journal: VecDeque<JournalEntry>,
    dropped_journal_entries: u64,
    dropped_alerts: u64,
    violation_threshold: u32,
    violation_window: Duration,
    debounce: Duration,
    auto_restore: bool,
    escalate_after_restores: u32,
    restore_order: RestoreOrder,
}

impl IntegrityGuard {
    /// Build the guard and replay quarantine state from the journal.
    /// Does file I/O — call from `spawn_blocking`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        vault: Arc<Vault>,
        journal: Arc<Journal>,
        cache: SharedCache,
        alert_tx: mpsc::Sender<IntegrityAlert>,
        violation_threshold: u32,
        violation_window_secs: u64,
        debounce_ms: u64,
        auto_restore: bool,
        escalate_after_restores: u32,
        restore_order: RestoreOrder,
    ) -> Self {
        let mut guard = Self {
            vault,
            journal,
            cache,
            quarantine: QuarantineRegistry::default(),
            alert_tx,
            trackers: HashMap::new(),
            pending_journal: VecDeque::new(),
            dropped_journal_entries: 0,
            dropped_alerts: 0,
            violation_threshold,
            violation_window: Duration::from_secs(violation_window_secs),
            debounce: Duration::from_millis(debounce_ms),
            auto_restore,
            escalate_after_restores,
            restore_order,
        };
        guard.sync_quarantine();
        guard
    }

    /// Pick up journal entries written by other processes (CLI lifts).
    /// Violation counts for lifted namespaces are reset so they start fresh.
    pub fn sync_quarantine(&mut self) {
        self.flush_pending_journal();
        for ns in self.quarantine.sync_from_journal(&self.journal) {
            let before = self.trackers.len();
            self.trackers.retain(|_, t| t.namespace != ns);
            if self.trackers.len() != before {
                info!(
                    "IntegrityGuard: quarantine lifted on '{}' — violation counts reset",
                    ns
                );
            }
        }
    }

    pub fn quarantined(&self) -> Vec<String> {
        self.quarantine.all_quarantined()
    }

    /// `true` while journal entries are buffered in memory (auto-restore suspended).
    pub fn is_degraded(&self) -> bool {
        !self.pending_journal.is_empty()
    }

    pub fn dropped_alerts(&self) -> u64 {
        self.dropped_alerts
    }

    /// Called by WatcherService for every sealed record that fails
    /// verification and every tracked file written with invisible characters.
    pub fn record_violation(&mut self, v: Violation) {
        let now = Instant::now();
        self.flush_pending_journal();
        let path = v.path(&self.vault);

        let tracker = self
            .trackers
            .entry(path.clone())
            .or_insert_with(|| ViolationTracker {
                namespace: v.namespace.clone(),
                count: 0,
                restores: 0,
                last_seen: None,
                window_start: now,
            });

        // Debounce: events closer together than the debounce window count
        // once, but each is still repaired — a burst of writes must not slip
        // through uncounted *and* unrepaired. The first event is never
        // debounced.
        let debounced = tracker
            .last_seen
            .is_some_and(|last| now.duration_since(last) < self.debounce);
        tracker.last_seen = Some(now);

        if !debounced {
            // Start a new rolling window if the current one has expired.
            if now.duration_since(tracker.window_start) > self.violation_window {
                tracker.count = 0;
                tracker.window_start = now;
            }
            tracker.count += 1;
        }

        let count = tracker.count;
        let restores = tracker.restores;

        if !debounced {
            self.alert(IntegrityAlert::ViolationDetected {
                path: path.clone(),
                count,
            });
            self.journal_write(entry(
                JournalOp::WriteViolation,
                &v,
                None,
                format!("violation_count={count}"),
            ));
        }

        // Quarantined namespaces are watched and journaled, never repaired.
        if self.quarantine.is_quarantined(&v.namespace) || !self.auto_restore {
            return;
        }

        if count < self.violation_threshold {
            // Below the threshold, prevent the write: strip what it added.
            let Target::Tracked { baseline, .. } = &v.target else {
                return;
            };
            let baseline = *baseline;
            if self.is_degraded() {
                self.repair_suspended(path);
                return;
            }
            if self.sanitize(&v, &path, baseline) {
                return;
            }
            // Deleted, or no longer text: nothing to strip, so recreate it.
        }

        if restores >= self.escalate_after_restores {
            self.escalate(&v);
            return;
        }
        if self.is_degraded() {
            self.repair_suspended(path);
            return;
        }
        self.restore(&v, path);
    }

    /// A tracked file changed with no invisible characters added: an ordinary
    /// edit. Store it as the stable version or as pending, per `mode`.
    pub fn record_clean_edit(&mut self, seen: &TrackedFile, content: Vec<u8>, mode: ApprovalMode) {
        self.store_version(
            &seen.path,
            |t| t.stable == seen.stable && t.pending == seen.pending,
            content,
            mode,
            format!("approval={}", mode.as_str()),
        );
    }

    /// Record `content` as a version of the tracked file at `path`: stable
    /// (automatic) or pending (approved). `unchanged` re-checks the state
    /// under the tracking lock, since the CLI may have approved or untracked
    /// the file meanwhile; if it fails, nothing happens and the next scan
    /// looks again.
    fn store_version(
        &mut self,
        path: &Path,
        unchanged: impl Fn(&TrackedFile) -> bool,
        content: Vec<u8>,
        mode: ApprovalMode,
        detail: String,
    ) {
        self.flush_pending_journal();
        // Never change state we can't record; the next scan retries.
        if self.is_degraded() {
            debug!(
                "IntegrityGuard: journal unavailable — not versioning {}",
                path.display()
            );
            return;
        }

        let vault = self.vault.clone();
        let result = vault.update_tracked(path, |state| {
            let Some(t) = state.as_mut().filter(|t| unchanged(t)) else {
                return Ok(None);
            };
            let id = vault.write_version(&t.namespace, &t.path, &content)?;
            match mode {
                ApprovalMode::Automatic => {
                    t.stable = id;
                    t.pending = None;
                }
                ApprovalMode::Approved => t.pending = Some(id),
            }
            t.updated_at = Utc::now();
            Ok(Some((id, t.namespace.as_str())))
        });

        let (id, namespace) = match result {
            Ok(Some(stored)) => stored,
            Ok(None) => return,
            Err(e) => {
                warn!(
                    "IntegrityGuard: could not version {}: {}",
                    path.display(),
                    e
                );
                return;
            }
        };
        let size = content.len() as u64;
        lock(&self.cache).insert(id, content);

        let path = path.to_path_buf();
        let (op, alert) = match mode {
            ApprovalMode::Automatic => {
                info!("IntegrityGuard: accepted clean edit of {}", path.display());
                (
                    JournalOp::VersionAccepted,
                    IntegrityAlert::VersionAccepted { path: path.clone() },
                )
            }
            ApprovalMode::Approved => {
                info!(
                    "IntegrityGuard: edit of {} is pending approval ({})",
                    path.display(),
                    detail
                );
                (
                    JournalOp::VersionPending,
                    IntegrityAlert::VersionPending { path: path.clone() },
                )
            }
        };
        self.journal_write(journal_entry(
            op,
            id,
            namespace,
            Some(path),
            Some(size),
            detail,
        ));
        self.alert(alert);
    }

    /// Start tracking a file that appeared under a tracked directory. Its
    /// first version becomes stable; invisible characters in it are
    /// stripped first, since a new file has no baseline that could excuse
    /// them. Binary and oversized files are left alone.
    pub fn adopt(&mut self, dir: &TrackedDir, path: &Path) {
        self.flush_pending_journal();
        if self.is_degraded() || self.quarantine.is_quarantined(&dir.namespace.as_str()) {
            return;
        }
        // Nothing on another filesystem, i.e. a mount below the tracked
        // directory — checked before reading, so a pseudo-filesystem mounted
        // there is never even opened.
        match std::fs::symlink_metadata(path) {
            Ok(m) if dir.same_filesystem(&m) => {}
            _ => return,
        }
        let Ok(Live::File { content, meta }) = read_live(path) else {
            return;
        };
        if meta.len() > MAX_ADOPT_BYTES || !dir.same_filesystem(&meta) {
            return;
        }
        let observed = hash_bytes(&content);
        let Ok(text) = String::from_utf8(content) else {
            return;
        };
        let namespace = dir.namespace.as_str();
        let journal_path = Some(path.to_path_buf());

        let found = invisible::scan(&text);
        let content = if let Some(first) = found.first() {
            let detail = format!("removed={} first={} (new file)", found.len(), first);
            if !self.journal_write(journal_entry(
                JournalOp::IntegrityRestoreIntent,
                observed,
                namespace.clone(),
                journal_path.clone(),
                None,
                format!("action=sanitize {detail}"),
            )) {
                return;
            }
            let cleaned = invisible::strip_introduced("", &text);
            if let Err(e) = rewrite_file(path, cleaned.as_bytes(), Some(observed)) {
                // Changed meanwhile (the next event retries) or unwritable.
                debug!("IntegrityGuard: not adopting {} yet: {}", path.display(), e);
                self.journal_write(journal_entry(
                    JournalOp::IntegrityRestoreFailed,
                    observed,
                    namespace,
                    journal_path,
                    None,
                    format!("action=sanitize {e}"),
                ));
                return;
            }
            warn!(
                "IntegrityGuard: stripped invisible characters from new file {} ({})",
                path.display(),
                detail
            );
            self.journal_write(journal_entry(
                JournalOp::IntegritySanitize,
                observed,
                namespace.clone(),
                journal_path.clone(),
                Some(found.len() as u64),
                detail,
            ));
            self.alert(IntegrityAlert::Sanitized {
                path: path.to_path_buf(),
                removed: found.len(),
            });
            cleaned.into_bytes()
        } else {
            text.into_bytes()
        };

        let vault = self.vault.clone();
        let adopted = vault.update_tracked(path, |state| {
            if state.is_some() {
                return Ok(None);
            }
            let id = vault.write_version(&dir.namespace, path, &content)?;
            *state = Some(TrackedFile {
                path: path.to_path_buf(),
                namespace: dir.namespace.clone(),
                stable: id,
                pending: None,
                updated_at: Utc::now(),
                source: Some(dir.path.clone()),
            });
            Ok(Some(id))
        });
        match adopted {
            Ok(Some(id)) => {
                info!(
                    "IntegrityGuard: now tracking {} (in {})",
                    path.display(),
                    dir.path.display()
                );
                lock(&self.cache).insert(id, content);
                self.journal_write(journal_entry(
                    JournalOp::TrackFile,
                    id,
                    namespace,
                    journal_path,
                    None,
                    format!("adopted from {}", dir.path.display()),
                ));
            }
            Ok(None) => {}
            Err(e) => warn!("IntegrityGuard: could not adopt {}: {}", path.display(), e),
        }
    }

    /// A file tracked through a directory was deleted, and the approval mode
    /// is automatic: accept that like any other clean change, and stop
    /// tracking it. (In approved mode a deletion is a violation instead.)
    pub fn record_removal(&mut self, seen: &TrackedFile) {
        self.flush_pending_journal();
        if self.is_degraded() {
            return;
        }
        let removed = self.vault.update_tracked(&seen.path, |state| {
            if state.as_ref() != Some(seen) {
                return Ok(false);
            }
            *state = None;
            Ok(true)
        });
        match removed {
            Ok(true) => {
                info!(
                    "IntegrityGuard: {} was deleted — no longer tracking it",
                    seen.path.display()
                );
                self.trackers.remove(&seen.path);
                self.journal_write(journal_entry(
                    JournalOp::UntrackFile,
                    seen.stable,
                    seen.namespace.as_str(),
                    Some(seen.path.clone()),
                    None,
                    "deleted (approval=automatic)".into(),
                ));
            }
            Ok(false) => {}
            Err(e) => warn!(
                "IntegrityGuard: could not untrack {}: {}",
                seen.path.display(),
                e
            ),
        }
    }

    /// Keep a tracked file's stable and pending versions in memory, so a
    /// restore doesn't depend on the vault's copy (or it on the cache).
    pub fn ensure_cached(&mut self, t: &TrackedFile) {
        for id in [Some(t.stable), t.pending].into_iter().flatten() {
            if lock(&self.cache).contains(&id) {
                continue;
            }
            match self.vault.read_verified(&id) {
                Ok(data) => {
                    lock(&self.cache).insert(id, data);
                }
                Err(e) => warn!(
                    "IntegrityGuard: no verified vault copy of {} for {}: {}",
                    id,
                    t.path.display(),
                    e
                ),
            }
        }
    }

    /// A verified copy of `id`, from the sources `restore_order` allows.
    /// The vault is only independent of tracked files (`allow_store`).
    pub fn verified_copy(
        &mut self,
        id: &ContentId,
        allow_store: bool,
    ) -> Option<(Arc<[u8]>, RestoreSource)> {
        let try_memory = !allow_store || self.restore_order != RestoreOrder::Store;
        let try_store = allow_store && self.restore_order != RestoreOrder::Memory;
        if try_memory {
            // Only the Arc is cloned under the lock.
            if let Some(data) = lock(&self.cache).get(id) {
                return Some((data, RestoreSource::MemoryCache));
            }
        }
        if try_store {
            match self.vault.read_verified(id) {
                Ok(data) => return Some((data.into(), RestoreSource::VaultStore)),
                Err(e) => debug!("IntegrityGuard: vault copy of {} unusable: {}", id, e),
            }
        }
        None
    }

    /// Strip the invisible characters a write added to a tracked file.
    /// Returns `false` if there was nothing to strip because the file is
    /// missing, not a regular file, or not text, so the caller should
    /// recreate it instead.
    fn sanitize(&mut self, v: &Violation, path: &Path, baseline: ContentId) -> bool {
        let Ok(Live::File { content, .. }) = read_live(path) else {
            return false;
        };
        let observed = hash_bytes(&content);
        let Ok(live) = String::from_utf8(content) else {
            return false;
        };
        let base = match self.verified_copy(&baseline, true) {
            Some((data, _)) => String::from_utf8_lossy(&data).into_owned(),
            None => {
                warn!(
                    "IntegrityGuard: no verified baseline for {} — stripping every invisible character",
                    path.display()
                );
                String::new()
            }
        };
        let found = invisible::introduced(&base, &live);
        let Some(first) = found.first() else {
            // Rewritten cleanly since the watcher looked.
            return true;
        };
        let detail = format!("removed={} first={}", found.len(), first);

        if !self.journal_write(entry(
            JournalOp::IntegrityRestoreIntent,
            v,
            None,
            format!("action=sanitize {detail}"),
        )) {
            self.repair_suspended(path.to_path_buf());
            return true;
        }
        let cleaned = invisible::strip_introduced(&base, &live);
        match rewrite_file(path, cleaned.as_bytes(), Some(observed)) {
            Ok(()) => {
                warn!(
                    "IntegrityGuard: stripped invisible characters from {} ({})",
                    path.display(),
                    detail
                );
                self.journal_write(entry(
                    JournalOp::IntegritySanitize,
                    v,
                    Some(found.len() as u64),
                    detail,
                ));
                self.alert(IntegrityAlert::Sanitized {
                    path: path.to_path_buf(),
                    removed: found.len(),
                });
                // The same write changed visible text too. It arrived with
                // invisible characters, so it's never trusted as an edit:
                // hold it for approval whatever the mode.
                if cleaned != base {
                    let stable = v.id;
                    self.store_version(
                        path,
                        |t| t.stable == stable && t.baseline() == baseline,
                        cleaned.into_bytes(),
                        ApprovalMode::Approved,
                        "reason=arrived with invisible characters".into(),
                    );
                }
            }
            Err(e @ UltnasCoreError::ChangedDuringWrite(_)) => {
                info!("IntegrityGuard: {} — rechecking on the next scan", e);
                self.journal_write(entry(
                    JournalOp::IntegrityRestoreFailed,
                    v,
                    None,
                    format!("action=sanitize {e}"),
                ));
            }
            Err(e) => {
                error!(
                    "IntegrityGuard: could not sanitize {}: {}",
                    path.display(),
                    e
                );
                self.journal_write(entry(
                    JournalOp::IntegrityRestoreFailed,
                    v,
                    None,
                    format!("action=sanitize {e}"),
                ));
                self.alert(IntegrityAlert::RestoreFailed {
                    path: path.to_path_buf(),
                    reason: e.to_string(),
                });
            }
        }
        true
    }

    fn repair_suspended(&mut self, path: PathBuf) {
        warn!(
            "IntegrityGuard: journal unavailable — not repairing {}",
            path.display()
        );
        self.alert(IntegrityAlert::RestoreFailed {
            path,
            reason: "journal unavailable — auto-restore suspended".into(),
        });
    }

    /// Delete the damaged file and recreate it from a verified copy.
    fn restore(&mut self, v: &Violation, path: PathBuf) {
        let tracked = matches!(v.target, Target::Tracked { .. });
        let copy = self.verified_copy(&v.id, tracked);
        let source_name = copy.as_ref().map_or("none", |(_, s)| s.as_str());

        // Write-ahead: never touch the file without a record of intent.
        if !self.journal_write(entry(
            JournalOp::IntegrityRestoreIntent,
            v,
            None,
            format!("source={source_name}"),
        )) {
            self.alert(IntegrityAlert::RestoreFailed {
                path,
                reason: "journal unavailable — restore not attempted".into(),
            });
            return;
        }

        if let Some(t) = self.trackers.get_mut(&path) {
            t.restores += 1;
        }

        let result = match copy {
            Some((data, source)) => {
                let written = match &v.target {
                    Target::Object => self.vault.restore_object(&v.id, &data),
                    Target::Tracked { observed, .. } => recreate_file(&path, &data, *observed),
                };
                written.map(|()| (data.len() as u64, source))
            }
            None => Err(UltnasCoreError::RestoreFailed {
                id: v.id.to_hex(),
                reason: if tracked {
                    "no verified copy in memory or the vault".into()
                } else {
                    "not in the memory cache, and a sealed object has no independent copy".into()
                },
            }),
        };

        match result {
            Ok((size, source)) => {
                info!(
                    "IntegrityGuard: restored {} from {}",
                    path.display(),
                    source.as_str()
                );
                if let Some(t) = self.trackers.get_mut(&path) {
                    t.count = 0;
                }
                self.journal_write(entry(
                    JournalOp::IntegrityRestore,
                    v,
                    Some(size),
                    format!("source={}", source.as_str()),
                ));
                self.alert(IntegrityAlert::RestoreSucceeded { path, source });
            }
            Err(e @ UltnasCoreError::ChangedDuringWrite(_)) => {
                // Someone wrote again first; that write gets inspected next
                // scan. An abandoned restore doesn't count toward escalation.
                info!("IntegrityGuard: {} — rechecking on the next scan", e);
                if let Some(t) = self.trackers.get_mut(&path) {
                    t.restores = t.restores.saturating_sub(1);
                }
                self.journal_write(entry(
                    JournalOp::IntegrityRestoreFailed,
                    v,
                    None,
                    e.to_string(),
                ));
            }
            Err(e) => {
                error!(
                    "IntegrityGuard: restore of {} failed: {}",
                    path.display(),
                    e
                );
                self.journal_write(entry(
                    JournalOp::IntegrityRestoreFailed,
                    v,
                    None,
                    e.to_string(),
                ));
                self.alert(IntegrityAlert::RestoreFailed {
                    path,
                    reason: e.to_string(),
                });
            }
        }
    }

    fn escalate(&mut self, v: &Violation) {
        warn!(
            "IntegrityGuard: escalating namespace '{}' to quarantine",
            v.namespace
        );
        // Journal first, then memory — so a rebuild from the journal agrees.
        let at = Utc::now();
        let mut escalation = entry(
            JournalOp::IntegrityEscalate,
            v,
            None,
            format!("threshold={}", self.escalate_after_restores),
        );
        escalation.ts = at;
        if self.journal_write(escalation) {
            self.quarantine.quarantine_journaled(&v.namespace);
        } else {
            // Fail closed: quarantine anyway, and remember it wasn't journaled.
            self.quarantine.quarantine_unjournaled(&v.namespace, at);
        }
        self.alert(IntegrityAlert::NamespaceQuarantined {
            namespace: v.namespace.clone(),
        });
    }

    /// Write an entry, or buffer it if the journal is unwritable (entering
    /// degraded mode). Returns `true` only if it reached the journal.
    fn journal_write(&mut self, entry: JournalEntry) -> bool {
        if self.pending_journal.is_empty() {
            match self.journal.write(entry.clone()) {
                Ok(()) => return true,
                Err(e) => {
                    error!(
                        "IntegrityGuard: journal write failed — entering degraded mode: {}",
                        e
                    );
                    self.alert(IntegrityAlert::JournalDegraded {
                        reason: e.to_string(),
                    });
                }
            }
        }
        if self.pending_journal.len() >= MAX_PENDING_JOURNAL {
            self.pending_journal.pop_front();
            self.dropped_journal_entries += 1;
            error!(
                "IntegrityGuard: journal buffer full — {} entr(y/ies) lost so far",
                self.dropped_journal_entries,
            );
        }
        self.pending_journal.push_back(entry);
        false
    }

    /// Retry buffered entries in order; leave degraded mode once all land.
    fn flush_pending_journal(&mut self) {
        if self.pending_journal.is_empty() {
            return;
        }
        while let Some(entry) = self.pending_journal.front() {
            if let Err(e) = self.journal.write(entry.clone()) {
                debug!("IntegrityGuard: journal still unwritable: {}", e);
                return;
            }
            self.pending_journal.pop_front();
        }
        info!("IntegrityGuard: journal writable again — leaving degraded mode");
    }

    fn alert(&mut self, alert: IntegrityAlert) {
        if self.alert_tx.try_send(alert).is_err() {
            self.dropped_alerts += 1;
            if self.dropped_alerts.is_power_of_two() {
                warn!(
                    "IntegrityGuard: alert channel full or closed — {} alert(s) dropped \
                     (the journal still has the full record)",
                    self.dropped_alerts,
                );
            }
        }
    }
}

fn entry(op: JournalOp, v: &Violation, size: Option<u64>, detail: String) -> JournalEntry {
    journal_entry(op, v.id, v.namespace.clone(), v.live_path(), size, detail)
}

fn journal_entry(
    op: JournalOp,
    id: ContentId,
    namespace: String,
    path: Option<PathBuf>,
    size: Option<u64>,
    detail: String,
) -> JournalEntry {
    JournalEntry {
        ts: Utc::now(),
        op,
        id: Some(id),
        ns: Some(namespace),
        label: None,
        size,
        detail: Some(detail),
        path,
    }
}

#[cfg(test)]
mod tests {
    use super::super::verified_cache::VerifiedCache;
    use super::*;
    use tempfile::TempDir;
    use ultnas_core::{NamespacePath, RecordBuilder};

    const CONTENT: &[u8] = b"sealed content";

    struct Fixture {
        _dir: TempDir,
        vault: Arc<Vault>,
        journal: Arc<Journal>,
        guard: IntegrityGuard,
        v: Violation,
        _rx: mpsc::Receiver<IntegrityAlert>,
    }

    /// threshold 2, escalate after 1 restore attempt, no debounce.
    fn fixture(cache_it: bool) -> Fixture {
        let dir = TempDir::new().unwrap();
        let vault = Arc::new(Vault::init(dir.path(), "t").unwrap());
        let journal = Arc::new(Journal::open(&dir.path().join("journal.log")).unwrap());
        let record = RecordBuilder::new(NamespacePath::parse("docs").unwrap(), "r")
            .build(CONTENT)
            .unwrap();
        vault.write_record(&record, CONTENT).unwrap();

        let mut cache = VerifiedCache::new(1024);
        if cache_it {
            cache.insert(record.id, CONTENT.to_vec());
        }
        let (tx, rx) = mpsc::channel(64);
        let guard = IntegrityGuard::new(
            vault.clone(),
            journal.clone(),
            Arc::new(Mutex::new(cache)),
            tx,
            2,
            300,
            0,
            true,
            1,
            RestoreOrder::MemoryThenStore,
        );
        let v = Violation {
            id: record.id,
            namespace: "docs".into(),
            target: Target::Object,
        };
        Fixture {
            _dir: dir,
            vault,
            journal,
            guard,
            v,
            _rx: rx,
        }
    }

    fn tamper(f: &Fixture) {
        std::fs::write(f.vault.object_path(&f.v.id), b"tampered").unwrap();
    }

    #[test]
    fn first_violation_is_counted_and_threshold_triggers_restore() {
        let mut f = fixture(true);
        tamper(&f);
        f.guard.record_violation(f.v.clone());
        assert_eq!(f.journal.count_op(&JournalOp::WriteViolation).unwrap(), 1);
        assert!(f.vault.verify(&f.v.id).is_err());

        f.guard.record_violation(f.v.clone());
        f.vault.verify(&f.v.id).unwrap();
        assert_eq!(
            f.journal
                .count_op(&JournalOp::IntegrityRestoreIntent)
                .unwrap(),
            1
        );
        assert_eq!(f.journal.count_op(&JournalOp::IntegrityRestore).unwrap(), 1);
    }

    #[test]
    fn cache_miss_fails_restore_then_escalates_once() {
        let mut f = fixture(false);
        tamper(&f);
        for _ in 0..6 {
            f.guard.record_violation(f.v.clone());
        }

        assert!(
            f.vault.verify(&f.v.id).is_err(),
            "must not 'restore' tampered bytes"
        );
        assert_eq!(f.journal.count_op(&JournalOp::IntegrityRestore).unwrap(), 0);
        assert_eq!(
            f.journal
                .count_op(&JournalOp::IntegrityRestoreFailed)
                .unwrap(),
            1
        );
        assert_eq!(
            f.journal.count_op(&JournalOp::IntegrityEscalate).unwrap(),
            1
        );
        assert_eq!(f.guard.quarantined(), vec!["docs".to_string()]);
    }

    #[test]
    fn cli_lift_clears_quarantine_and_resets_counts() {
        let mut f = fixture(false);
        tamper(&f);
        for _ in 0..3 {
            f.guard.record_violation(f.v.clone());
        }
        assert_eq!(f.guard.quarantined(), vec!["docs".to_string()]);

        // What `ultnas integrity lift-quarantine docs` writes.
        let cli = Journal::open(&f.vault.root().join("journal.log")).unwrap();
        cli.write(JournalEntry {
            ts: Utc::now(),
            op: JournalOp::QuarantineLift,
            id: None,
            ns: Some("docs".into()),
            label: None,
            size: None,
            detail: None,
            path: None,
        })
        .unwrap();

        f.guard.sync_quarantine();
        assert!(f.guard.quarantined().is_empty());

        // Counts restarted: one more violation must not re-escalate immediately.
        f.guard.record_violation(f.v.clone());
        assert_eq!(
            f.journal.count_op(&JournalOp::IntegrityEscalate).unwrap(),
            1
        );
        assert!(f.guard.quarantined().is_empty());
    }
}
