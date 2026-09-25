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
//!   Under a tracked directory, every subdirectory it could adopt from is
//!   watched — not hidden, excluded, or ignored ones, and never across into
//!   another filesystem, so tracking `/` doesn't descend into `/proc` or
//!   spend watches on `node_modules`. New subdirectories are picked up as
//!   they appear, and a file that appears is adopted (`IntegrityGuard::adopt`).
//! - The vault root (for `journal.log`: CLI quarantine lifts) and `tracked/`
//!   (files tracked or untracked by the CLI) are watched too, so both apply
//!   at once and the watch set follows the tracked set.
//! - Events are batched over [`SETTLE`], so one save is one check.
//! - Reads are ignored. The daemon's own reads would otherwise feed back
//!   into events; its own writes do produce events, but re-checking a file
//!   the daemon just repaired finds it unchanged.
//!
//! A file tracked through a directory that is deleted stops being tracked
//! in automatic mode; in approved mode the deletion is a violation, like any
//! other unapproved change, and the file is recreated.
//!
//! Sealed records are event-driven too: the vault's `objects/`, each
//! `objects/xx/` prefix directory, and `records/` are watched, and an event
//! names the content id it concerns, so only that record is verified.
//!
//! A full scan (every sealed object and tracked file, and a walk of every
//! tracked directory for files to adopt) runs at start, every
//! `full_scan_interval`, and whenever the OS reports dropped events. If watching can't start at all,
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
    covering_dir, hash_bytes,
    invisible::{classify_change, Change},
    read_live, ApprovalMode, ContentId, Live, Policy, Record, TrackedDir, TrackedFile, Vault,
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

/// The tracked set as of the last job, to spot what's new.
#[derive(Clone, Default)]
struct Known {
    files: HashSet<PathBuf>,
    dirs: Vec<TrackedDir>,
    /// Directories to watch under tracked directories.
    watch_dirs: HashSet<PathBuf>,
    /// The vault's `objects/xx/` prefix directories.
    object_dirs: HashSet<PathBuf>,
}

/// Directories whose events the service interprets, in the form `notify`
/// reports them (canonical, like tracked paths).
#[derive(Clone)]
struct Roots {
    root: PathBuf,
    tracked_dir: PathBuf,
    objects_dir: PathBuf,
    records_dir: PathBuf,
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
        let mut known = Known::default();
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
                        w.sync(wanted_watches(r, &known));
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
        Ok(Roots {
            objects_dir: root.join("objects"),
            records_dir: root.join("records"),
            root,
            tracked_dir,
        })
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

/// What to watch, each non-recursively: the directories under tracked
/// directories, each tracked file's parent, the vault root, `tracked/`,
/// `records/`, `objects/`, and each `objects/xx/`.
fn wanted_watches(roots: &Roots, known: &Known) -> HashSet<PathBuf> {
    known
        .watch_dirs
        .iter()
        .chain(&known.object_dirs)
        .cloned()
        .chain(
            known
                .files
                .iter()
                .filter_map(|p| p.parent().map(Path::to_path_buf)),
        )
        .chain([
            roots.root.clone(),
            roots.tracked_dir.clone(),
            roots.objects_dir.clone(),
            roots.records_dir.clone(),
        ])
        .collect()
}

/// The `objects/xx/` prefix directories that exist now.
fn object_dirs(roots: &Roots) -> HashSet<PathBuf> {
    std::fs::read_dir(&roots.objects_dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect()
}

/// Content ids that events under `objects/xx/<id>` or `records/<id>.json`
/// concern. Anything else there (temp files, stray names) is ignored.
fn sealed_hits(roots: &Roots, hits: &HashSet<PathBuf>) -> HashSet<ContentId> {
    hits.iter()
        .filter_map(|p| {
            let parent = p.parent()?;
            let name = if parent.parent() == Some(roots.objects_dir.as_path()) {
                p.file_name()?.to_str()?
            } else if parent == roots.records_dir {
                p.file_name()?.to_str()?.strip_suffix(".json")?
            } else {
                return None;
            };
            ContentId::from_hex(name).ok()
        })
        .collect()
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
    /// Directories that couldn't be watched at the last sync, to warn only
    /// when that changes.
    failed: usize,
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
            failed: 0,
        })
    }

    fn sync(&mut self, wanted: HashSet<PathBuf>) {
        let stale: Vec<PathBuf> = self.dirs.difference(&wanted).cloned().collect();
        for dir in stale {
            let _ = self.inner.unwatch(&dir);
            self.dirs.remove(&dir);
        }
        let mut failed = vec![];
        for dir in wanted {
            if self.dirs.contains(&dir) {
                continue;
            }
            match self.inner.watch(&dir, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    self.dirs.insert(dir);
                }
                // Gone since it was listed; the next full scan drops it.
                Err(e) if matches!(e.kind, notify::ErrorKind::PathNotFound) => {}
                Err(e) => failed.push((dir, e)),
            }
        }
        if failed.len() != self.failed {
            self.failed = failed.len();
            if let Some((dir, e)) = failed.first() {
                warn!(
                    "WatcherService: can't watch {} director(y/ies) (first: {}: {}). \
                     Files there are still checked by full scans; on Linux, raising \
                     fs.inotify.max_user_watches may help",
                    failed.len(),
                    dir.display(),
                    e
                );
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
    known: &Known,
    roots: Option<&Roots>,
) -> Option<Known> {
    // Cheap and incremental: applies any CLI lift just written.
    lock(guard).sync_quarantine();

    let (tracked, dirs) = match (vault.tracked_files(), vault.tracked_dirs()) {
        (Ok(t), Ok(d)) => (t, d),
        (Err(e), _) | (_, Err(e)) => {
            warn!("WatcherService: could not read the tracked set: {}", e);
            return None;
        }
    };
    let files: HashSet<PathBuf> = tracked.iter().map(|t| t.path.clone()).collect();

    // Directories to watch under tracked directories: walked afresh on a
    // full scan or when the tracked directories change, extended as new
    // directories appear, otherwise carried over (walking is expensive).
    let mut watch_dirs: HashSet<PathBuf> = if matches!(job, Job::Full) || known.dirs != dirs {
        dirs.iter().flat_map(|d| d.subdirs(&d.path)).collect()
    } else {
        known.watch_dirs.clone()
    };
    let mut objects = match (&job, roots) {
        (Job::Full, Some(r)) => object_dirs(r),
        _ => known.object_dirs.clone(),
    };

    match job {
        Job::Full => {
            scan_sealed(vault, guard);
            check_tracked(guard, policy, tracked.iter());
            for dir in &dirs {
                adopt_new(guard, &dirs, &files, dir.candidates());
            }
        }
        Job::Paths(hits) => {
            if let Some(r) = roots {
                // A new objects/xx/ directory, and sealed records hit.
                objects.extend(
                    hits.iter()
                        .filter(|p| p.parent() == Some(r.objects_dir.as_path()) && p.is_dir())
                        .cloned(),
                );
                for id in sealed_hits(r, &hits) {
                    if let Ok(record) = vault.get_record(&id) {
                        verify_sealed(vault, guard, &record);
                    }
                }
            }
            // A change under tracked/ may have added files: first-check them.
            let set_changed = roots.is_some_and(|r| {
                hits.iter()
                    .any(|p| p.parent() == Some(r.tracked_dir.as_path()))
            });
            check_tracked(
                guard,
                policy,
                tracked.iter().filter(|t| {
                    hits.contains(&t.path) || (set_changed && !known.files.contains(&t.path))
                }),
            );
            for dir in dirs.iter().filter(|d| !known.dirs.contains(d)) {
                adopt_new(guard, &dirs, &files, dir.candidates());
            }
            // A directory created under a tracked directory: watch it, and
            // adopt what's already inside, since files can land before the
            // watch does.
            for hit in &hits {
                let Some(dir) = covering_dir(&dirs, hit) else {
                    continue;
                };
                let is_new_dir = std::fs::symlink_metadata(hit)
                    .is_ok_and(|m| m.is_dir() && dir.same_filesystem(&m));
                if is_new_dir {
                    watch_dirs.extend(dir.subdirs(hit));
                    adopt_new(guard, &dirs, &files, dir.candidates_in(hit));
                }
            }
            adopt_new(guard, &dirs, &files, hits);
        }
    }
    Some(Known {
        files,
        dirs,
        watch_dirs,
        object_dirs: objects,
    })
}

/// Adopt each untracked path that a tracked directory covers.
fn adopt_new(
    guard: &Mutex<IntegrityGuard>,
    dirs: &[TrackedDir],
    tracked: &HashSet<PathBuf>,
    paths: impl IntoIterator<Item = PathBuf>,
) {
    for path in paths {
        if tracked.contains(&path) {
            continue;
        }
        if let Some(dir) = covering_dir(dirs, &path) {
            lock(guard).adopt(dir, &path);
        }
    }
}

/// Full check: sync quarantine, then every sealed record and tracked file.
#[cfg(test)]
fn scan(vault: &Vault, guard: &Mutex<IntegrityGuard>, policy: &Policy) {
    run_job(vault, guard, policy, Job::Full, &Known::default(), None);
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
    for record in &records {
        verify_sealed(vault, guard, record);
    }
}

/// Report `record` if it is sealed and its object no longer matches.
fn verify_sealed(vault: &Vault, guard: &Mutex<IntegrityGuard>, record: &Record) {
    if !record.is_sealed() {
        return;
    }
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
                let mode = policy.approval_for(&t.namespace);
                if t.source.is_some() && mode == ApprovalMode::Automatic {
                    g.record_removal(t);
                } else {
                    warn!(
                        "WatcherService: tracked file {} was deleted",
                        t.path.display()
                    );
                    g.record_violation(violation(t, None));
                }
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
    use ultnas_core::{
        canonical_path, ContentId, Journal, JournalOp, NamespacePath, RecordBuilder,
    };

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
                    source: None,
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
                    source: None,
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

    /// Track `f`'s directory recursively, as `ultnas track --recursive` would.
    fn track_dir(f: &Fixture) -> TrackedDir {
        let d = TrackedDir {
            path: canonical_path(f._dir.path()).unwrap(),
            namespace: NamespacePath::parse("code").unwrap(),
            exclude: vec!["target".into()],
            ignored: vec![canonical_path(f.vault.root()).unwrap()],
            added_at: Utc::now(),
            device: None,
        };
        f.vault
            .update_tracked_dir(&d.path, |s| {
                *s = Some(d.clone());
                Ok(())
            })
            .unwrap();
        d
    }

    fn tracked_paths(f: &Fixture) -> Vec<PathBuf> {
        f.vault
            .tracked_files()
            .unwrap()
            .into_iter()
            .map(|t| t.path)
            .collect()
    }

    #[test]
    fn full_scan_adopts_new_text_files_in_tracked_dirs() {
        let f = fixture(ApprovalMode::Automatic, true);
        let d = track_dir(&f);
        fs::create_dir_all(d.path.join("src")).unwrap();
        fs::create_dir_all(d.path.join("target")).unwrap();
        fs::write(d.path.join("src/new.rs"), "fn main() {}\n").unwrap();
        fs::write(d.path.join("evil.txt"), "pay\u{200B}ee\n").unwrap();
        fs::write(d.path.join("bin.dat"), [0xffu8, 0xfe, 0x00]).unwrap();
        fs::write(d.path.join("target/out.txt"), "build output\n").unwrap();
        fs::write(d.path.join("notes.txt.swp"), "scratch\n").unwrap();
        f.scan();

        let paths = tracked_paths(&f);
        assert!(paths.contains(&d.path.join("src").join("new.rs")));
        assert!(paths.contains(&d.path.join("evil.txt")));
        assert_eq!(paths.len(), 3, "{paths:?}"); // + the fixture's own file
                                                 // A new file's invisible characters are stripped before adoption.
        assert_eq!(
            fs::read_to_string(d.path.join("evil.txt")).unwrap(),
            "payee\n"
        );
        let t = f
            .vault
            .tracked_files()
            .unwrap()
            .into_iter()
            .find(|t| t.path.ends_with("evil.txt"))
            .unwrap();
        assert_eq!(t.stable, hash_bytes(b"payee\n"));
        assert_eq!(t.source.as_deref(), Some(d.path.as_path()));
    }

    #[test]
    fn deleting_a_dir_tracked_file_follows_the_approval_mode() {
        for (mode, restored) in [
            (ApprovalMode::Automatic, false),
            (ApprovalMode::Approved, true),
        ] {
            let f = fixture(mode, true);
            let d = track_dir(&f);
            let p = d.path.join("doc.txt");
            fs::write(&p, "keep me\n").unwrap();
            f.scan();
            assert!(tracked_paths(&f).contains(&p));

            fs::remove_file(&p).unwrap();
            f.scan();
            assert_eq!(p.exists(), restored, "{mode:?}");
            assert_eq!(tracked_paths(&f).contains(&p), restored, "{mode:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn events_adopt_and_protect_files_created_in_tracked_dirs() {
        let f = fixture(ApprovalMode::Automatic, true);
        let d = track_dir(&f);
        fs::create_dir_all(d.path.join("sub")).unwrap();
        let guard = Arc::new(Mutex::new(guard_with(&f, 5, 0)));
        let svc = WatcherService::new(
            f.vault.clone(),
            guard,
            Arc::new(f.policy.clone()),
            Duration::from_secs(3600),
        );
        let task = tokio::spawn(svc.run());
        tokio::time::sleep(Duration::from_millis(500)).await;

        let p = d.path.join("sub").join("fresh.txt");
        fs::write(&p, "fresh\n").unwrap();
        assert!(
            eventually(|| tracked_paths(&f).contains(&p)).await,
            "new file not adopted"
        );
        fs::write(&p, "fr\u{200B}esh\n").unwrap();
        assert!(
            eventually(|| fs::read_to_string(&p).unwrap() == "fresh\n").await,
            "attack on adopted file not repaired"
        );
        task.abort();
    }

    #[test]
    fn watches_cover_subdirs_file_parents_and_the_vault() {
        let roots = test_roots();
        let known = Known {
            files: [PathBuf::from("/w/a.txt"), PathBuf::from("/other/b.txt")].into(),
            dirs: vec![],
            watch_dirs: [PathBuf::from("/w"), PathBuf::from("/w/src")].into(),
            object_dirs: [PathBuf::from("/v/objects/ab")].into(),
        };
        let w = wanted_watches(&roots, &known);
        let want: HashSet<PathBuf> = [
            "/w",
            "/w/src",
            "/other",
            "/v",
            "/v/tracked",
            "/v/objects",
            "/v/objects/ab",
            "/v/records",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(w, want);
    }

    fn test_roots() -> Roots {
        Roots {
            root: PathBuf::from("/v"),
            tracked_dir: PathBuf::from("/v/tracked"),
            objects_dir: PathBuf::from("/v/objects"),
            records_dir: PathBuf::from("/v/records"),
        }
    }

    #[test]
    fn sealed_hits_name_objects_and_records_only() {
        let r = test_roots();
        let id = hash_bytes(b"x");
        let hex = id.to_hex();
        let hits: HashSet<PathBuf> = [
            r.objects_dir.join(&hex[..2]).join(&hex),
            r.records_dir.join(format!("{hex}.json")),
            r.objects_dir.join(&hex[..2]).join(format!("{hex}.123.tmp")),
            r.records_dir.join("not-an-id.json"),
            PathBuf::from("/elsewhere").join(&hex),
        ]
        .into();
        assert_eq!(sealed_hits(&r, &hits), [id].into());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn events_repair_sealed_records_without_a_scan() {
        let f = fixture(ApprovalMode::Automatic, true);
        let content = b"sealed record\n";
        let mut record = RecordBuilder::new(NamespacePath::parse("archive").unwrap(), "r")
            .build(content)
            .unwrap();
        record
            .seal_record("pk".into(), "sig".into(), hash_bytes(b"policy"))
            .unwrap();
        f.vault.write_record(&record, content).unwrap();

        let mut cache = VerifiedCache::new(1 << 20);
        cache.insert(record.id, content.to_vec());
        let (tx, rx) = mpsc::channel(64);
        std::mem::forget(rx);
        // Threshold 1: a sealed object is restored on its first violation.
        let guard = IntegrityGuard::new(
            f.vault.clone(),
            f.journal.clone(),
            Arc::new(Mutex::new(cache)),
            tx,
            1,
            300,
            0,
            true,
            5,
            RestoreOrder::MemoryThenStore,
        );
        let svc = WatcherService::new(
            f.vault.clone(),
            Arc::new(Mutex::new(guard)),
            Arc::new(f.policy.clone()),
            Duration::from_secs(3600),
        );
        let task = tokio::spawn(svc.run());
        tokio::time::sleep(Duration::from_millis(500)).await;

        fs::write(f.vault.object_path(&record.id), b"tampered").unwrap();
        assert!(
            eventually(|| f.vault.verify(&record.id).is_ok()).await,
            "tampered sealed object not restored"
        );
        task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn events_follow_directories_created_after_start() {
        let f = fixture(ApprovalMode::Automatic, true);
        let d = track_dir(&f);
        let guard = Arc::new(Mutex::new(guard_with(&f, 5, 0)));
        let svc = WatcherService::new(
            f.vault.clone(),
            guard,
            Arc::new(f.policy.clone()),
            Duration::from_secs(3600),
        );
        let task = tokio::spawn(svc.run());
        tokio::time::sleep(Duration::from_millis(500)).await;

        // A new directory tree, with a file in it straight away.
        let deep = d.path.join("later").join("deeper");
        fs::create_dir_all(&deep).unwrap();
        let p = deep.join("n.txt");
        fs::write(&p, "note\n").unwrap();
        assert!(
            eventually(|| tracked_paths(&f).contains(&p)).await,
            "file in new directory not adopted"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        fs::write(&p, "no\u{200B}te\n").unwrap();
        assert!(
            eventually(|| fs::read_to_string(&p).unwrap() == "note\n").await,
            "attack in new directory not repaired"
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
