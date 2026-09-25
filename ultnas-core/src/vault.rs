//! On-disk vault layout and atomic read/write primitives.

use crate::{hash_bytes, ContentId, NamespacePath, Record, UltnasCoreError};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

/// On-disk vault manifest stored in `vault.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultManifest {
    pub name: String,
    pub created_at: chrono::DateTime<Utc>,
    pub version: u32,
    pub policy_path: Option<PathBuf>,
}

/// An open Ultnas vault.
pub struct Vault {
    root: PathBuf,
    manifest: VaultManifest,
}

impl Vault {
    /// Initialize a new vault at `root`. Fails if already a vault.
    pub fn init(root: &Path, name: &str) -> Result<Self, UltnasCoreError> {
        if root.join("vault.toml").exists() {
            return Err(UltnasCoreError::VaultAlreadyExists(root.to_path_buf()));
        }
        fs::create_dir_all(root)?;
        for subdir in &["objects", "records", "seals"] {
            fs::create_dir_all(root.join(subdir))?;
        }
        let manifest = VaultManifest {
            name: name.to_string(),
            created_at: Utc::now(),
            version: 1,
            policy_path: None,
        };
        let toml_str = toml::to_string(&manifest)
            .map_err(|e| UltnasCoreError::Serialization(e.to_string()))?;
        atomic_write(&root.join("vault.toml"), toml_str.as_bytes())?;
        Ok(Self {
            root: root.to_path_buf(),
            manifest,
        })
    }

    /// Open an existing vault at `root`.
    pub fn open(root: &Path) -> Result<Self, UltnasCoreError> {
        let manifest_path = root.join("vault.toml");
        if !manifest_path.exists() {
            return Err(UltnasCoreError::VaultNotFound(root.to_path_buf()));
        }
        let raw = fs::read_to_string(&manifest_path)?;
        let manifest: VaultManifest = toml::from_str(&raw)?;
        Ok(Self {
            root: root.to_path_buf(),
            manifest,
        })
    }

    /// Write content bytes and record metadata atomically.
    pub fn write_record(&self, record: &Record, content: &[u8]) -> Result<(), UltnasCoreError> {
        // Write content object
        fs::create_dir_all(self.object_dir(&record.id))?;
        atomic_write(&self.object_path(&record.id), content)?;

        // Write record metadata JSON
        let rec_path = self
            .root
            .join("records")
            .join(format!("{}.json", record.id.to_hex()));
        let json = serde_json::to_vec_pretty(record)
            .map_err(|e| UltnasCoreError::Serialization(e.to_string()))?;
        atomic_write(&rec_path, &json)?;
        Ok(())
    }

    /// Read the content bytes for a record by ContentId.
    pub fn read_content(&self, id: &ContentId) -> Result<Vec<u8>, UltnasCoreError> {
        Ok(fs::read(self.object_path(id))?)
    }

    /// Overwrite the object for `id` with `data`, but only if `data` hashes to `id`.
    ///
    /// Used by the daemon's restore pipeline, so a bad restore source can never
    /// write wrong bytes into the object store.
    pub fn restore_object(&self, id: &ContentId, data: &[u8]) -> Result<(), UltnasCoreError> {
        let actual = hash_bytes(data);
        if actual != *id {
            return Err(UltnasCoreError::IntegrityFailure {
                expected: id.to_hex(),
                actual: actual.to_hex(),
            });
        }
        fs::create_dir_all(self.object_dir(id))?;
        atomic_write(&self.object_path(id), data)
    }

    /// Fetch a record's metadata by ContentId.
    pub fn get_record(&self, id: &ContentId) -> Result<Record, UltnasCoreError> {
        let path = self
            .root
            .join("records")
            .join(format!("{}.json", id.to_hex()));
        if !path.exists() {
            return Err(UltnasCoreError::RecordNotFound(id.to_hex()));
        }
        let raw = fs::read(&path)?;
        serde_json::from_slice(&raw).map_err(|e| UltnasCoreError::Serialization(e.to_string()))
    }

    /// List all records in a namespace (non-recursive).
    pub fn list_records(&self, namespace: &NamespacePath) -> Result<Vec<Record>, UltnasCoreError> {
        let records_dir = self.root.join("records");
        let mut out = vec![];
        for entry in fs::read_dir(&records_dir)? {
            let entry = entry?;
            let raw = fs::read(entry.path())?;
            if let Ok(record) = serde_json::from_slice::<Record>(&raw) {
                if record.namespace == *namespace {
                    out.push(record);
                }
            }
        }
        out.sort_by_key(|a| a.created_at);
        Ok(out)
    }

    /// Every parseable record in the vault, in no particular order.
    /// Files that can't be read or parsed (e.g. leftover temp files) are skipped.
    pub fn all_records(&self) -> Result<Vec<Record>, UltnasCoreError> {
        let mut out = vec![];
        for entry in fs::read_dir(self.root.join("records"))? {
            let path = entry?.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let Ok(raw) = fs::read(&path) else { continue };
            if let Ok(record) = serde_json::from_slice::<Record>(&raw) {
                out.push(record);
            }
        }
        Ok(out)
    }

    /// The vault's policy: `explicit` if given, else the manifest's
    /// `policy_path` (relative to the vault root), else `None` (built-in
    /// defaults). Returns the policy with the file it came from.
    pub fn load_policy(
        &self,
        explicit: Option<&Path>,
    ) -> Result<Option<(crate::Policy, PathBuf)>, UltnasCoreError> {
        let path = explicit.map(Path::to_path_buf).or_else(|| {
            self.manifest
                .policy_path
                .as_ref()
                .map(|p| self.root.join(p))
        });
        let Some(path) = path else {
            return Ok(None);
        };
        let raw = fs::read_to_string(&path)?;
        let policy = crate::Policy::from_toml(&raw)
            .map_err(|e| UltnasCoreError::InvalidPolicy(format!("{}: {e}", path.display())))?;
        Ok(Some((policy, path)))
    }

    /// Remove a record: its metadata, content object, and seal file. Missing
    /// pieces are fine. The caller journals the purge first.
    pub fn purge_record(&self, id: &ContentId) -> Result<(), UltnasCoreError> {
        let hex = id.to_hex();
        for path in [
            self.root.join("records").join(format!("{hex}.json")),
            self.object_path(id),
            self.root.join("seals").join(format!("{hex}.seal")),
        ] {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Verify a record's on-disk content matches its ContentId.
    pub fn verify(&self, id: &ContentId) -> Result<(), UltnasCoreError> {
        let content = self.read_content(id)?;
        let record = self.get_record(id)?;
        record.verify_content(&content)
    }

    /// Verify all records. Returns list of verified ContentIds.
    pub fn verify_all(&self) -> Result<Vec<ContentId>, UltnasCoreError> {
        let records_dir = self.root.join("records");
        let mut verified = vec![];
        for entry in fs::read_dir(&records_dir)? {
            let entry = entry?;
            let raw = fs::read(entry.path())?;
            if let Ok(record) = serde_json::from_slice::<Record>(&raw) {
                self.verify(&record.id)?;
                verified.push(record.id);
            }
        }
        Ok(verified)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn manifest(&self) -> &VaultManifest {
        &self.manifest
    }

    /// On-disk path of the content object for `id` (`objects/ab/abcdef…`).
    pub fn object_path(&self, id: &ContentId) -> PathBuf {
        self.object_dir(id).join(id.to_hex())
    }

    fn object_dir(&self, id: &ContentId) -> PathBuf {
        let hex = id.to_hex();
        self.root.join("objects").join(&hex[..2])
    }
}

/// Write `data` to `path` atomically: temp file → fsync → rename → fsync dir.
///
/// The temp name includes the PID so the CLI and daemon never share one.
pub(crate) fn atomic_write(path: &Path, data: &[u8]) -> Result<(), UltnasCoreError> {
    atomic_write_as(path, data, None, || Ok(()))
}

/// [`atomic_write`], giving the new file `like`'s permissions (and, on Unix,
/// owner) before it is renamed into place, so there is no window where it
/// has the daemon's defaults. `before_rename` runs last and can veto.
pub(crate) fn atomic_write_as(
    path: &Path,
    data: &[u8],
    like: Option<&fs::Metadata>,
    before_rename: impl FnOnce() -> Result<(), UltnasCoreError>,
) -> Result<(), UltnasCoreError> {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("object");
    let tmp = path.with_file_name(format!("{}.{}.tmp", file_name, std::process::id()));
    // The name is predictable, so never open an existing one: a symbolic
    // link planted there would redirect the write. Clear a stale temp (or
    // such a link, which remove_file deletes rather than follows) first.
    let _ = fs::remove_file(&tmp);
    let result = (|| -> Result<(), UltnasCoreError> {
        {
            let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            file.write_all(data)?;
            file.sync_all()?;
        }
        if let Some(meta) = like {
            fs::set_permissions(&tmp, meta.permissions())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                // Best effort: only root may give a file away. As the owner, a
                // same-owner chown succeeds and anything else keeps our identity.
                let _ = std::os::unix::fs::chown(&tmp, Some(meta.uid()), Some(meta.gid()));
            }
        }
        before_rename()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    sync_parent_dir(path)
}

/// Persist the rename itself. Windows has no directory fsync; NTFS journals
/// the metadata change, so this is a no-op there.
#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> Result<(), UltnasCoreError> {
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> Result<(), UltnasCoreError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NamespacePath, RecordBuilder};
    use tempfile::TempDir;

    #[test]
    fn init_open_roundtrip() {
        let dir = TempDir::new().unwrap();
        let vault = Vault::init(dir.path(), "test-vault").unwrap();
        assert_eq!(vault.manifest().name, "test-vault");
        let vault2 = Vault::open(dir.path()).unwrap();
        assert_eq!(vault2.manifest().name, "test-vault");
    }

    #[test]
    fn write_and_read_record() {
        let dir = TempDir::new().unwrap();
        let vault = Vault::init(dir.path(), "test").unwrap();
        let ns = NamespacePath::parse("test/ns").unwrap();
        let content = b"hello vault";
        let record = RecordBuilder::new(ns, "test-record")
            .media_type("text/plain")
            .build(content)
            .unwrap();
        vault.write_record(&record, content).unwrap();
        let read_back = vault.read_content(&record.id).unwrap();
        assert_eq!(read_back, content);
        vault.verify(&record.id).unwrap();
    }

    #[test]
    fn restore_object_rejects_wrong_bytes_and_repairs_with_right_ones() {
        let dir = TempDir::new().unwrap();
        let vault = Vault::init(dir.path(), "test").unwrap();
        let content = b"original bytes";
        let record = RecordBuilder::new(NamespacePath::parse("a").unwrap(), "r")
            .build(content)
            .unwrap();
        vault.write_record(&record, content).unwrap();

        std::fs::write(vault.object_path(&record.id), b"tampered").unwrap();
        assert!(vault.verify(&record.id).is_err());

        assert!(matches!(
            vault.restore_object(&record.id, b"also wrong"),
            Err(UltnasCoreError::IntegrityFailure { .. })
        ));
        vault.restore_object(&record.id, content).unwrap();
        vault.verify(&record.id).unwrap();
        assert_eq!(vault.all_records().unwrap().len(), 1);
    }
}
