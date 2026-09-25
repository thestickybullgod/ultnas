//! IntegrityGuard — write-violation detection, restore pipeline, and quarantine.
//!
//! ## Design overview
//!
//! ```text
//! WatcherService (spawn_blocking) ──► IntegrityGuard::record_violation(v)
//!                          │
//!                    debounce (N ms)
//!                          │
//!                    count within rolling window >= threshold?
//!                         YES
//!                          │
//!                    namespace quarantined? ── YES ──► detect + journal only
//!                          │
//!                    restore attempts >= escalate_after_restores?
//!                         YES ──► journal IntegrityEscalate, then quarantine
//!                          │
//!                    restore:
//!                      journal IntegrityRestoreIntent (write-ahead)
//!                      L1 → VerifiedCache (hash-checked on admission)
//!                      Vault::restore_object (hash-checked, atomic + fsync)
//!                      journal IntegrityRestore / IntegrityRestoreFailed
//! ```
//!
//! ## Threading
//! Everything here is synchronous file I/O, so `IntegrityGuard` is a plain
//! sync struct shared as `Arc<std::sync::Mutex<_>>`. Only call into it from
//! `spawn_blocking` — never lock it on an async worker thread.
//!
//! ## Restore sources
//! L1 (VerifiedCache) is the only independent copy today. The object store
//! can't be a restore source: the object *is* the file being restored. An L2
//! tier (replica / pack / remote) slots in after L1 in [`IntegrityGuard::restore`].
//!
//! ## Journal failures (degraded mode)
//! If the journal can't be written, the guard keeps detecting and keeps
//! quarantining (fail closed) but stops auto-restoring, so it never changes
//! data it can't record. Unwritten entries are buffered (bounded) and retried
//! at the start of every `record_violation` / `sync_quarantine` call.
//!
//! ## QuarantineRegistry
//! Owned by the guard and folded incrementally from the journal (see
//! [`QuarantineRegistry::sync_from_journal`]), so CLI-issued lifts apply on
//! the next watcher scan without a daemon restart.

use chrono::{DateTime, Utc};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use ultnas_core::{
    apply_quarantine_entry, ContentId, Journal, JournalEntry, JournalOp, QuarantineChange,
    UltnasCoreError, Vault,
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
}

impl RestoreSource {
    fn as_str(&self) -> &'static str {
        match self {
            RestoreSource::MemoryCache => "memory_cache",
        }
    }
}

/// A sealed record whose object no longer matches its ContentId.
#[derive(Debug, Clone)]
pub struct Violation {
    pub id: ContentId,
    pub namespace: String,
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
    trackers: HashMap<ContentId, ViolationTracker>,
    /// Entries that failed to reach the journal. Non-empty means degraded.
    pending_journal: VecDeque<JournalEntry>,
    dropped_journal_entries: u64,
    dropped_alerts: u64,
    violation_threshold: u32,
    violation_window: Duration,
    debounce: Duration,
    auto_restore: bool,
    escalate_after_restores: u32,
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

    /// Called by WatcherService for every sealed record that fails verification.
    pub fn record_violation(&mut self, v: Violation) {
        let now = Instant::now();
        self.flush_pending_journal();
        let path = self.vault.object_path(&v.id);

        let tracker = self
            .trackers
            .entry(v.id)
            .or_insert_with(|| ViolationTracker {
                namespace: v.namespace.clone(),
                count: 0,
                restores: 0,
                last_seen: None,
                window_start: now,
            });

        // Debounce: ignore events closer together than the debounce window.
        // The first event for a record is never debounced.
        if tracker
            .last_seen
            .is_some_and(|last| now.duration_since(last) < self.debounce)
        {
            tracker.last_seen = Some(now);
            return;
        }
        tracker.last_seen = Some(now);

        // Start a new rolling window if the current one has expired.
        if now.duration_since(tracker.window_start) > self.violation_window {
            tracker.count = 0;
            tracker.window_start = now;
        }
        tracker.count += 1;

        let count = tracker.count;
        let restores = tracker.restores;

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

        // Quarantined namespaces are watched and journaled, never auto-restored.
        if self.quarantine.is_quarantined(&v.namespace) {
            return;
        }
        if count < self.violation_threshold || !self.auto_restore {
            return;
        }

        if restores >= self.escalate_after_restores {
            self.escalate(&v);
            return;
        }
        if self.is_degraded() {
            warn!(
                "IntegrityGuard: journal unavailable — not restoring {}",
                path.display()
            );
            self.alert(IntegrityAlert::RestoreFailed {
                path,
                reason: "journal unavailable — auto-restore suspended".into(),
            });
            return;
        }
        self.restore(&v, path);
    }

    fn restore(&mut self, v: &Violation, path: PathBuf) {
        let source = RestoreSource::MemoryCache;

        // Write-ahead: never touch the object store without a record of intent.
        if !self.journal_write(entry(
            JournalOp::IntegrityRestoreIntent,
            v,
            None,
            format!("source={}", source.as_str()),
        )) {
            self.alert(IntegrityAlert::RestoreFailed {
                path,
                reason: "journal unavailable — restore not attempted".into(),
            });
            return;
        }

        if let Some(t) = self.trackers.get_mut(&v.id) {
            t.restores += 1;
        }

        // L1: VerifiedCache. Only the Arc is cloned under the lock.
        let cached = lock(&self.cache).get(&v.id);
        let result = match cached {
            Some(data) => self
                .vault
                .restore_object(&v.id, &data)
                .map(|()| data.len() as u64),
            None => Err(UltnasCoreError::RestoreFailed {
                id: v.id.to_hex(),
                reason: "not in L1 cache and no independent L2 source is configured".into(),
            }),
        };

        match result {
            Ok(size) => {
                info!("IntegrityGuard: restored {} from L1 cache", path.display());
                if let Some(t) = self.trackers.get_mut(&v.id) {
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
    JournalEntry {
        ts: Utc::now(),
        op,
        id: Some(v.id),
        ns: Some(v.namespace.clone()),
        label: None,
        size,
        detail: Some(detail),
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
        );
        let v = Violation {
            id: record.id,
            namespace: "docs".into(),
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
