//! File-system watcher service.
//!
//! Checks two things and forwards what it finds to `IntegrityGuard`:
//!
//! - **Sealed records**: any object in the vault whose bytes no longer match
//!   its ContentId.
//! - **Tracked files**: live text files outside the vault. A change that adds
//!   no invisible characters is an ordinary edit, versioned per the policy's
//!   approval mode. A change that adds some, makes the file non-text,
//!   deletes it, or replaces it with a symbolic link or other non-regular
//!   file is a violation. Links are never followed.
//!
//! ## Events, with a full scan as backstop
//! File-system events (inotify on Linux, FSEvents on macOS,
//! ReadDirectoryChangesW on Windows, via `notify`) drive tracked-file checks:
//! each write is inspected within milliseconds, so every attempt counts once.
//!
//! - Each tracked file's *directory* is watched, not the file: restores and
//!   editors replace files by rename, which would orphan a per-file watch.
//! - The vault root (for `journal.log`: CLI quarantine lifts) and `tracked/`
//!   (files tracked or untracked by the CLI) are watched too, so both apply
//!   at once and the watch set follows the tracked set.
//! - Events are batched over [`SETTLE`], so one save is one check.
//! - Reads are ignored. The daemon's own reads would otherwise feed back
//!   into events; its own writes do produce events, but re-checking a file
//!   the daemon just repaired finds it unchanged.
//!
//! A full scan (every sealed object and tracked file) runs at start, every
//! `full_scan_interval`, and whenever the OS reports dropped events. Sealed
//! objects are only checked by full scans. If watching can't start at all,
//! the service falls back to full scans every [`FALLBACK_POLL`].
//!
//! Checks hash files, so they — and the guard calls they make — run in
//! `spawn_blocking`, off the async workers.

use notify::{
    event::{AccessKind, AccessMode},
    Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use ultnas_core::{
    hash_bytes,
    invisible::{classify_change, Change},
    read_live, ContentId, Live, Policy, TrackedFile, Vault,
};

use super::integrity_guard::{lock, IntegrityGuard, Target, Violation};

/// How long to collect events after the first one before checking.
pub const SETTLE: Duration = Duration::from_millis(50);
/// Full-scan period when file-system events are unavailable.
pub const FALLBACK_POLL: Duration = Duration::from_secs(30);
/// Events buffered between batches; beyond this, a full scan replaces them.
const EVENT_QUEUE: usize = 4096;

pub struct WatcherService {
    vault: Arc<Vault>,
    guard: Arc<Mutex<IntegrityGuard>>,
    policy: Arc<Policy>,
    full_scan_interval: Duration,
}

/// What one pass of the service checks.
#[derive(Debug)]
enum Job {
    Full,
    /// Paths named by a batch of events.
    Paths(HashSet<PathBuf>),
}

/// Directories whose events the service interprets, in the form `notify`
/// reports them (canonical, like tracked paths).
#[derive(Clone)]
struct Roots {
    root: PathBuf,
    tracked_dir: PathBuf,
}

impl WatcherService {
    pub fn new(
        vault: Arc<Vault>,
        guard: Arc<Mutex<IntegrityGuard>>,
        policy: Arc<Policy>,
        full_scan_interval: Duration,
    ) -> Self {
        Self {
            vault,
            guard,
            policy,
            full_scan_interval,
        }
    }

    /// Main service loop.
    pub async fn run(self) {
        let roots = match self.roots() {
            Ok(roots) => Some(roots),
            Err(e) => {
                warn!(
                    "WatcherService: can't resolve the vault's directories: {}",
                    e
                );
                None
            }
        };
        let (tx, mut rx) = mpsc::channel(EVENT_QUEUE);
        let overflow = Arc::new(AtomicBool::new(false));
        let mut watches = match roots.as_ref().map(|_| Watches::new(tx, overflow.clone())) {
            Some(Ok(w)) => Some(w),
            Some(Err(e)) => {
                warn!("WatcherService: file-system events unavailable: {}", e);
                None
            }
            None => None,
        };
        let period = if watches.is_some() {
            info!(
                "WatcherService started (file-system events; full scan every {}s)",
                self.full_scan_interval.as_secs()
            );
            self.full_scan_interval
        } else {
            warn!(
                "WatcherService started in polling mode (full scan every {}s)",
                FALLBACK_POLL.as_secs()
            );
            FALLBACK_POLL
        };

        let mut interval = tokio::time::interval(period);
        let mut known: HashSet<PathBuf> = HashSet::new();
        loop {
            let job = tokio::select! {
                _ = interval.tick() => Job::Full,
                Some(first) = rx.recv() => {
                    tokio::time::sleep(SETTLE).await;
                    let mut batch = vec![first];
                    while let Ok(more) = rx.try_recv() {
                        batch.push(more);
                    }
                    batch_job(batch, overflow.swap(false, Ordering::Relaxed))
                }
            };
            debug!("WatcherService: {:?}", job);

            let (vault, guard, policy) =
                (self.vault.clone(), self.guard.clone(), self.policy.clone());
            let (prev, roots2) = (known.clone(), roots.clone());
            let checked = tokio::task::spawn_blocking(move || {
                run_job(&vault, &guard, &policy, job, &prev, roots2.as_ref())
            })
            .await;
            match checked {
                Ok(Some(tracked)) => {
                    known = tracked;
                    if let (Some(w), Some(r)) = (watches.as_mut(), roots.as_ref()) {
                        w.sync(wanted_dirs(r, &known));
                    }
                }
                Ok(None) => {}
                Err(e) => error!("WatcherService: check task panicked: {}", e),
            }
        }
    }

    fn roots(&self) -> std::io::Result<Roots> {
        let root = std::fs::canonicalize(self.vault.root())?;
        let tracked_dir = root.join("tracked");
        std::fs::create_dir_all(&tracked_dir)?;
        Ok(Roots { root, tracked_dir })
    }
}

/// Turn one batch of watcher signals into a job.
fn batch_job(batch: Vec<Signal>, overflowed: bool) -> Job {
    let mut paths = HashSet::new();
    for signal in batch {
        match signal {
            Signal::Rescan => return Job::Full,
            Signal::Paths(p) => paths.extend(p),
        }
    }
    if overflowed {
        Job::Full
    } else {
        Job::Paths(paths)
    }
}

/// Directories to watch: the vault root, `tracked/`, and every tracked
/// file's parent.
fn wanted_dirs(roots: &Roots, tracked: &HashSet<PathBuf>) -> HashSet<PathBuf> {
    let mut dirs: HashSet<PathBuf> = tracked
        .iter()
        .filter_map(|p| p.parent().map(Path::to_path_buf))
        .collect();
    dirs.insert(roots.root.clone());
    dirs.insert(roots.tracked_dir.clone());
    dirs
}

enum Signal {
    Paths(Vec<PathBuf>),
    /// The OS dropped events, or the watcher hit an error: rescan everything.
    Rescan,
}

/// The OS-level watches, kept in step with the tracked set.
struct Watches {
    inner: RecommendedWatcher,
    dirs: HashSet<PathBuf>,
}

impl Watches {
    fn new(tx: mpsc::Sender<Signal>, overflow: Arc<AtomicBool>) -> notify::Result<Self> {
        let inner = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let signal = match res {
                Ok(event) if event.need_rescan() => Signal::Rescan,
                Ok(event) if !is_write(&event.kind) => return,
                Ok(event) => Signal::Paths(event.paths),
                Err(_) => Signal::Rescan,
            };
            if tx.try_send(signal).is_err() {
                overflow.store(true, Ordering::Relaxed);
            }
        })?;
        Ok(Self {
            inner,
            dirs: HashSet::new(),
        })
    }

    fn sync(&mut self, wanted: HashSet<PathBuf>) {
        let stale: Vec<PathBuf> = self.dirs.difference(&wanted).cloned().collect();
        for dir in stale {
            let _ = self.inner.unwatch(&dir);
            self.dirs.remove(&dir);
        }
        for dir in wanted {
            if self.dirs.contains(&dir) {
                continue;
            }
            // A missing directory is retried after the next job; the full
            // scan still covers files in it meanwhile.
            match self.inner.watch(&dir, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    self.dirs.insert(dir);
                }
                Err(e) => debug!("WatcherService: can't watch {}: {}", dir.display(), e),
            }
        }
    }
}

/// Anything that may have changed a file's contents or existence. Plain
/// reads (including the daemon's own) are not.
fn is_write(kind: &EventKind) -> bool {
    match kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) => false,
        _ => true,
    }
}

/// Run one job and return the tracked set it saw, or `None` if the vault
/// couldn't be read (the caller keeps what it had).
fn run_job(
    vault: &Vault,
    guard: &Mutex<IntegrityGuard>,
    policy: &Policy,
    job: Job,
    known: &HashSet<PathBuf>,
    roots: Option<&Roots>,
) -> Option<HashSet<PathBuf>> {
    // Cheap and incremental: applies any CLI lift just written.
    lock(guard).sync_quarantine();

    let tracked = match vault.tracked_files() {
        Ok(tracked) => tracked,
        Err(e) => {
            warn!("WatcherService: could not list tracked files: {}", e);
            return None;
        }
    };
    let paths: HashSet<PathBuf> = tracked.iter().map(|t| t.path.clone()).collect();

    match job {
        Job::Full => {
            scan_sealed(vault, guard);
            check_tracked(guard, policy, tracked.iter());
        }
        Job::Paths(hits) => {
            // Newly tracked files get a first check; so does anything hit.
            let set_changed = roots.is_some_and(|r| {
                hits.iter()
                    .any(|p| p.parent() == Some(r.tracked_dir.as_path()))
            });
            check_tracked(
                guard,
                policy,
                tracked.iter().filter(|t| {
                    hits.contains(&t.path) || (set_changed && !known.contains(&t.path))
                }),
            );
        }
    }
    Some(paths)
}

/// Full check: sync quarantine, then every sealed record and tracked file.
#[cfg(test)]
fn scan(vault: &Vault, guard: &Mutex<IntegrityGuard>, policy: &Policy) {
    run_job(vault, guard, policy, Job::Full, &HashSet::new(), None);
}

/// Report every sealed record whose object no longer matches its ContentId
/// (including missing objects).
fn scan_sealed(vault: &Vault, guard: &Mutex<IntegrityGuard>) {
    let records = match vault.all_records() {
        Ok(records) => records,
        Err(e) => {
            warn!("WatcherService: could not list records: {}", e);
            return;
        }
    };

    for record in records.into_iter().filter(|r| r.is_sealed()) {
        if let Err(e) = vault.verify(&record.id) {
            warn!(
                "WatcherService: integrity violation detected on {}: {}",
                record.id, e
            );
            lock(guard).record_violation(Violation {
                id: record.id,
                namespace: record.namespace.as_str(),
                target: Target::Object,
            });
        }
    }
}

/// Classify each tracked file's current contents against its baseline.
fn check_tracked<'a>(
    guard: &Mutex<IntegrityGuard>,
    policy: &Policy,
    tracked: impl Iterator<Item = &'a TrackedFile>,
) {
    for t in tracked {
        let live = match read_live(&t.path) {
            Ok(live) => live,
            Err(e) => {
                warn!("WatcherService: could not read {}: {}", t.path.display(), e);
                continue;
            }
        };

        let mut g = lock(guard);
        g.ensure_cached(t);

        let live = match live {
            Live::File { content, .. } => content,
            Live::Missing => {
                warn!(
                    "WatcherService: tracked file {} was deleted",
                    t.path.display()
                );
                g.record_violation(violation(t, None));
                continue;
            }
            Live::NotRegular => {
                warn!(
                    "WatcherService: tracked file {} was replaced by a link or other non-regular file",
                    t.path.display()
                );
                g.record_violation(violation(t, None));
                continue;
            }
        };
        let id = hash_bytes(&live);
        if id == t.stable || Some(id) == t.pending {
            continue;
        }

        let baseline = match g.verified_copy(&t.baseline(), true) {
            Some((data, _)) => data,
            None => {
                warn!(
                    "WatcherService: no verified baseline for {} — any invisible character counts",
                    t.path.display()
                );
                Arc::from(&[][..])
            }
        };
        match classify_change(&baseline, &live) {
            Change::Clean => {
                let mode = policy.approval_for(&t.namespace);
                g.record_clean_edit(t, live, mode);
            }
            Change::Introduced(found) => {
                warn!(
                    "WatcherService: {} invisible character(s) written to {} (first: {})",
                    found.len(),
                    t.path.display(),
                    found[0]
                );
                g.record_violation(violation(t, Some(id)));
            }
            Change::NotText => {
                warn!(
                    "WatcherService: tracked file {} is no longer UTF-8 text",
                    t.path.display()
                );
                g.record_violation(violation(t, Some(id)));
            }
        }
    }
}

fn violation(t: &TrackedFile, observed: Option<ContentId>) -> Violation {
    Violation {
        id: t.stable,
        namespace: t.namespace.as_str(),
        target: Target::Tracked {
            path: t.path.clone(),
            baseline: t.baseline(),
            observed,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::super::integrity_guard::RestoreOrder;
    use super::super::verified_cache::VerifiedCache;
    use super::*;
    use chrono::Utc;
    use std::{fs, path::PathBuf};
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use ultnas_core::{canonical_path, ApprovalMode, ContentId, Journal, JournalOp, NamespacePath};

    const CLEAN: &str = "let is_admin = false;\n";

    struct Fixture {
        _dir: TempDir,
        vault: Arc<Vault>,
        journal: Arc<Journal>,
        guard: Mutex<IntegrityGuard>,
        policy: Policy,
        live: PathBuf,
        stable: ContentId,
        _rx: mpsc::Receiver<super::super::integrity_guard::IntegrityAlert>,
    }

    /// One tracked file; threshold 3, escalate after 2 restores, no debounce.
    fn fixture(approval: ApprovalMode, cache_it: bool) -> Fixture {
        let dir = TempDir::new().unwrap();
        let vault = Arc::new(Vault::init(&dir.path().join("vault"), "t").unwrap());
        let journal = Arc::new(Journal::open(&vault.root().join("journal.log")).unwrap());
        let live = dir.path().join("config.rs");
        fs::write(&live, CLEAN).unwrap();
        let live = canonical_path(&live).unwrap();

        let ns = NamespacePath::parse("code").unwrap();
        let stable = vault.write_version(&ns, &live, CLEAN.as_bytes()).unwrap();
        vault
            .update_tracked(&live, |t| {
                *t = Some(TrackedFile {
                    path: live.clone(),
                    namespace: ns,
                    stable,
                    pending: None,
                    updated_at: Utc::now(),
                });
                Ok(())
            })
            .unwrap();

        let mut cache = VerifiedCache::new(1 << 20);
        if cache_it {
            cache.insert(stable, CLEAN.as_bytes().to_vec());
        }
        let (tx, rx) = mpsc::channel(64);
        let guard = IntegrityGuard::new(
            vault.clone(),
            journal.clone(),
            Arc::new(Mutex::new(cache)),
            tx,
            3,
            300,
            0,
            true,
            2,
            RestoreOrder::MemoryThenStore,
        );
        let mut policy = Policy::default();
        policy.global.integrity.approval = approval;
        Fixture {
            _dir: dir,
            vault,
            journal,
            guard: Mutex::new(guard),
            policy,
            live,
            stable,
            _rx: rx,
        }
    }

    impl Fixture {
        fn scan(&self) {
            scan(&self.vault, &self.guard, &self.policy);
        }
        fn write(&self, s: &str) {
            fs::write(&self.live, s).unwrap();
        }
        fn read(&self) -> String {
            fs::read_to_string(&self.live).unwrap()
        }
        fn tracked(&self) -> TrackedFile {
            self.vault.tracked_files().unwrap().remove(0)
        }
        fn count(&self, op: JournalOp) -> usize {
            self.journal.count_op(&op).unwrap()
        }
    }

    #[test]
    fn unchanged_file_is_left_alone() {
        let f = fixture(ApprovalMode::Automatic, true);
        f.scan();
        assert_eq!(f.read(), CLEAN);
        assert_eq!(f.count(JournalOp::WriteViolation), 0);
    }

    #[test]
    fn attempts_below_threshold_are_stripped_then_file_is_recreated() {
        let f = fixture(ApprovalMode::Automatic, true);

        // Attempts 1 and 2: prevented by stripping in place.
        f.write("let is_\u{200B}admin = false;\n");
        f.scan();
        assert_eq!(f.read(), CLEAN);
        f.write("let is_admin = \u{202E}false;\n");
        f.scan();
        assert_eq!(f.read(), CLEAN);
        assert_eq!(f.count(JournalOp::IntegritySanitize), 2);
        assert_eq!(f.count(JournalOp::IntegrityRestore), 0);

        // Attempt 3 hits the threshold: delete and recreate from memory.
        f.write("let is_admin = true;\u{200B}\n");
        f.scan();
        assert_eq!(f.read(), CLEAN);
        assert_eq!(f.count(JournalOp::IntegrityRestore), 1);
        let entries: Vec<_> = f.journal.iter().unwrap().flatten().collect();
        let restore = entries
            .iter()
            .find(|e| e.op == JournalOp::IntegrityRestore)
            .unwrap();
        assert_eq!(restore.detail.as_deref(), Some("source=memory_cache"));
        assert_eq!(restore.path.as_deref(), Some(f.live.as_path()));
        assert_eq!(f.tracked().stable, f.stable, "attack must not be versioned");
    }

    #[test]
    fn restore_falls_back_to_vault_copy() {
        let f = fixture(ApprovalMode::Automatic, false);
        // ensure_cached would load it; evict by using a zero-byte cache instead.
        let (tx, _rx) = mpsc::channel(8);
        let guard = IntegrityGuard::new(
            f.vault.clone(),
            f.journal.clone(),
            Arc::new(Mutex::new(VerifiedCache::new(0))),
            tx,
            1,
            300,
            0,
            true,
            5,
            RestoreOrder::MemoryThenStore,
        );
        let guard = Mutex::new(guard);
        f.write("tampered\u{200B}");
        scan(&f.vault, &guard, &f.policy);
        assert_eq!(f.read(), CLEAN);
        let entries: Vec<_> = f.journal.iter().unwrap().flatten().collect();
        assert!(entries.iter().any(|e| e.op == JournalOp::IntegrityRestore
            && e.detail.as_deref() == Some("source=vault_store")));
    }

    #[test]
    fn deleted_file_is_recreated() {
        let f = fixture(ApprovalMode::Automatic, true);
        fs::remove_file(&f.live).unwrap();
        f.scan();
        assert_eq!(f.read(), CLEAN);
        assert_eq!(f.count(JournalOp::IntegrityRestore), 1);
    }

    #[test]
    fn automatic_mode_accepts_clean_edit_as_stable() {
        let f = fixture(ApprovalMode::Automatic, true);
        let edit = "let is_admin = false; // reviewed\n";
        f.write(edit);
        f.scan();
        assert_eq!(f.read(), edit);
        let t = f.tracked();
        assert_eq!(t.stable, hash_bytes(edit.as_bytes()));
        assert_eq!(t.pending, None);
        assert_eq!(f.count(JournalOp::VersionAccepted), 1);

        // A later attack is restored to the accepted edit, not the original.
        f.write("let is_admin = false; // reviewed\u{200B}\n");
        f.scan();
        assert_eq!(f.read(), edit);
    }

    #[test]
    fn approved_mode_keeps_clean_edit_pending_and_restores_last_approved() {
        let f = fixture(ApprovalMode::Approved, true);
        let edit = "let is_admin = false; // draft\n";
        f.write(edit);
        f.scan();
        assert_eq!(f.read(), edit, "pending edit stays on disk");
        let t = f.tracked();
        assert_eq!(t.stable, f.stable);
        assert_eq!(t.pending, Some(hash_bytes(edit.as_bytes())));
        assert_eq!(f.count(JournalOp::VersionPending), 1);

        // Rescanning the pending edit is a no-op.
        f.scan();
        assert_eq!(f.count(JournalOp::VersionPending), 1);

        // Below threshold, an attack on the pending edit is stripped back to it.
        f.write("let is_admin = false; // draft\u{200B}\n");
        f.scan();
        assert_eq!(f.read(), edit);

        // At the threshold the file is recreated from the last *approved* copy,
        // and the pending version is kept for `ultnas approve`.
        for _ in 0..2 {
            f.write("let is_admin = false; // draft\u{200B}\n");
            f.scan();
        }
        assert_eq!(f.read(), CLEAN);
        assert_eq!(f.tracked().pending, Some(hash_bytes(edit.as_bytes())));
        assert!(f.vault.read_verified(&hash_bytes(edit.as_bytes())).is_ok());
    }

    #[test]
    fn tainted_edit_is_held_pending_even_in_automatic_mode() {
        let f = fixture(ApprovalMode::Automatic, true);
        // Visible change and an invisible character in the same write.
        f.write("let is_admin = true;\u{200B}\n");
        f.scan();
        let stripped = "let is_admin = true;\n";
        assert_eq!(f.read(), stripped);
        let t = f.tracked();
        assert_eq!(t.stable, f.stable, "must not be accepted automatically");
        assert_eq!(t.pending, Some(hash_bytes(stripped.as_bytes())));
        assert_eq!(f.count(JournalOp::VersionAccepted), 0);
        assert_eq!(f.count(JournalOp::VersionPending), 1);

        // Nothing further happens until it's approved.
        f.scan();
        assert_eq!(f.count(JournalOp::VersionPending), 1);
        assert_eq!(f.read(), stripped);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_replacement_is_recreated_without_following_it() {
        let f = fixture(ApprovalMode::Automatic, true);
        let secret = f._dir.path().join("secret");
        fs::write(&secret, "root only\n").unwrap();
        fs::remove_file(&f.live).unwrap();
        std::os::unix::fs::symlink(&secret, &f.live).unwrap();

        f.scan();
        assert!(fs::symlink_metadata(&f.live).unwrap().is_file());
        assert_eq!(f.read(), CLEAN);
        assert_eq!(fs::read_to_string(&secret).unwrap(), "root only\n");
        assert_eq!(f.tracked().stable, f.stable, "target must not be versioned");
        assert_eq!(f.count(JournalOp::VersionAccepted), 0);
    }

    /// A guard over `f`'s vault with its own settings; the stable copy cached.
    fn guard_with(f: &Fixture, threshold: u32, debounce_ms: u64) -> IntegrityGuard {
        let mut cache = VerifiedCache::new(1 << 20);
        cache.insert(f.stable, CLEAN.as_bytes().to_vec());
        let (tx, rx) = mpsc::channel(64);
        std::mem::forget(rx);
        IntegrityGuard::new(
            f.vault.clone(),
            f.journal.clone(),
            Arc::new(Mutex::new(cache)),
            tx,
            threshold,
            300,
            debounce_ms,
            true,
            2,
            RestoreOrder::MemoryThenStore,
        )
    }

    #[test]
    fn debounced_writes_count_once_but_are_each_repaired() {
        let f = fixture(ApprovalMode::Automatic, true);
        let guard = Mutex::new(guard_with(&f, 3, 60_000));
        for _ in 0..4 {
            f.write("let is_\u{200B}admin = false;\n");
            scan(&f.vault, &guard, &f.policy);
            assert_eq!(f.read(), CLEAN, "every write in a burst is stripped");
        }
        assert_eq!(f.count(JournalOp::WriteViolation), 1);
        assert_eq!(f.count(JournalOp::IntegritySanitize), 4);
        assert_eq!(f.count(JournalOp::IntegrityRestore), 0, "burst counts once");
    }

    /// Poll `ok` for up to 10 s.
    async fn eventually(mut ok: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if ok() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn events_repair_writes_without_waiting_for_a_scan() {
        let f = fixture(ApprovalMode::Automatic, true);
        let guard = Arc::new(Mutex::new(guard_with(&f, 5, 0)));
        // No periodic scan within the test: only events can repair anything.
        let svc = WatcherService::new(
            f.vault.clone(),
            guard,
            Arc::new(f.policy.clone()),
            Duration::from_secs(3600),
        );
        let task = tokio::spawn(svc.run());
        tokio::time::sleep(Duration::from_millis(500)).await;

        f.write("let is_\u{200B}admin = false;\n");
        assert!(
            eventually(|| f.read() == CLEAN).await,
            "attack not repaired"
        );

        // A file tracked after start (as the CLI would) in another directory
        // is watched too.
        let other_dir = f._dir.path().join("other");
        fs::create_dir(&other_dir).unwrap();
        fs::write(other_dir.join("b.txt"), "second\n").unwrap();
        let other = canonical_path(&other_dir.join("b.txt")).unwrap();
        let ns = NamespacePath::parse("code").unwrap();
        let id = f.vault.write_version(&ns, &other, b"second\n").unwrap();
        f.vault
            .update_tracked(&other, |t| {
                *t = Some(TrackedFile {
                    path: other.clone(),
                    namespace: ns,
                    stable: id,
                    pending: None,
                    updated_at: Utc::now(),
                });
                Ok(())
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        fs::write(&other, "sec\u{202E}ond\n").unwrap();
        assert!(
            eventually(|| fs::read_to_string(&other).unwrap() == "second\n").await,
            "attack on newly tracked file not repaired"
        );
        task.abort();
    }

    #[test]
    fn batch_job_rescans_on_overflow_or_rescan_signal() {
        let p = PathBuf::from("x");
        assert!(matches!(
            batch_job(vec![Signal::Paths(vec![p.clone()])], false),
            Job::Paths(ref s) if s.contains(&p)
        ));
        assert!(matches!(
            batch_job(vec![Signal::Paths(vec![p])], true),
            Job::Full
        ));
        assert!(matches!(batch_job(vec![Signal::Rescan], false), Job::Full));
    }

    #[test]
    fn reads_are_not_writes() {
        use notify::event::{AccessKind, AccessMode, ModifyKind};
        assert!(!is_write(&EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
        assert!(!is_write(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
        assert!(is_write(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
        assert!(is_write(&EventKind::Modify(ModifyKind::Any)));
    }

    #[test]
    fn repeated_restores_escalate_to_quarantine_and_stop_repairing() {
        let f = fixture(ApprovalMode::Automatic, true);
        // 3 attempts per restore; escalate after 2 restores.
        for _ in 0..9 {
            f.write("x\u{200B}");
            f.scan();
        }
        assert_eq!(f.count(JournalOp::IntegrityRestore), 2);
        assert_eq!(f.count(JournalOp::IntegrityEscalate), 1);
        assert_eq!(lock(&f.guard).quarantined(), vec!["code".to_string()]);

        // Quarantined: detected and journaled, but left as-is.
        f.write("y\u{200B}");
        f.scan();
        assert_eq!(f.read(), "y\u{200B}");
    }
}
