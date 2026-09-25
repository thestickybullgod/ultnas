//! VaultLock — exclusive, OS-enforced ownership of a vault by one daemon.
//!
//! ## Why an OS lock
//! `.ultnas-lock` is held with `File::try_lock` (flock on Unix, LockFileEx on
//! Windows), not by the file's mere existence. The kernel releases the lock
//! when the owning process exits for any reason, including a crash or
//! SIGKILL, so a stale file can never block startup and a live owner can
//! never be overwritten.
//!
//! ## The file is never deleted
//! Unlinking a locked file lets a waiting process lock the orphaned inode
//! while a third creates and locks a fresh one: two owners. The file stays;
//! only the lock comes and goes. Its contents (the owner PID) are advisory,
//! for humans and error messages only.

use anyhow::{bail, Context, Result};
use std::{
    fs::{File, OpenOptions, TryLockError},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub const LOCK_FILE: &str = ".ultnas-lock";

/// Held for the daemon's lifetime; dropping it releases the lock.
pub struct VaultLock {
    _file: File,
    path: PathBuf,
}

impl VaultLock {
    /// Take the vault lock without waiting. Fails if another process holds it.
    pub fn acquire(vault_root: &Path) -> Result<Self> {
        let path = vault_root.join(LOCK_FILE);
        // No truncate: the current owner's PID must survive our failed attempt.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening vault lock {}", path.display()))?;

        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                // Unix locks are advisory, so the PID is readable. Windows
                // locks are mandatory and the read fails; say so generically.
                let mut owner = String::new();
                let owner = match file.read_to_string(&mut owner) {
                    Ok(_) if !owner.trim().is_empty() => format!("PID {}", owner.trim()),
                    _ => "another process".to_string(),
                };
                bail!(
                    "vault {} is already in use by {owner} (lock: {})",
                    vault_root.display(),
                    path.display()
                );
            }
            Err(TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", path.display()))
            }
        }

        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        write!(file, "{}", std::process::id())?;
        file.sync_all()?;

        Ok(Self { _file: file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn second_acquire_fails_while_first_is_held() {
        let dir = TempDir::new().unwrap();
        let _held = VaultLock::acquire(dir.path()).unwrap();

        let err = VaultLock::acquire(dir.path()).err().expect("must fail");
        assert!(err.to_string().contains("already in use"), "{err}");
    }

    #[test]
    fn failed_acquire_preserves_owner_pid() {
        let dir = TempDir::new().unwrap();
        let _held = VaultLock::acquire(dir.path()).unwrap();
        let _ = VaultLock::acquire(dir.path());
        drop(_held);

        let pid = std::fs::read_to_string(dir.path().join(LOCK_FILE)).unwrap();
        assert_eq!(pid, std::process::id().to_string());
    }

    #[test]
    fn released_on_drop() {
        let dir = TempDir::new().unwrap();
        drop(VaultLock::acquire(dir.path()).unwrap());
        VaultLock::acquire(dir.path()).unwrap();
    }

    #[test]
    fn stale_lock_file_does_not_block() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(LOCK_FILE), "999999").unwrap();

        let _lock = VaultLock::acquire(dir.path()).unwrap();
        let pid = std::fs::read_to_string(dir.path().join(LOCK_FILE));
        // Windows forbids reading a file another handle has locked.
        if let Ok(pid) = pid {
            assert_eq!(pid, std::process::id().to_string());
        }
    }
}
