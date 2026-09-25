//! Append-only structured operation journal.
//!
//! Every mutation to the vault is written here as newline-delimited JSON.
//!
//! ## Performance improvement (v3 → v4)
//! The `Journal` now holds a **persistent `BufWriter<File>` file handle**
//! protected by a `std::sync::Mutex`. Previously, every `write()` call
//! opened, wrote, and closed the file — creating unnecessary syscall overhead
//! on active vaults. The write lock is held only for the brief duration of
//! serialization + a buffered write + flush (+ fsync for integrity ops).
//! Callers in async code should still invoke it from `spawn_blocking`.

use crate::{ContentId, UltnasCoreError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
};

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
}

impl JournalOp {
    /// Ops whose entries must survive power loss, so `write()` fsyncs after them.
    fn requires_sync(&self) -> bool {
        matches!(
            self,
            JournalOp::SealRecord
                | JournalOp::PurgeRecord
                | JournalOp::IntegrityRestoreIntent
                | JournalOp::IntegrityRestore
                | JournalOp::IntegrityRestoreFailed
                | JournalOp::IntegrityEscalate
                | JournalOp::QuarantineLift
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

/// Apply one entry to a set of quarantined namespaces.
///
/// This is the single definition of quarantine state: the daemon and the CLI
/// both fold the journal through it, so they can never disagree. The last
/// `IntegrityEscalate` / `QuarantineLift` for a namespace wins.
pub fn apply_quarantine_entry(
    quarantined: &mut BTreeSet<String>,
    entry: &JournalEntry,
) -> Option<QuarantineChange> {
    let ns = entry.ns.as_ref()?;
    match entry.op {
        JournalOp::IntegrityEscalate => {
            quarantined.insert(ns.clone());
            Some(QuarantineChange::Escalated(ns.clone()))
        }
        JournalOp::QuarantineLift => {
            quarantined.remove(ns);
            Some(QuarantineChange::Lifted(ns.clone()))
        }
        _ => None,
    }
}

/// The append-only journal for a vault.
///
/// Internally holds a persistent `BufWriter<File>` so repeated writes
/// do not pay the open/close syscall cost on every entry.
pub struct Journal {
    path: PathBuf,
    // std::sync::Mutex is intentional — writes are brief sync operations
    // and we must never hold this lock across an `.await` point.
    writer: Mutex<BufWriter<File>>,
}

impl Journal {
    /// Open (or create) a journal at `path`, retaining the file handle.
    pub fn open(path: &Path) -> Result<Self, UltnasCoreError> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            writer: Mutex::new(BufWriter::new(file)),
        })
    }

    /// Append one entry. Serializes to JSON, writes a newline, then flushes.
    /// Integrity-critical ops are also fsynced (see [`JournalOp`]).
    ///
    /// The lock is held only for serialize + buffered write + flush —
    /// never across any async boundary.
    pub fn write(&self, entry: JournalEntry) -> Result<(), UltnasCoreError> {
        let line =
            serde_json::to_string(&entry).map_err(|e| UltnasCoreError::Journal(e.to_string()))?;

        // A panic elsewhere while holding this lock must not disable the
        // journal forever. The writer holds no invariant beyond "bytes not
        // yet flushed", so recovering the guard is safe.
        let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);

        writeln!(writer, "{}", line)?;
        // Explicit flush ensures the entry reaches the OS page cache
        // even if the BufWriter is not yet full.
        writer.flush()?;
        if entry.op.requires_sync() {
            writer.get_ref().sync_data()?;
        }
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
        let mut quarantined = BTreeSet::new();
        for entry in self.iter()?.flatten() {
            apply_quarantine_entry(&mut quarantined, &entry);
        }
        Ok(quarantined)
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
