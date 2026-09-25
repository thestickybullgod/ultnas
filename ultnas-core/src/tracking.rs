//! Tracked files: live text files outside the vault, protected in place.
//!
//! Each tracked file has a *stable* version (the copy restores come from)
//! and, when approval is required, an optional *pending* version: a clean
//! edit that is on disk but not yet approved. Both are ordinary content
//! objects in the vault, so the vault is an independent restore source for
//! the live file.
//!
//! A *tracked directory* ([`TrackedDir`]) tracks every text file under it,
//! including files created later, which the daemon adopts. Hidden names,
//! editor scratch files, excluded names, and explicitly untracked paths are
//! skipped ([`TrackedDir::covers`]), and so is anything on a different
//! filesystem from the directory itself: tracking `/` covers the root
//! filesystem only, never `/proc`, `/sys`, or other mounts below it.
//!
//! Kernel pseudo-filesystems can't be tracked at all ([`check_trackable`]):
//! their "files" are live kernel state, and repairing one would mean
//! writing a kernel setting.
//!
//! State lives in `tracked/<blake3 of path>.json` (files) and
//! `tracked/<blake3 of path>.tdir` (directories). The CLI (track, approve)
//! and the daemon (accept, mark pending, adopt) both change it, so every
//! change is a read-modify-write under an OS lock on `tracked/.lock`
//! ([`Vault::update_tracked`], [`Vault::update_tracked_dir`]).

use crate::{
    hash_bytes, vault::atomic_write, ContentId, NamespacePath, RecordBuilder, UltnasCoreError,
    Vault,
};
use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    fs::{self, Metadata, OpenOptions},
    io::ErrorKind,
    path::{Path, PathBuf},
};
use walkdir::{DirEntry, WalkDir};

/// Largest file a tracked directory adopts.
pub const MAX_ADOPT_BYTES: u64 = 16 * 1024 * 1024;

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
    /// The tracked directory this file was tracked through, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PathBuf>,
}

impl TrackedFile {
    /// The version new writes are compared against: the newest clean one.
    pub fn baseline(&self) -> ContentId {
        self.pending.unwrap_or(self.stable)
    }
}

/// A directory whose text files are all tracked, including new ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackedDir {
    /// Canonical absolute path.
    pub path: PathBuf,
    pub namespace: NamespacePath,
    /// File or directory names skipped anywhere below (e.g. `target`).
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Paths skipped with everything under them: files untracked one by
    /// one, files that already had invisible characters, a vault inside.
    #[serde(default)]
    pub ignored: Vec<PathBuf>,
    pub added_at: DateTime<Utc>,
    /// Device id of the directory's filesystem (Unix). Nothing on another
    /// filesystem is adopted.
    #[serde(default)]
    pub device: Option<u64>,
}

impl TrackedDir {
    /// Whether this directory tracks `path`: it is below it, and neither it
    /// nor any directory between is hidden, excluded, or ignored, and it
    /// isn't an editor scratch file.
    pub fn covers(&self, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(&self.path) else {
            return false;
        };
        if rel.as_os_str().is_empty() || self.ignored.iter().any(|i| path.starts_with(i)) {
            return false;
        }
        let names: Vec<_> = rel.iter().map(|c| c.to_string_lossy()).collect();
        let Some(file) = names.last() else {
            return false;
        };
        !names.iter().any(|n| self.skips_name(n)) && !is_scratch(file)
    }

    fn skips_name(&self, name: &str) -> bool {
        name.starts_with('.') || self.exclude.iter().any(|e| e == name)
    }

    /// Whether a file with this metadata is on the directory's filesystem.
    pub fn same_filesystem(&self, meta: &Metadata) -> bool {
        match (self.device, device_of(meta)) {
            (Some(dir), Some(file)) => dir == file,
            _ => true,
        }
    }

    /// Entries under `start` (this directory or one inside it) that the
    /// directory doesn't skip, staying on `start`'s filesystem and never
    /// following links.
    fn walk(&self, start: &Path) -> impl Iterator<Item = DirEntry> + '_ {
        WalkDir::new(start)
            .follow_links(false)
            .same_file_system(true)
            .into_iter()
            .filter_entry(move |e| {
                e.depth() == 0
                    || (!self.skips_name(&e.file_name().to_string_lossy())
                        && !self.ignored.iter().any(|i| e.path().starts_with(i)))
            })
            .filter_map(Result::ok)
    }

    /// Regular files now under the directory that it covers, up to
    /// [`MAX_ADOPT_BYTES`]. Symbolic links are neither followed nor listed.
    pub fn candidates(&self) -> Vec<PathBuf> {
        self.candidates_in(&self.path)
    }

    /// [`TrackedDir::candidates`], limited to the subtree at `start`.
    pub fn candidates_in(&self, start: &Path) -> Vec<PathBuf> {
        self.walk(start)
            .filter(|e| e.file_type().is_file())
            .filter(|e| e.metadata().is_ok_and(|m| m.len() <= MAX_ADOPT_BYTES))
            .map(|e| e.into_path())
            .filter(|p| self.covers(p))
            .collect()
    }

    /// Directories at and under `start` that files could be adopted from:
    /// what the daemon watches for this tracked directory.
    pub fn subdirs(&self, start: &Path) -> Vec<PathBuf> {
        self.walk(start)
            .filter(|e| e.file_type().is_dir())
            .map(|e| e.into_path())
            .collect()
    }
}

/// Device id of the filesystem holding a file (Unix only).
pub fn device_of(meta: &Metadata) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(meta.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// Refuse paths on kernel pseudo-filesystems (`/proc`, `/sys`, `/dev`,
/// `/run`, and on Linux anything whose filesystem type is one of them,
/// wherever it is mounted). Their files are live kernel state: reads can
/// block or change, and a write is a kernel setting, not an edit.
pub fn check_trackable(path: &Path) -> Result<(), UltnasCoreError> {
    let refuse = |reason: String| {
        Err(UltnasCoreError::Untrackable {
            path: path.to_path_buf(),
            reason,
        })
    };
    #[cfg(unix)]
    for root in ["/proc", "/sys", "/dev", "/run"] {
        if path.starts_with(root) {
            return refuse(format!("{root} holds kernel or runtime state, not files"));
        }
    }
    if let Some(fs) = pseudo_filesystem(path) {
        return refuse(format!("it is on a {fs} pseudo-filesystem"));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn pseudo_filesystem(path: &Path) -> Option<&'static str> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c` is a valid NUL-terminated path and `st` a writable statfs.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    // f_type's width varies by architecture; the magic numbers are 32-bit.
    #[allow(clippy::unnecessary_cast)]
    let magic = (st.f_type as u64) & 0xffff_ffff;
    Some(match magic {
        0x9fa0 => "proc",
        0x6265_6572 => "sysfs",
        0x1373 => "devfs",
        0x1cd1 => "devpts",
        0x0027_e0eb => "cgroup",
        0x6367_7270 => "cgroup2",
        0x6462_6720 => "debugfs",
        0x7363_6673 => "securityfs",
        0xcafe_4a11 => "bpf",
        0x7472_6163 => "tracefs",
        0x6265_6570 => "configfs",
        0xde5e_81e4 => "efivarfs",
        0x6165_676c => "pstore",
        _ => return None,
    })
}

#[cfg(not(target_os = "linux"))]
fn pseudo_filesystem(_path: &Path) -> Option<&'static str> {
    None
}

/// Editor and tool scratch files (including the daemon's own `*.tmp`),
/// which come and go and must never be adopted, let alone restored.
fn is_scratch(name: &str) -> bool {
    const SUFFIXES: [&str; 7] = ["~", ".swp", ".swo", ".swx", ".tmp", ".bak", ".orig"];
    SUFFIXES.iter().any(|s| name.ends_with(s)) || (name.starts_with('#') && name.ends_with('#'))
}

/// The most specific tracked directory that covers `path`.
pub fn covering_dir<'a>(dirs: &'a [TrackedDir], path: &Path) -> Option<&'a TrackedDir> {
    dirs.iter()
        .filter(|d| d.covers(path))
        .max_by_key(|d| d.path.components().count())
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

    fn tracked_entry_path(&self, path: &Path, ext: &str) -> PathBuf {
        let key = hash_bytes(path.to_string_lossy().as_bytes());
        self.tracked_dir().join(format!("{}.{ext}", key.to_hex()))
    }

    /// Every entry of type `S` stored with extension `ext`.
    fn tracked_entries<S: DeserializeOwned>(&self, ext: &str) -> Result<Vec<S>, UltnasCoreError> {
        let entries = match fs::read_dir(self.tracked_dir()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut out = vec![];
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != ext) {
                continue;
            }
            let Ok(raw) = fs::read(&path) else { continue };
            if let Ok(t) = serde_json::from_slice::<S>(&raw) {
                out.push(t);
            }
        }
        Ok(out)
    }

    /// Every tracked directory.
    pub fn tracked_dirs(&self) -> Result<Vec<TrackedDir>, UltnasCoreError> {
        let mut out: Vec<TrackedDir> = self.tracked_entries("tdir")?;
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// [`Vault::update_tracked`] for a tracked directory's state.
    pub fn update_tracked_dir<T>(
        &self,
        path: &Path,
        f: impl FnOnce(&mut Option<TrackedDir>) -> Result<T, UltnasCoreError>,
    ) -> Result<T, UltnasCoreError> {
        self.locked_update(&self.tracked_entry_path(path, "tdir"), f)
    }

    /// Run `f` holding the tracking lock, so no tracked file's state changes
    /// meanwhile. `f` must not call [`Vault::update_tracked`] (it would wait
    /// on the lock it holds).
    pub fn with_tracking_lock<T>(
        &self,
        f: impl FnOnce() -> Result<T, UltnasCoreError>,
    ) -> Result<T, UltnasCoreError> {
        let _lock = self.tracking_lock()?;
        f()
    }

    fn tracking_lock(&self) -> Result<fs::File, UltnasCoreError> {
        fs::create_dir_all(self.tracked_dir())?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.tracked_dir().join(".lock"))?;
        lock.lock()?;
        Ok(lock)
    }

    /// Read-modify-write one entry under the tracking lock.
    fn locked_update<S, T>(
        &self,
        entry_path: &Path,
        f: impl FnOnce(&mut Option<S>) -> Result<T, UltnasCoreError>,
    ) -> Result<T, UltnasCoreError>
    where
        S: Serialize + DeserializeOwned + Clone + PartialEq,
    {
        let lock = self.tracking_lock()?;

        let before = match fs::read(entry_path) {
            Ok(raw) => Some(
                serde_json::from_slice::<S>(&raw)
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
                    atomic_write(entry_path, &json)?;
                }
                None => fs::remove_file(entry_path)?,
            }
        }
        drop(lock);
        Ok(out)
    }

    /// Every tracked file. Vaults created before tracking existed have none.
    pub fn tracked_files(&self) -> Result<Vec<TrackedFile>, UltnasCoreError> {
        let mut out: Vec<TrackedFile> = self.tracked_entries("json")?;
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
        self.locked_update(&self.tracked_entry_path(path, "json"), f)
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
                    source: None,
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

    fn tdir(root: &Path) -> TrackedDir {
        TrackedDir {
            path: root.to_path_buf(),
            namespace: NamespacePath::parse("docs").unwrap(),
            exclude: vec!["target".into()],
            ignored: vec![root.join("vault")],
            added_at: Utc::now(),
            device: None,
        }
    }

    #[test]
    fn tracked_dir_covers_text_files_but_not_scratch_hidden_or_excluded() {
        let root = PathBuf::from("/w");
        let d = tdir(&root);
        for yes in ["a.txt", "src/main.rs", "deep/er/notes.md", "README"] {
            assert!(d.covers(&root.join(yes)), "{yes}");
        }
        for no in [
            ".env",
            ".git/config",
            "src/.hidden/x.rs",
            "target/debug/out.txt",
            "a.txt~",
            "a.txt.swp",
            "a.txt.1234.tmp",
            "#a.txt#",
            "vault/records/x.json",
        ] {
            assert!(!d.covers(&root.join(no)), "{no}");
        }
        assert!(!d.covers(&root), "the directory itself");
        assert!(!d.covers(Path::new("/elsewhere/a.txt")));
    }

    #[test]
    fn covering_dir_prefers_the_most_specific() {
        let outer = tdir(Path::new("/w"));
        let mut inner = tdir(Path::new("/w/sub"));
        inner.namespace = NamespacePath::parse("inner").unwrap();
        let dirs = vec![outer, inner];
        let hit = covering_dir(&dirs, Path::new("/w/sub/a.txt")).unwrap();
        assert_eq!(hit.namespace.as_str(), "inner");
        assert_eq!(
            covering_dir(&dirs, Path::new("/w/a.txt")).unwrap().path,
            PathBuf::from("/w")
        );
    }

    #[test]
    fn candidates_walk_skips_what_covers_rejects() {
        let dir = TempDir::new().unwrap();
        let root = canonical_path(dir.path()).unwrap();
        for f in ["a.txt", "src/b.rs", ".git/c", "target/d.txt", "e.swp"] {
            let p = root.join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, "x").unwrap();
        }
        let mut got: Vec<_> = tdir(&root)
            .candidates()
            .into_iter()
            .map(|p| p.strip_prefix(&root).unwrap().to_path_buf())
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![PathBuf::from("a.txt"), PathBuf::from("src").join("b.rs")]
        );
    }

    #[test]
    fn subdirs_skip_hidden_and_excluded_directories() {
        let dir = TempDir::new().unwrap();
        let root = canonical_path(dir.path()).unwrap();
        for d in ["src/deep", ".git/objects", "target/debug"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        let mut got: Vec<_> = tdir(&root)
            .subdirs(&root)
            .into_iter()
            .map(|p| p.strip_prefix(&root).unwrap().to_path_buf())
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                PathBuf::new(),
                PathBuf::from("src"),
                PathBuf::from("src").join("deep")
            ]
        );
    }

    #[test]
    fn ordinary_directories_are_trackable() {
        let dir = TempDir::new().unwrap();
        check_trackable(dir.path()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn kernel_pseudo_filesystems_are_not_trackable() {
        for p in [
            "/proc",
            "/proc/sys/kernel/hostname",
            "/sys/kernel",
            "/dev/shm/x",
            "/run/user",
        ] {
            assert!(
                matches!(
                    check_trackable(Path::new(p)),
                    Err(UltnasCoreError::Untrackable { .. })
                ),
                "{p}"
            );
        }
        check_trackable(Path::new("/")).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn procfs_is_recognised_by_type() {
        assert_eq!(pseudo_filesystem(Path::new("/proc/self")), Some("proc"));
    }

    #[test]
    fn tracked_dirs_roundtrip_separately_from_files() {
        let (dir, vault, live) = setup();
        track(&vault, &live);
        let d = tdir(&canonical_path(dir.path()).unwrap());
        vault
            .update_tracked_dir(&d.path, |s| {
                *s = Some(d.clone());
                Ok(())
            })
            .unwrap();
        assert_eq!(vault.tracked_dirs().unwrap(), vec![d]);
        assert_eq!(vault.tracked_files().unwrap().len(), 1);
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
