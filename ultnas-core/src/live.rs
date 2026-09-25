//! Reading and replacing live (tracked) files safely.
//!
//! The daemon may run with more privilege than whoever can write a tracked
//! file's directory, so it never follows a symbolic link at a tracked path:
//! a link planted there could point it at `/etc/shadow`, or at a FIFO that
//! blocks it forever. [`read_live`] opens without following links (and
//! without blocking) and reports anything that isn't a regular file as
//! [`Live::NotRegular`], which the daemon treats as a violation.
//!
//! Replacements are guarded against lost updates: [`rewrite_file`] and
//! [`recreate_file`] take the content id the caller *observed*, and refuse
//! with [`UltnasCoreError::ChangedDuringWrite`] if the file changed since —
//! checked once up front and again just before the new file is renamed in.

use crate::{hash_bytes, vault::atomic_write_as, ContentId, UltnasCoreError};
use std::{
    fs::{self, Metadata, OpenOptions},
    io::{self, ErrorKind, Read},
    path::Path,
};

/// What is at a tracked path right now.
#[derive(Debug)]
pub enum Live {
    Missing,
    /// A symbolic link, directory, FIFO, device, …
    NotRegular,
    File {
        content: Vec<u8>,
        meta: Metadata,
    },
}

impl Live {
    /// Content id of a regular file; `None` for anything else.
    pub fn content_id(&self) -> Option<ContentId> {
        match self {
            Live::File { content, .. } => Some(hash_bytes(content)),
            _ => None,
        }
    }
}

/// Open the file at `path` without following a final symbolic link.
pub fn read_live(path: &Path) -> io::Result<Live> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NONBLOCK: opening a FIFO must not wait for a writer.
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT: open a link itself, not its target.
        opts.custom_flags(0x0020_0000);
    }

    let mut file = match opts.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Live::Missing),
        // O_NOFOLLOW on a symbolic link.
        #[cfg(unix)]
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Ok(Live::NotRegular),
        Err(e) => return Err(e),
    };
    // Metadata of what was actually opened, so nothing can be swapped in
    // between a check and the read.
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Ok(Live::NotRegular);
    }
    let mut content = Vec::with_capacity(meta.len() as usize);
    file.read_to_end(&mut content)?;
    Ok(Live::File { content, meta })
}

fn ensure_unchanged(path: &Path, observed: Option<ContentId>) -> Result<(), UltnasCoreError> {
    if read_live(path)?.content_id() == observed {
        Ok(())
    } else {
        Err(UltnasCoreError::ChangedDuringWrite(path.to_path_buf()))
    }
}

/// Replace a live file's contents atomically, keeping its permissions, if
/// it still has content id `observed` (`None`: still missing or not a
/// regular file). Used to strip invisible characters in place.
pub fn rewrite_file(
    path: &Path,
    data: &[u8],
    observed: Option<ContentId>,
) -> Result<(), UltnasCoreError> {
    let live = read_live(path)?;
    if live.content_id() != observed {
        return Err(UltnasCoreError::ChangedDuringWrite(path.to_path_buf()));
    }
    let like = match &live {
        Live::File { meta, .. } => Some(meta),
        _ => None,
    };
    atomic_write_as(path, data, like, || ensure_unchanged(path, observed))
}

/// Delete what is at `path`, then create a fresh regular file from `data`,
/// if it still has content id `observed` (see [`rewrite_file`]).
///
/// The new file is a new inode, so a writer still holding the old one open
/// keeps writing into the deleted copy. A symbolic link is removed, not
/// followed, and its target's permissions are never copied.
pub fn recreate_file(
    path: &Path,
    data: &[u8],
    observed: Option<ContentId>,
) -> Result<(), UltnasCoreError> {
    let live = read_live(path)?;
    if live.content_id() != observed {
        return Err(UltnasCoreError::ChangedDuringWrite(path.to_path_buf()));
    }
    let like = match &live {
        Live::File { meta, .. } => Some(meta.clone()),
        _ => None,
    };
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    // Something created in the gap belongs to whoever made it.
    atomic_write_as(path, data, like.as_ref(), || match read_live(path)? {
        Live::Missing => Ok(()),
        _ => Err(UltnasCoreError::ChangedDuringWrite(path.to_path_buf())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn read_live_reports_missing_and_regular() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("f.txt");
        assert!(matches!(read_live(&p).unwrap(), Live::Missing));
        fs::write(&p, b"hi").unwrap();
        let live = read_live(&p).unwrap();
        assert_eq!(live.content_id(), Some(hash_bytes(b"hi")));
    }

    #[test]
    fn rewrite_and_recreate_replace_contents() {
        let dir = TempDir::new().unwrap();
        let live = dir.path().join("live.txt");
        fs::write(&live, b"old").unwrap();
        rewrite_file(&live, b"new", Some(hash_bytes(b"old"))).unwrap();
        assert_eq!(fs::read(&live).unwrap(), b"new");
        recreate_file(&live, b"fresh", Some(hash_bytes(b"new"))).unwrap();
        assert_eq!(fs::read(&live).unwrap(), b"fresh");
        fs::remove_file(&live).unwrap();
        recreate_file(&live, b"back", None).unwrap();
        assert_eq!(fs::read(&live).unwrap(), b"back");
    }

    #[test]
    fn stale_observation_is_refused_and_file_untouched() {
        let dir = TempDir::new().unwrap();
        let live = dir.path().join("live.txt");
        fs::write(&live, b"users new edit").unwrap();
        let stale = Some(hash_bytes(b"what the daemon saw"));
        for result in [
            rewrite_file(&live, b"x", stale),
            recreate_file(&live, b"x", stale),
            recreate_file(&live, b"x", None),
        ] {
            assert!(matches!(
                result,
                Err(UltnasCoreError::ChangedDuringWrite(_))
            ));
        }
        assert_eq!(fs::read(&live).unwrap(), b"users new edit");
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = TempDir::new().unwrap();
        let secret = dir.path().join("secret");
        fs::write(&secret, b"root only").unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        let live = dir.path().join("live.txt");
        symlink(&secret, &live).unwrap();

        assert!(matches!(read_live(&live).unwrap(), Live::NotRegular));
        recreate_file(&live, b"restored", None).unwrap();
        assert!(fs::symlink_metadata(&live).unwrap().is_file());
        assert_eq!(fs::read(&live).unwrap(), b"restored");
        assert_eq!(fs::read(&secret).unwrap(), b"root only", "target untouched");
        let mode = fs::metadata(&live).unwrap().permissions().mode() & 0o777;
        assert_ne!(mode, 0o600, "target's permissions must not be copied");
    }

    #[cfg(unix)]
    #[test]
    fn fifo_does_not_block() {
        let dir = TempDir::new().unwrap();
        let fifo = dir.path().join("pipe");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        assert!(matches!(read_live(&fifo).unwrap(), Live::NotRegular));
    }

    #[cfg(unix)]
    #[test]
    fn rewrite_and_recreate_keep_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let live = dir.path().join("live.txt");
        fs::write(&live, b"old").unwrap();
        fs::set_permissions(&live, fs::Permissions::from_mode(0o640)).unwrap();
        let mode = || fs::metadata(&live).unwrap().permissions().mode() & 0o777;

        rewrite_file(&live, b"new", Some(hash_bytes(b"old"))).unwrap();
        assert_eq!(mode(), 0o640);
        recreate_file(&live, b"fresh", Some(hash_bytes(b"new"))).unwrap();
        assert_eq!(mode(), 0o640);
    }
}
