//! File-system watcher service.
//!
//! Each scan checks two things and forwards what it finds to `IntegrityGuard`:
//!
//! - **Sealed records**: any object in the vault whose bytes no longer match
//!   its ContentId.
//! - **Tracked files**: live text files outside the vault. A change that adds
//!   no invisible characters is an ordinary edit, versioned per the policy's
//!   approval mode. A change that adds some, makes the file non-text,
//!   deletes it, or replaces it with a symbolic link or other non-regular
//!   file is a violation. Links are never followed.
//!
//! Current implementation: polling on a 30-second interval (stub).
//! v0.3 will integrate the `notify` crate for true inotify/FSEvents/RDCW support.
//!
//! Each scan reads and hashes every sealed object and tracked file, so the
//! whole scan — and the guard calls it makes — runs in `spawn_blocking`, off
//! the async workers.

use std::sync::{Arc, Mutex};
use tracing::{debug, error, info, warn};
use ultnas_core::{
    hash_bytes,
    invisible::{classify_change, Change},
    read_live, ContentId, Live, Policy, TrackedFile, Vault,
};

use super::integrity_guard::{lock, IntegrityGuard, Target, Violation};

pub struct WatcherService {
    vault: Arc<Vault>,
    guard: Arc<Mutex<IntegrityGuard>>,
    policy: Arc<Policy>,
    poll_interval_s: u64,
}

impl WatcherService {
    pub fn new(vault: Arc<Vault>, guard: Arc<Mutex<IntegrityGuard>>, policy: Arc<Policy>) -> Self {
        Self {
            vault,
            guard,
            policy,
            poll_interval_s: 30,
        }
    }

    /// Main service loop. Polls for integrity violations.
    pub async fn run(self) {
        info!(
            "WatcherService started (poll interval: {}s)",
            self.poll_interval_s
        );
        let mut interval =
            tokio::time::interval(tokio::time::Duration::from_secs(self.poll_interval_s));
        loop {
            interval.tick().await;
            debug!("WatcherService: scanning sealed records and tracked files");
            let (vault, guard, policy) =
                (self.vault.clone(), self.guard.clone(), self.policy.clone());
            if let Err(e) = tokio::task::spawn_blocking(move || scan(&vault, &guard, &policy)).await
            {
                error!("WatcherService: scan task panicked: {}", e);
            }
        }
    }
}

/// Apply pending quarantine lifts, then check sealed records and tracked files.
fn scan(vault: &Vault, guard: &Mutex<IntegrityGuard>, policy: &Policy) {
    lock(guard).sync_quarantine();
    scan_sealed(vault, guard);
    scan_tracked(vault, guard, policy);
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

/// Classify every tracked file's current contents against its baseline.
fn scan_tracked(vault: &Vault, guard: &Mutex<IntegrityGuard>, policy: &Policy) {
    let tracked = match vault.tracked_files() {
        Ok(tracked) => tracked,
        Err(e) => {
            warn!("WatcherService: could not list tracked files: {}", e);
            return;
        }
    };

    for t in tracked {
        let live = match read_live(&t.path) {
            Ok(live) => live,
            Err(e) => {
                warn!("WatcherService: could not read {}: {}", t.path.display(), e);
                continue;
            }
        };

        let mut g = lock(guard);
        g.ensure_cached(&t);

        let live = match live {
            Live::File { content, .. } => content,
            Live::Missing => {
                warn!(
                    "WatcherService: tracked file {} was deleted",
                    t.path.display()
                );
                g.record_violation(violation(&t, None));
                continue;
            }
            Live::NotRegular => {
                warn!(
                    "WatcherService: tracked file {} was replaced by a link or other non-regular file",
                    t.path.display()
                );
                g.record_violation(violation(&t, None));
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
                g.record_clean_edit(&t, live, mode);
            }
            Change::Introduced(found) => {
                warn!(
                    "WatcherService: {} invisible character(s) written to {} (first: {})",
                    found.len(),
                    t.path.display(),
                    found[0]
                );
                g.record_violation(violation(&t, Some(id)));
            }
            Change::NotText => {
                warn!(
                    "WatcherService: tracked file {} is no longer UTF-8 text",
                    t.path.display()
                );
                g.record_violation(violation(&t, Some(id)));
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
