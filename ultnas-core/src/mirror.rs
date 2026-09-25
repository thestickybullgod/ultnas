//! Mirror: a second, independent copy of the vault's content objects.
//!
//! For a sealed record, the object in the vault *is* the file being
//! protected, so if it is damaged and not in the daemon's memory cache there
//! is nothing to restore it from. A mirror directory, ideally on another
//! disk, holds a copy of every sealed object and tracked-file version in the
//! same content-addressed layout (`objects/ab/abcdef…`). The daemon fills
//! and repairs it on every full scan; restores fall back to it last.
//!
//! Every read is hash-checked, so a damaged mirror copy is never used, and
//! every write is atomic.

use crate::{hash_bytes, vault::atomic_write, ContentId, UltnasCoreError};
use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub struct Mirror {
    root: PathBuf,
}

impl Mirror {
    /// Open (creating if needed) a mirror at `root`.
    pub fn open(root: &Path) -> Result<Self, UltnasCoreError> {
        fs::create_dir_all(root.join("objects"))?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, id: &ContentId) -> PathBuf {
        let hex = id.to_hex();
        self.root.join("objects").join(&hex[..2]).join(hex)
    }

    /// The mirrored bytes for `id`, verified against it.
    pub fn read_verified(&self, id: &ContentId) -> Result<Vec<u8>, UltnasCoreError> {
        let data = fs::read(self.path(id))?;
        let actual = hash_bytes(&data);
        if actual != *id {
            return Err(UltnasCoreError::IntegrityFailure {
                expected: id.to_hex(),
                actual: actual.to_hex(),
            });
        }
        Ok(data)
    }

    /// Whether the mirror holds a good copy of `id` (reads and hashes it).
    pub fn has_valid(&self, id: &ContentId) -> bool {
        self.read_verified(id).is_ok()
    }

    /// Store `data` as the copy of `id`, if it really is `id`'s content.
    pub fn store(&self, id: &ContentId, data: &[u8]) -> Result<(), UltnasCoreError> {
        let actual = hash_bytes(data);
        if actual != *id {
            return Err(UltnasCoreError::IntegrityFailure {
                expected: id.to_hex(),
                actual: actual.to_hex(),
            });
        }
        let path = self.path(id);
        fs::create_dir_all(path.parent().expect("object paths have a parent"))?;
        atomic_write(&path, data)
    }

    /// Drop the copy of `id` (retention purged it). Missing is fine.
    pub fn remove(&self, id: &ContentId) -> Result<(), UltnasCoreError> {
        match fs::remove_file(self.path(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn store_read_remove() {
        let dir = TempDir::new().unwrap();
        let m = Mirror::open(dir.path()).unwrap();
        let id = hash_bytes(b"content");
        assert!(!m.has_valid(&id));
        m.store(&id, b"content").unwrap();
        assert_eq!(m.read_verified(&id).unwrap(), b"content");
        m.remove(&id).unwrap();
        m.remove(&id).unwrap();
        assert!(!m.has_valid(&id));
    }

    #[test]
    fn wrong_or_damaged_bytes_are_never_used() {
        let dir = TempDir::new().unwrap();
        let m = Mirror::open(dir.path()).unwrap();
        let id = hash_bytes(b"content");
        assert!(m.store(&id, b"other").is_err());
        m.store(&id, b"content").unwrap();
        fs::write(m.path(&id), b"damaged").unwrap();
        assert!(matches!(
            m.read_verified(&id),
            Err(UltnasCoreError::IntegrityFailure { .. })
        ));
    }
}
