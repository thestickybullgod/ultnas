//! Append-only structured operation journal.
//!
//! Every mutation to the vault is written here as newline-delimited JSON.
//!
//! ## Writers
//! The daemon owns the journal ([`Journal::open`]): it keeps a persistent
//! `BufWriter<File>` behind a `std::sync::Mutex`, held only for serialize +
//! buffered write + flush (+ fsync for integrity ops). Call it from
//! `spawn_blocking` in async code. The CLI writes occasionally
//! ([`Journal::open_shared`]): each write takes the OS lock on
//! `<journal>.lock` and opens the file afresh, so it always lands in the
//! current file even across a rotation.
//!
//! ## Rotation
//! With rotation set ([`Journal::with_rotation`]), once the file passes
//! `max_bytes` the owner renames it to `<journal>.1` (shifting older
//! archives up to `<journal>.<keep>`, dropping beyond), under the OS lock,
//! and starts a new file with a checkpoint: one `JournalRotated` entry,
//! then a `QuarantineLift` for every namespace ever lifted (at its lift
//! time) and an `IntegrityEscalate` for every namespace quarantined (at its
//! escalation time). Folding the new file alone therefore gives the same
//! quarantine state as folding everything before it — including the rule
//! that an escalation older than a lift is stale. Readers that notice the
//! file shrink ([`Journal::read_from`] returns `None`) rebuild from it.

use crate::{ContentId, UltnasCoreError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, ErrorKind, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, PoisonError},
};

/// `detail` of the entries a rotation writes at the start of a new file.
pub const CHECKPOINT: &str = "checkpoint";

/// A single journal entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub ts: DateTime<Utc>,
    pub op: JournalOp,
    pub id: Option<ContentId>,
    pub ns: Option<String>,
    pub label: Option<String>,
    pub size: Option<u64>,
    pub detail: Option<String>,
    /// Live file path, for entries about tracked files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

/// All operation types that can appear in the journal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum JournalOp {
    // Vault lifecycle
    VaultInit,
    WriteRecord,
    SealRecord,
    VerifyRecord,
    PurgeRecord,
    MetadataUpdate,
    PolicyLoad,

    // IntegrityGuard
    /// An unauthorized write was detected on a sealed record path.
    WriteViolation,
    /// Written *before* a restore touches the object store (write-ahead).
    /// Always followed by `IntegrityRestore` or `IntegrityRestoreFailed`
    /// unless the daemon crashed mid-restore.
    IntegrityRestoreIntent,
    /// Violation threshold hit; file deleted and restored from a verified source.
    IntegrityRestore,
    /// A restore was attempted and failed (no verified source, write error, …).
    IntegrityRestoreFailed,
    /// Namespace quarantined after repeated restores exceeded `escalate_after_restores`.
    IntegrityEscalate,
    /// Quarantine manually lifted by an operator via the CLI.
    QuarantineLift,

    // Tracked files
    /// A live file was put under protection (`ultnas track`).
    TrackFile,
    /// A live file was removed from protection (`ultnas untrack`).
    UntrackFile,
    /// A clean edit became the stable version (approval mode `automatic`).
    VersionAccepted,
    /// A clean edit was stored as pending (approval mode `approved`).
    VersionPending,
    /// An operator promoted the pending version (`ultnas approve`).
    VersionApproved,
    /// Invisible characters a write introduced were stripped in place.
    IntegritySanitize,

    /// First entry of a new file after rotation; `detail` names the archive.
    JournalRotated,
}

impl JournalOp {
    /// Ops whose entries must survive power loss, so `write()` fsyncs after them.
    fn requires_sync(&self) -> bool {
        matches!(
            self,
            JournalOp::SealRecord
                | JournalOp::PurgeRecord
                | JournalOp::JournalRotated
                | JournalOp::IntegrityRestoreIntent
                | JournalOp::IntegrityRestore
                | JournalOp::IntegrityRestoreFailed
                | JournalOp::IntegrityEscalate
                | JournalOp::QuarantineLift
                | JournalOp::TrackFile
                | JournalOp::UntrackFile
                | JournalOp::VersionAccepted
                | JournalOp::VersionApproved
                | JournalOp::IntegritySanitize
        )
    }
}

/// Entries read by [`Journal::read_from`].
#[derive(Debug)]
pub struct JournalTail {
    /// Parsed entries, in write order.
    pub entries: Vec<JournalEntry>,
    /// Offset to pass to the next `read_from` call. Never points past a
    /// half-written line, so a torn tail is re-read once it is complete.
    pub next_offset: u64,
    /// Complete lines that failed to parse and were skipped.
    pub malformed: usize,
}

/// A change to quarantine state caused by one journal entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuarantineChange {
    Escalated(String),
    Lifted(String),
}

/// Quarantine state folded from journal entries.
///
/// This is the single definition of quarantine state: the daemon and the CLI
/// both fold the journal through it, so they can never disagree.
///
/// Entries apply in journal order, except that an `IntegrityEscalate` older
/// than a lift already applied for its namespace is stale and ignored. The
/// daemon buffers escalations while the journal is unwritable and writes
/// them, with their original timestamps, once it recovers; an operator may
/// have lifted that quarantine in between, and appending the old escalation
/// after the lift must not undo it. (Timestamps come from one machine's
/// clock: the daemon and CLI share a vault.)
#[derive(Debug, Clone, Default)]
pub struct QuarantineFold {
    quarantined: BTreeSet<String>,
    lifted_at: BTreeMap<String, DateTime<Utc>>,
    escalated_at: BTreeMap<String, DateTime<Utc>>,
}

impl QuarantineFold {
    /// Apply one entry; returns the change it made, if any.
    pub fn apply(&mut self, entry: &JournalEntry) -> Option<QuarantineChange> {
        let ns = entry.ns.as_ref()?;
        match entry.op {
            JournalOp::IntegrityEscalate => {
                if self.lifted_at.get(ns).is_some_and(|lift| entry.ts < *lift) {
                    return None;
                }
                self.quarantined.insert(ns.clone());
                self.escalated_at.insert(ns.clone(), entry.ts);
                Some(QuarantineChange::Escalated(ns.clone()))
            }
            JournalOp::QuarantineLift => {
                self.quarantined.remove(ns);
                let last = self.lifted_at.entry(ns.clone()).or_insert(entry.ts);
                *last = (*last).max(entry.ts);
                Some(QuarantineChange::Lifted(ns.clone()))
            }
            _ => None,
        }
    }

    pub fn contains(&self, namespace: &str) -> bool {
        self.quarantined.contains(namespace)
    }

    pub fn namespaces(&self) -> &BTreeSet<String> {
        &self.quarantined
    }

    /// Entries that rebuild this state when folded from nothing: lifts
    /// first, each at its time, then the standing escalations, each at its
    /// time (always after that namespace's last lift, or it would be stale).
    fn checkpoint(&self) -> Vec<JournalEntry> {
        let at = |op, ns: &String, ts: DateTime<Utc>| JournalEntry {
            ts,
            op,
            id: None,
            ns: Some(ns.clone()),
            label: None,
            size: None,
            detail: Some(CHECKPOINT.into()),
            path: None,
        };
        let lifts = self
            .lifted_at
            .iter()
            .map(|(ns, ts)| at(JournalOp::QuarantineLift, ns, *ts));
        let standing = self.quarantined.iter().filter_map(|ns| {
            let ts = *self.escalated_at.get(ns)?;
            Some(at(JournalOp::IntegrityEscalate, ns, ts))
        });
        lifts.chain(standing).collect()
    }
}

/// `<journal>.<i>`.
fn archive_path(path: &Path, i: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{i}"));
    PathBuf::from(name)
}

/// Size-based rotation settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rotation {
    pub max_bytes: u64,
    /// Archives kept (`<journal>.1` … `<journal>.<keep>`); at least 1.
    pub keep: u32,
}

enum Writer {
    /// The daemon: a persistent handle, and the bytes in the current file.
    Owner { file: BufWriter<File>, len: u64 },
    /// The CLI: open, lock, and write per entry.
    Shared,
}

/// The append-only journal for a vault.
pub struct Journal {
    path: PathBuf,
    // std::sync::Mutex is intentional — writes are brief sync operations
    // and we must never hold this lock across an `.await` point.
    writer: Mutex<Writer>,
    rotation: Option<Rotation>,
}

impl Journal {
    /// Open (or create) a journal at `path` as its owner, retaining the
    /// file handle. Only the daemon should own a vault's journal.
    pub fn open(path: &Path) -> Result<Self, UltnasCoreError> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            writer: Mutex::new(Writer::Owner {
                file: BufWriter::new(file),
                len,
            }),
            rotation: None,
        })
    }

    /// A journal for occasional writers (the CLI): each write locks, opens
    /// the current file, appends, and closes. Nothing is created until the
    /// first write.
    pub fn open_shared(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            writer: Mutex::new(Writer::Shared),
            rotation: None,
        }
    }

    /// Rotate once the file passes `rotation.max_bytes` (owner only).
    pub fn with_rotation(mut self, rotation: Rotation) -> Self {
        self.rotation = Some(Rotation {
            keep: rotation.keep.max(1),
            ..rotation
        });
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Archived files that exist (`<journal>.1`, …), newest first.
    pub fn archives(&self) -> Vec<PathBuf> {
        (1..)
            .map(|i| archive_path(&self.path, i))
            .take_while(|p| p.exists())
            .collect()
    }

    /// Hold the OS lock that serializes shared writers with rotation.
    fn os_lock(&self) -> Result<File, UltnasCoreError> {
        let mut name = self.path.as_os_str().to_owned();
        name.push(".lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(PathBuf::from(name))?;
        lock.lock()?;
        Ok(lock)
    }

    /// Append one entry. Serializes to JSON, writes a newline, then flushes.
    /// Integrity-critical ops are also fsynced (see [`JournalOp`]).
    pub fn write(&self, entry: JournalEntry) -> Result<(), UltnasCoreError> {
        let mut line =
            serde_json::to_string(&entry).map_err(|e| UltnasCoreError::Journal(e.to_string()))?;
        line.push('\n');
        let sync = entry.op.requires_sync();

        // A panic elsewhere while holding this lock must not disable the
        // journal forever. The writer holds no invariant beyond "bytes not
        // yet flushed", so recovering the guard is safe.
        let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        match &mut *writer {
            Writer::Owner { file, len } => {
                file.write_all(line.as_bytes())?;
                // Reach the OS page cache even if the BufWriter isn't full.
                file.flush()?;
                if sync && !crate::vault::sync_deferred() {
                    file.get_ref().sync_data()?;
                }
                *len += line.len() as u64;
                if self.rotation.is_some_and(|r| *len > r.max_bytes) {
                    self.rotate(&mut writer)?;
                }
            }
            Writer::Shared => {
                let _lock = self.os_lock()?;
                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)?;
                file.write_all(line.as_bytes())?;
                if sync && !crate::vault::sync_deferred() {
                    file.sync_data()?;
                }
            }
        }
        Ok(())
    }

    /// Archive the current file and start a new one with a checkpoint.
    fn rotate(&self, writer: &mut MutexGuard<'_, Writer>) -> Result<(), UltnasCoreError> {
        let Some(rotation) = self.rotation else {
            return Ok(());
        };
        let _lock = self.os_lock()?;

        // Everything in the file, including what a shared writer appended.
        let mut fold = QuarantineFold::default();
        for entry in self.iter()?.flatten() {
            fold.apply(&entry);
        }

        // Shift archives up, dropping the oldest, then archive the current.
        let keep = rotation.keep;
        match fs::remove_file(archive_path(&self.path, keep)) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        for i in (1..keep).rev() {
            let from = archive_path(&self.path, i);
            if from.exists() {
                fs::rename(&from, archive_path(&self.path, i + 1))?;
            }
        }
        // Let go of the old handle first (Windows won't always rename a file
        // held open); writes fall back to shared mode if anything below fails.
        **writer = Writer::Shared;
        let archived = archive_path(&self.path, 1);
        fs::rename(&self.path, &archived)?;

        let mut file = BufWriter::new(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?,
        );
        let mut len = 0u64;
        let header = JournalEntry {
            ts: Utc::now(),
            op: JournalOp::JournalRotated,
            id: None,
            ns: None,
            label: None,
            size: None,
            detail: Some(format!("previous entries are in {}", archived.display())),
            path: None,
        };
        for entry in std::iter::once(header).chain(fold.checkpoint()) {
            let mut line = serde_json::to_string(&entry)
                .map_err(|e| UltnasCoreError::Journal(e.to_string()))?;
            line.push('\n');
            file.write_all(line.as_bytes())?;
            len += line.len() as u64;
        }
        file.flush()?;
        file.get_ref().sync_data()?;
        **writer = Writer::Owner { file, len };
        Ok(())
    }

    /// Open a fresh read handle and iterate over every entry in write order.
    ///
    /// Uses a separate `File::open` so reads never contend with the writer.
    pub fn iter(
        &self,
    ) -> Result<impl Iterator<Item = Result<JournalEntry, UltnasCoreError>>, UltnasCoreError> {
        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        Ok(reader.lines().map(|line| {
            let line = line.map_err(UltnasCoreError::Io)?;
            serde_json::from_str(&line).map_err(|e| UltnasCoreError::Journal(e.to_string()))
        }))
    }

    /// Read every complete entry starting at byte `offset`.
    ///
    /// Returns `Ok(None)` if the journal is now shorter than `offset`
    /// (rotated or truncated) — the caller should rebuild from offset 0.
    pub fn read_from(&self, offset: u64) -> Result<Option<JournalTail>, UltnasCoreError> {
        let mut file = File::open(&self.path)?;
        if file.metadata()?.len() < offset {
            return Ok(None);
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut reader = BufReader::new(file);

        let mut tail = JournalTail {
            entries: vec![],
            next_offset: offset,
            malformed: 0,
        };
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = reader.read_until(b'\n', &mut line)?;
            // EOF, or a line another process is still writing.
            if n == 0 || line.last() != Some(&b'\n') {
                break;
            }
            tail.next_offset += n as u64;
            match serde_json::from_slice::<JournalEntry>(&line) {
                Ok(entry) => tail.entries.push(entry),
                Err(_) => tail.malformed += 1,
            }
        }
        Ok(Some(tail))
    }

    /// Currently quarantined namespaces, folded from the whole journal.
    pub fn quarantined_namespaces(&self) -> Result<BTreeSet<String>, UltnasCoreError> {
        let mut fold = QuarantineFold::default();
        for entry in self.iter()?.flatten() {
            fold.apply(&entry);
        }
        Ok(fold.quarantined)
    }

    /// Count entries matching a specific operation type.
    pub fn count_op(&self, op: &JournalOp) -> Result<usize, UltnasCoreError> {
        let mut n = 0usize;
        for e in self.iter()?.flatten() {
            if &e.op == op {
                n += 1;
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::hash_bytes;
    use tempfile::NamedTempFile;

    fn ns_entry(op: JournalOp, ns: &str) -> JournalEntry {
        JournalEntry {
            ts: Utc::now(),
            op,
            id: None,
            ns: Some(ns.into()),
            label: None,
            size: None,
            detail: None,
            path: None,
        }
    }

    #[test]
    fn write_and_iterate() {
        let tmp = NamedTempFile::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();

        journal
            .write(JournalEntry {
                ts: Utc::now(),
                op: JournalOp::WriteRecord,
                id: Some(hash_bytes(b"test")),
                ns: Some("test/ns".into()),
                label: Some("test-label".into()),
                size: Some(42),
                detail: None,
                path: None,
            })
            .unwrap();

        journal
            .write(JournalEntry {
                ts: Utc::now(),
                op: JournalOp::WriteViolation,
                id: Some(hash_bytes(b"test")),
                ns: Some("test/ns".into()),
                label: None,
                size: None,
                detail: Some("violation count: 1".into()),
                path: None,
            })
            .unwrap();

        let entries: Vec<_> = journal.iter().unwrap().filter_map(Result::ok).collect();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].op, JournalOp::WriteRecord);
        assert_eq!(entries[1].op, JournalOp::WriteViolation);
    }

    #[test]
    fn persistent_handle_multiple_writes() {
        let tmp = NamedTempFile::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        for i in 0..100 {
            journal
                .write(JournalEntry {
                    ts: Utc::now(),
                    op: JournalOp::IntegrityRestore,
                    id: None,
                    ns: Some(format!("ns/{}", i)),
                    label: None,
                    size: None,
                    detail: None,
                    path: None,
                })
                .unwrap();
        }
        assert_eq!(journal.count_op(&JournalOp::IntegrityRestore).unwrap(), 100);
    }

    #[test]
    fn count_op_works() {
        let tmp = NamedTempFile::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        for _ in 0..3 {
            journal
                .write(JournalEntry {
                    ts: Utc::now(),
                    op: JournalOp::IntegrityEscalate,
                    id: None,
                    ns: None,
                    label: None,
                    size: None,
                    detail: None,
                    path: None,
                })
                .unwrap();
        }
        assert_eq!(journal.count_op(&JournalOp::IntegrityEscalate).unwrap(), 3);
    }

    #[test]
    fn quarantine_fold_last_entry_wins() {
        let tmp = NamedTempFile::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        journal
            .write(ns_entry(JournalOp::IntegrityEscalate, "a"))
            .unwrap();
        journal
            .write(ns_entry(JournalOp::QuarantineLift, "a"))
            .unwrap();
        journal
            .write(ns_entry(JournalOp::IntegrityEscalate, "a"))
            .unwrap();
        journal
            .write(ns_entry(JournalOp::IntegrityEscalate, "b"))
            .unwrap();
        journal
            .write(ns_entry(JournalOp::IntegrityEscalate, "b"))
            .unwrap();
        journal
            .write(ns_entry(JournalOp::QuarantineLift, "b"))
            .unwrap();
        journal
            .write(ns_entry(JournalOp::QuarantineLift, "never-quarantined"))
            .unwrap();

        let q = journal.quarantined_namespaces().unwrap();
        assert_eq!(q.into_iter().collect::<Vec<_>>(), vec!["a".to_string()]);
    }

    #[test]
    fn escalation_older_than_a_lift_does_not_undo_it() {
        let tmp = NamedTempFile::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        let t0 = Utc::now() - chrono::Duration::seconds(60);
        let at = |secs| {
            let mut e = ns_entry(JournalOp::IntegrityEscalate, "a");
            e.ts = t0 + chrono::Duration::seconds(secs);
            e
        };
        let mut lift = ns_entry(JournalOp::QuarantineLift, "a");
        lift.ts = t0 + chrono::Duration::seconds(10);

        // The operator lifts; then a buffered escalation from *before* the
        // lift finally reaches the journal.
        journal.write(lift).unwrap();
        journal.write(at(5)).unwrap();
        assert!(journal.quarantined_namespaces().unwrap().is_empty());

        // An escalation after the lift is genuine.
        journal.write(at(20)).unwrap();
        assert_eq!(
            journal
                .quarantined_namespaces()
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["a".to_string()]
        );
    }

    fn rotating(path: &Path, max_bytes: u64, keep: u32) -> Journal {
        Journal::open(path)
            .unwrap()
            .with_rotation(Rotation { max_bytes, keep })
    }

    #[test]
    fn rotation_archives_keeps_at_most_keep_and_starts_fresh() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("journal.log");
        let journal = rotating(&path, 400, 2);
        for i in 0..40 {
            journal
                .write(ns_entry(JournalOp::WriteViolation, &format!("ns{i}")))
                .unwrap();
        }
        let archives = journal.archives();
        assert_eq!(archives.len(), 2, "{archives:?}");
        assert!(!archive_path(&path, 3).exists());
        assert!(fs::metadata(&path).unwrap().len() <= 400 + 200);
        let first = journal.iter().unwrap().next().unwrap().unwrap();
        assert_eq!(first.op, JournalOp::JournalRotated);
    }

    #[test]
    fn rotation_preserves_quarantine_state_and_stale_rules() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("journal.log");
        let journal = rotating(&path, 300, 3);
        let t0 = Utc::now() - chrono::Duration::seconds(100);
        let at = |op, ns: &str, secs| {
            let mut e = ns_entry(op, ns);
            e.ts = t0 + chrono::Duration::seconds(secs);
            e
        };
        journal
            .write(at(JournalOp::IntegrityEscalate, "q", 1))
            .unwrap();
        journal
            .write(at(JournalOp::IntegrityEscalate, "lifted", 2))
            .unwrap();
        journal
            .write(at(JournalOp::QuarantineLift, "lifted", 10))
            .unwrap();
        let before = journal.quarantined_namespaces().unwrap();
        // Push past the limit so it rotates.
        while journal.archives().is_empty() {
            journal
                .write(ns_entry(JournalOp::WriteViolation, "x"))
                .unwrap();
        }
        assert_eq!(journal.quarantined_namespaces().unwrap(), before);
        assert_eq!(
            before.into_iter().collect::<Vec<_>>(),
            vec!["q".to_string()]
        );

        // A buffered escalation from before the lift still can't undo it.
        journal
            .write(at(JournalOp::IntegrityEscalate, "lifted", 5))
            .unwrap();
        assert!(!journal.quarantined_namespaces().unwrap().contains("lifted"));
    }

    #[test]
    fn shared_writers_follow_rotation_into_the_new_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("journal.log");
        let owner = rotating(&path, 300, 3);
        let cli = Journal::open_shared(&path);
        cli.write(ns_entry(JournalOp::QuarantineLift, "before"))
            .unwrap();
        while owner.archives().is_empty() {
            owner
                .write(ns_entry(JournalOp::WriteViolation, "x"))
                .unwrap();
        }
        cli.write(ns_entry(JournalOp::QuarantineLift, "after"))
            .unwrap();
        let current: Vec<_> = owner.iter().unwrap().flatten().collect();
        assert!(current.iter().any(|e| e.ns.as_deref() == Some("after")));
        // The pre-rotation lift survives as a checkpoint entry.
        assert!(current
            .iter()
            .any(|e| e.ns.as_deref() == Some("before") && e.detail.as_deref() == Some(CHECKPOINT)));
    }

    #[test]
    fn read_from_skips_half_written_tail_until_complete() {
        let tmp = NamedTempFile::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        journal
            .write(ns_entry(JournalOp::IntegrityEscalate, "a"))
            .unwrap();

        // Simulate another process mid-append.
        let full = serde_json::to_string(&ns_entry(JournalOp::QuarantineLift, "a")).unwrap();
        let (head, rest) = full.split_at(full.len() / 2);
        let mut raw = OpenOptions::new().append(true).open(tmp.path()).unwrap();
        raw.write_all(head.as_bytes()).unwrap();

        let first = journal.read_from(0).unwrap().unwrap();
        assert_eq!(first.entries.len(), 1);
        assert_eq!(first.malformed, 0);

        writeln!(raw, "{}", rest).unwrap();
        let second = journal.read_from(first.next_offset).unwrap().unwrap();
        assert_eq!(second.entries.len(), 1);
        assert_eq!(second.entries[0].op, JournalOp::QuarantineLift);

        let len = std::fs::metadata(tmp.path()).unwrap().len();
        assert_eq!(second.next_offset, len);
        assert!(journal.read_from(len + 1).unwrap().is_none());
    }
}
