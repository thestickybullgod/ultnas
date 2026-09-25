//! Tracked files: live text files outside the vault, protected in place.
//!
//! Each tracked file has a *stable* version (the copy restores come from)
//! and, when approval is required, an optional *pending* version: a clean
//! edit that is on disk but not yet approved. Both are ordinary content
//! objects in the vault, so the vault is an independent restore source for
//! the live file.
//!
//! State lives in `tracked/<blake3 of path>.json`. The CLI (track, approve)
//! and the daemon (accept, mark pending) both change it, so every change is
//! a read-modify-write under an OS lock on `tracked/.lock`
//! ([`Vault::update_tracked`]).

use crate::{
    hash_bytes, vault::atomic_write, ContentId, NamespacePath, RecordBuilder, UltnasCoreError,
    Vault,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::ErrorKind,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackedFile {
    /// Canonical absolute path of the live file.
    pub path: PathBuf,
    pub namespace: NamespacePath,
    /// Last accepted or approved version; what restores recreate.
    pub stable: ContentId,
    /// Clean edit awaiting `ultnas approve` (approval mode only).
    pub pending: Option<ContentId>,
    pub updated_at: DateTime<Utc>,
}

impl TrackedFile {
    /// The version new writes are compared against: the newest clean one.
    pub fn baseline(&self) -> ContentId {
        self.pending.unwrap_or(self.stable)
    }
}

/// Canonical form of a path to track. Falls back to canonicalizing the parent
/// when the file itself is missing (e.g. untracking a deleted file).
pub fn canonical_path(path: &Path) -> Result<PathBuf, UltnasCoreError> {
    match fs::canonicalize(path) {
        Ok(p) => Ok(p),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let name = path.file_name().ok_or(e)?;
            let parent = match path.parent() {
                Some(p) if !p.as_os_str().is_empty() => p,
                _ => Path::new("."),
            };
            Ok(fs::canonicalize(parent)?.join(name))
        }
        Err(e) => Err(e.into()),
    }
}

impl Vault {
    fn tracked_dir(&self) -> PathBuf {
        self.root().join("tracked")
    }

    fn tracked_entry_path(&self, path: &Path) -> PathBuf {
        let key = hash_bytes(path.to_string_lossy().as_bytes());
        self.tracked_dir().join(format!("{}.json", key.to_hex()))
    }

    /// Every tracked file. Vaults created before tracking existed have none.
    pub fn tracked_files(&self) -> Result<Vec<TrackedFile>, UltnasCoreError> {
        let entries = match fs::read_dir(self.tracked_dir()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut out = vec![];
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let Ok(raw) = fs::read(&path) else { continue };
            if let Ok(t) = serde_json::from_slice::<TrackedFile>(&raw) {
                out.push(t);
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// Read-modify-write one tracked file's state under the tracking lock.
    ///
    /// `f` sees the current state (`None` if untracked) and may change it;
    /// setting it to `None` untracks the file. Nothing is written if `f`
    /// fails or leaves the state unchanged.
    pub fn update_tracked<T>(
        &self,
        path: &Path,
        f: impl FnOnce(&mut Option<TrackedFile>) -> Result<T, UltnasCoreError>,
    ) -> Result<T, UltnasCoreError> {
        fs::create_dir_all(self.tracked_dir())?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.tracked_dir().join(".lock"))?;
        lock.lock()?;

        let entry_path = self.tracked_entry_path(path);
        let before = match fs::read(&entry_path) {
            Ok(raw) => Some(
                serde_json::from_slice::<TrackedFile>(&raw)
                    .map_err(|e| UltnasCoreError::Serialization(e.to_string()))?,
            ),
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let mut state = before.clone();
        let out = f(&mut state)?;

        if state != before {
            match &state {
                Some(t) => {
                    let json = serde_json::to_vec_pretty(t)
                        .map_err(|e| UltnasCoreError::Serialization(e.to_string()))?;
                    atomic_write(&entry_path, &json)?;
                }
                None => fs::remove_file(&entry_path)?,
            }
        }
        drop(lock);
        Ok(out)
    }

    /// Store `content` as a version of a tracked file and return its id.
    /// Content that is already in the vault is not rewritten.
    pub fn write_version(
        &self,
        namespace: &NamespacePath,
        path: &Path,
        content: &[u8],
    ) -> Result<ContentId, UltnasCoreError> {
        let id = hash_bytes(content);
        if self.verify(&id).is_ok() {
            return Ok(id);
        }
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let record = RecordBuilder::new(namespace.clone(), label)
            .media_type("text/plain")
            .metadata("source_path", path.to_string_lossy())
            .build(content)?;
        self.write_record(&record, content)?;
        Ok(id)
    }

    /// Read a version's bytes, verified against its id.
    pub fn read_verified(&self, id: &ContentId) -> Result<Vec<u8>, UltnasCoreError> {
        let data = self.read_content(id)?;
        let actual = hash_bytes(&data);
        if actual != *id {
            return Err(UltnasCoreError::IntegrityFailure {
                expected: id.to_hex(),
                actual: actual.to_hex(),
            });
        }
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Vault, PathBuf) {
        let dir = TempDir::new().unwrap();
        let vault = Vault::init(&dir.path().join("vault"), "t").unwrap();
        let live = dir.path().join("notes.txt");
        fs::write(&live, "hello").unwrap();
        let live = canonical_path(&live).unwrap();
        (dir, vault, live)
    }

    fn track(vault: &Vault, live: &Path) -> ContentId {
        let ns = NamespacePath::parse("docs").unwrap();
        let id = vault.write_version(&ns, live, b"hello").unwrap();
        vault
            .update_tracked(live, |t| {
                *t = Some(TrackedFile {
                    path: live.to_path_buf(),
                    namespace: ns,
                    stable: id,
                    pending: None,
                    updated_at: Utc::now(),
                });
                Ok(())
            })
            .unwrap();
        id
    }

    #[test]
    fn track_list_update_untrack() {
        let (_dir, vault, live) = setup();
        assert!(vault.tracked_files().unwrap().is_empty());
        let id = track(&vault, &live);

        let all = vault.tracked_files().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].stable, id);
        assert_eq!(all[0].baseline(), id);
        assert_eq!(vault.read_verified(&id).unwrap(), b"hello");

        let pending = hash_bytes(b"edit");
        vault
            .update_tracked(&live, |t| {
                t.as_mut().unwrap().pending = Some(pending);
                Ok(())
            })
            .unwrap();
        assert_eq!(vault.tracked_files().unwrap()[0].baseline(), pending);

        vault
            .update_tracked(&live, |t| {
                *t = None;
                Ok(())
            })
            .unwrap();
        assert!(vault.tracked_files().unwrap().is_empty());
    }

    #[test]
    fn failed_update_writes_nothing() {
        let (_dir, vault, live) = setup();
        let id = track(&vault, &live);
        let r: Result<(), _> = vault.update_tracked(&live, |t| {
            *t = None;
            Err(UltnasCoreError::Journal("boom".into()))
        });
        assert!(r.is_err());
        assert_eq!(vault.tracked_files().unwrap()[0].stable, id);
    }

    #[test]
    fn read_verified_rejects_tampered_object() {
        let (_dir, vault, live) = setup();
        let id = track(&vault, &live);
        fs::write(vault.object_path(&id), b"evil").unwrap();
        assert!(vault.read_verified(&id).is_err());
    }

    #[test]
    fn canonical_path_of_missing_file_uses_parent() {
        let (dir, _vault, _live) = setup();
        let missing = dir.path().join("gone.txt");
        let c = canonical_path(&missing).unwrap();
        assert_eq!(c.file_name().unwrap(), "gone.txt");
        assert!(c.is_absolute());
    }
}
