//! Operator actions on tracked files: track, untrack, approve, list, lift a
//! quarantine.
//!
//! These are the operations behind `ultnas track | untrack | approve |
//! tracked | integrity lift-quarantine`, shared by every front end (the CLI
//! and the desktop app). They never print or prompt: each returns what
//! happened, and the caller decides how to say it or what to ask first.
//!
//! Every change is journaled with the [`Actor`] that made it, so the journal
//! says "tracked via CLI" or "tracked via GUI".

use crate::{
    canonical_path, check_trackable, covering_dir, device_of, invisible, read_live, rewrite_file,
    with_deferred_sync, ContentId, Journal, JournalEntry, JournalOp, Live, NamespacePath,
    TrackedDir, TrackedFile, UltnasCoreError, Vault,
};
use chrono::Utc;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

/// Directory sizes past which tracking a directory deserves a second look.
pub const CONFIRM_FILES: usize = 10_000;
pub const CONFIRM_BYTES: u64 = 1 << 30;

/// Which front end made a change; it ends each journal entry's detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    Cli,
    Gui,
}

impl Actor {
    fn detail(self, what: &str) -> String {
        let via = match self {
            Actor::Cli => "CLI",
            Actor::Gui => "GUI",
        };
        format!("{what} via {via}")
    }
}

/// The user's home directory, canonical.
pub fn home() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::fs::canonicalize(std::env::var_os(var)?).ok()
}

// ── Tracking one file ────────────────────────────────────────────────────────

/// What [`track_file`] did.
#[derive(Debug)]
pub enum TrackOutcome {
    Tracked(ContentId),
    AlreadyTracked,
    NotText,
    /// Nothing was tracked: the file already holds invisible characters and
    /// `accept_existing` was off.
    HasInvisible(Vec<invisible::Finding>),
}

/// Put one live text file under protection. `path` must be canonical.
pub fn track_file(
    vault: &Vault,
    path: &Path,
    namespace: &NamespacePath,
    accept_existing: bool,
    actor: Actor,
) -> Result<TrackOutcome, UltnasCoreError> {
    track_file_from(vault, path, namespace, None, accept_existing, actor)
}

/// [`track_file`], recording the tracked directory it came through.
pub fn track_file_from(
    vault: &Vault,
    path: &Path,
    namespace: &NamespacePath,
    source: Option<&Path>,
    accept_existing: bool,
    actor: Actor,
) -> Result<TrackOutcome, UltnasCoreError> {
    let content = std::fs::read(path).map_err(|source| UltnasCoreError::ReadLive {
        path: path.to_path_buf(),
        source,
    })?;
    let Ok(text) = std::str::from_utf8(&content) else {
        return Ok(TrackOutcome::NotText);
    };
    let found = invisible::scan(text);
    if !found.is_empty() && !accept_existing {
        return Ok(TrackOutcome::HasInvisible(found));
    }

    let id = vault.update_tracked(path, |state| {
        if state.is_some() {
            return Ok(None);
        }
        let id = vault.write_version(namespace, path, &content)?;
        *state = Some(TrackedFile {
            path: path.to_path_buf(),
            namespace: namespace.clone(),
            stable: id,
            pending: None,
            updated_at: Utc::now(),
            source: source.map(Path::to_path_buf),
        });
        Ok(Some(id))
    })?;
    let Some(id) = id else {
        return Ok(TrackOutcome::AlreadyTracked);
    };
    journal(
        vault,
        JournalOp::TrackFile,
        Some(id),
        namespace,
        path,
        &actor.detail("tracked"),
    )?;
    Ok(TrackOutcome::Tracked(id))
}

// ── Tracking a directory ─────────────────────────────────────────────────────

/// A directory about to be tracked, sized from metadata alone (no file has
/// been read yet). Pass it to [`track_dir`] to go ahead.
#[derive(Debug, Clone)]
pub struct DirPreview {
    pub dir: TrackedDir,
    /// Files the directory would track, before checking which are text.
    pub candidates: Vec<PathBuf>,
    pub bytes: u64,
    /// Why to ask before going ahead; empty when there's no need to.
    pub reasons: Vec<String>,
}

/// Size up tracking `path` (canonical) as a directory, skipping `exclude`d
/// names anywhere below. Fails if it is already tracked.
pub fn preview_dir(
    vault: &Vault,
    path: &Path,
    namespace: &NamespacePath,
    exclude: &[String],
) -> Result<DirPreview, UltnasCoreError> {
    check_trackable(path)?;
    if vault.tracked_dirs()?.iter().any(|d| d.path == path) {
        return Err(UltnasCoreError::AlreadyTracked(path.to_path_buf()));
    }
    let mut dir = TrackedDir {
        path: path.to_path_buf(),
        namespace: namespace.clone(),
        exclude: exclude.to_vec(),
        ignored: vec![],
        added_at: Utc::now(),
        device: std::fs::metadata(path).ok().as_ref().and_then(device_of),
    };
    let vault_root = canonical_path(vault.root())?;
    if vault_root.starts_with(path) {
        dir.ignored.push(vault_root);
    }

    let candidates = dir.candidates();
    let bytes: u64 = candidates
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    let reasons = needs_confirmation(path, candidates.len(), bytes);
    Ok(DirPreview {
        dir,
        candidates,
        bytes,
        reasons,
    })
}

/// Why tracking `path` deserves a second look, if it does.
fn needs_confirmation(path: &Path, files: usize, bytes: u64) -> Vec<String> {
    let mut why = vec![];
    if path.parent().is_none() {
        why.push("it is the root of the filesystem".to_string());
    } else if home().is_some_and(|h| !path.starts_with(h)) {
        why.push("it is outside your home directory".to_string());
    }
    if files > CONFIRM_FILES || bytes > CONFIRM_BYTES {
        why.push(format!(
            "it holds {files} candidate files ({} MiB); each is copied into the vault",
            bytes >> 20
        ));
    }
    why
}

/// Progress of [`track_dir`], in the order it happens.
#[derive(Debug)]
pub enum DirProgress<'a> {
    /// Read candidate `n` of `total` to sort text from binary and dirty.
    Reading {
        n: usize,
        total: usize,
    },
    /// A candidate couldn't be read and was skipped.
    Skipped {
        path: &'a Path,
        error: &'a std::io::Error,
    },
    /// Every candidate is sorted and the directory is recorded.
    ReadingDone {
        total: usize,
    },
    /// Tracked clean file `n` of `total`.
    Tracking {
        n: usize,
        total: usize,
    },
    TrackingDone {
        total: usize,
    },
}

/// What [`track_dir`] did.
#[derive(Debug, Default)]
pub struct DirReport {
    /// Text files now tracked (counting any a running daemon adopted first).
    pub tracked: usize,
    pub binary: usize,
    /// Files that already held invisible characters, with how many; they are
    /// not tracked, and the directory ignores them.
    pub dirty: Vec<(PathBuf, usize)>,
}

/// Track the previewed directory and every clean text file in it; the
/// daemon adopts files created there later.
pub fn track_dir(
    vault: &Vault,
    preview: DirPreview,
    accept_existing: bool,
    actor: Actor,
    progress: &mut dyn FnMut(DirProgress<'_>),
) -> Result<DirReport, UltnasCoreError> {
    let DirPreview {
        mut dir,
        candidates,
        ..
    } = preview;
    let path = dir.path.clone();
    let namespace = dir.namespace.clone();

    // Sort files first, so the directory is recorded with its exceptions
    // before the daemon can see it and adopt (and strip) anything.
    let total = candidates.len();
    let (mut clean, mut report) = (vec![], DirReport::default());
    for (i, file) in candidates.into_iter().enumerate() {
        progress(DirProgress::Reading { n: i + 1, total });
        match std::fs::read(&file).map(String::from_utf8) {
            Ok(Ok(text)) => {
                let n = invisible::scan(&text).len();
                if n == 0 || accept_existing {
                    clean.push(file);
                } else {
                    report.dirty.push((file, n));
                }
            }
            Ok(Err(_)) => report.binary += 1,
            Err(error) => progress(DirProgress::Skipped {
                path: &file,
                error: &error,
            }),
        }
    }
    dir.ignored
        .extend(report.dirty.iter().map(|(p, _)| p.clone()));
    vault.update_tracked_dir(&path, |state| {
        *state = Some(dir.clone());
        Ok(())
    })?;
    journal(
        vault,
        JournalOp::TrackFile,
        None,
        &namespace,
        &path,
        &actor.detail("tracked directory"),
    )?;
    progress(DirProgress::ReadingDone { total });

    // A running daemon may adopt some of these first; they count either way.
    // One flush at the end instead of several per file (see with_deferred_sync).
    let total = clean.len();
    report.tracked = with_deferred_sync(|| -> Result<usize, UltnasCoreError> {
        let mut tracked = 0usize;
        for (i, file) in clean.iter().enumerate() {
            progress(DirProgress::Tracking { n: i + 1, total });
            if let TrackOutcome::Tracked(_) | TrackOutcome::AlreadyTracked =
                track_file_from(vault, file, &namespace, Some(&path), true, actor)?
            {
                tracked += 1;
            }
        }
        Ok(tracked)
    })?;
    progress(DirProgress::TrackingDone { total });
    Ok(report)
}

// ── Untracking ───────────────────────────────────────────────────────────────

/// What [`untrack`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum UntrackOutcome {
    /// A tracked directory, and this many files tracked through it.
    Dir {
        files: usize,
    },
    /// A tracked file. If a tracked directory covers it, that directory now
    /// ignores it so it isn't adopted straight back.
    File {
        kept_out_of: Option<PathBuf>,
    },
    /// Not tracked itself, but now ignored by the directory that covers it.
    KeptOutOf(PathBuf),
    NotTracked,
}

/// Stop protecting a file or directory (`path` canonical). Its versions
/// stay in the vault.
pub fn untrack(
    vault: &Vault,
    path: &Path,
    actor: Actor,
) -> Result<UntrackOutcome, UltnasCoreError> {
    if let Some(dir) = vault.update_tracked_dir(path, |state| Ok(state.take()))? {
        let mut n = 0usize;
        for t in vault.tracked_files()? {
            if t.source.as_deref() != Some(dir.path.as_path()) {
                continue;
            }
            let removed = vault.update_tracked(&t.path, |state| {
                Ok(state
                    .take_if(|s| s.source.as_deref() == Some(dir.path.as_path()))
                    .is_some())
            })?;
            if removed {
                n += 1;
                journal(
                    vault,
                    JournalOp::UntrackFile,
                    Some(t.stable),
                    &t.namespace,
                    &t.path,
                    &actor.detail("untracked with its directory"),
                )?;
            }
        }
        journal(
            vault,
            JournalOp::UntrackFile,
            None,
            &dir.namespace,
            path,
            &actor.detail("untracked directory"),
        )?;
        return Ok(UntrackOutcome::Dir { files: n });
    }

    let removed = vault.update_tracked(path, |state| Ok(state.take()))?;
    // Keep a tracked directory from adopting it straight back.
    let dirs = vault.tracked_dirs()?;
    let parent = covering_dir(&dirs, path).map(|d| d.path.clone());
    if let Some(dir) = &parent {
        vault.update_tracked_dir(dir, |state| {
            if let Some(d) = state.as_mut() {
                d.ignored.push(path.to_path_buf());
            }
            Ok(())
        })?;
    }
    Ok(match (removed, parent) {
        (Some(t), dir) => {
            journal(
                vault,
                JournalOp::UntrackFile,
                Some(t.stable),
                &t.namespace,
                path,
                &actor.detail("untracked"),
            )?;
            UntrackOutcome::File { kept_out_of: dir }
        }
        (None, Some(dir)) => UntrackOutcome::KeptOutOf(dir),
        (None, None) => UntrackOutcome::NotTracked,
    })
}

// ── Approving ────────────────────────────────────────────────────────────────

/// What [`approve`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum ApproveOutcome {
    Approved {
        stable: ContentId,
        /// The live file had been restored to the old version since, so it
        /// was rewritten with the approved one.
        rewrote: bool,
    },
    NotTracked,
    NoPending,
}

/// Promote a tracked file's pending edit to its stable version (`path`
/// canonical).
pub fn approve(
    vault: &Vault,
    path: &Path,
    actor: Actor,
) -> Result<ApproveOutcome, UltnasCoreError> {
    let approved = vault.update_tracked(path, |state| {
        let Some(t) = state.as_mut() else {
            return Ok(Err(ApproveOutcome::NotTracked));
        };
        let Some(pending) = t.pending.take() else {
            return Ok(Err(ApproveOutcome::NoPending));
        };
        // Fail before changing state if the pending copy is damaged.
        let content = vault.read_verified(&pending)?;
        t.stable = pending;
        t.updated_at = Utc::now();
        Ok(Ok((t.clone(), content)))
    })?;
    let (t, content) = match approved {
        Ok(v) => v,
        Err(outcome) => return Ok(outcome),
    };

    journal(
        vault,
        JournalOp::VersionApproved,
        Some(t.stable),
        &t.namespace,
        path,
        &actor.detail("approved"),
    )?;
    // The live file may have been restored to the old version since.
    let observed = read_live(path)?.content_id();
    let rewrote = observed != Some(t.stable);
    if rewrote {
        rewrite_file(path, &content, observed).map_err(|e| {
            UltnasCoreError::RewriteAfterApprove {
                path: path.to_path_buf(),
                source: Box::new(e),
            }
        })?;
    }
    Ok(ApproveOutcome::Approved {
        stable: t.stable,
        rewrote,
    })
}

/// A pending edit's stable and pending text, for showing what changed.
/// `None` when the file isn't tracked or has nothing pending. Text that
/// isn't valid UTF-8 is shown with replacement characters.
pub fn pending_texts(
    vault: &Vault,
    path: &Path,
) -> Result<Option<(String, String)>, UltnasCoreError> {
    let Some(t) = vault.tracked_files()?.into_iter().find(|t| t.path == path) else {
        return Ok(None);
    };
    let Some(pending) = t.pending else {
        return Ok(None);
    };
    let text = |id: &ContentId| -> Result<String, UltnasCoreError> {
        Ok(String::from_utf8_lossy(&vault.read_verified(id)?).into_owned())
    };
    Ok(Some((text(&t.stable)?, text(&pending)?)))
}

// ── Listing ──────────────────────────────────────────────────────────────────

/// How a tracked file's live copy compares with its versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileStatus {
    /// Matches the stable version.
    Ok,
    /// Matches the pending edit, waiting for approval.
    Pending,
    /// Differs from both; the daemon will check it.
    Changed,
    Missing,
    NotRegular,
    Unreadable(String),
}

impl FileStatus {
    pub fn label(&self) -> String {
        match self {
            FileStatus::Ok => "ok".to_string(),
            FileStatus::Pending => "pending approval".to_string(),
            FileStatus::Changed => "changed — the daemon will check it".to_string(),
            FileStatus::Missing => "missing".to_string(),
            FileStatus::NotRegular => "not a regular file".to_string(),
            FileStatus::Unreadable(e) => format!("unreadable: {e}"),
        }
    }
}

/// Everything tracked, with each file's live status.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Overview {
    /// Each tracked directory with how many files are tracked through it.
    pub dirs: Vec<(TrackedDir, usize)>,
    pub files: Vec<(TrackedFile, FileStatus)>,
}

pub fn tracked_overview(vault: &Vault) -> Result<Overview, UltnasCoreError> {
    let dirs = vault.tracked_dirs()?;
    let files = vault.tracked_files()?;
    let dirs = dirs
        .into_iter()
        .map(|d| {
            let n = files
                .iter()
                .filter(|t| t.source.as_deref() == Some(d.path.as_path()))
                .count();
            (d, n)
        })
        .collect();
    let files = files
        .into_iter()
        .map(|t| {
            let status = file_status(&t);
            (t, status)
        })
        .collect();
    Ok(Overview { dirs, files })
}

fn file_status(t: &TrackedFile) -> FileStatus {
    match read_live(&t.path) {
        Err(e) => FileStatus::Unreadable(e.to_string()),
        Ok(Live::Missing) => FileStatus::Missing,
        Ok(Live::NotRegular) => FileStatus::NotRegular,
        Ok(live) => {
            let id = live.content_id();
            if id == Some(t.stable) {
                FileStatus::Ok
            } else if id == t.pending {
                FileStatus::Pending
            } else {
                FileStatus::Changed
            }
        }
    }
}

// ── Quarantine ───────────────────────────────────────────────────────────────

/// How to show a namespace to people: the root namespace's name is empty.
pub fn namespace_label(ns: &str) -> &str {
    if ns.is_empty() {
        ROOT_LABEL
    } else {
        ns
    }
}

/// What the root namespace is called on screen; also accepted as input.
pub const ROOT_LABEL: &str = "(root)";

/// A namespace as typed by a person: [`ROOT_LABEL`] or `""` is the root.
pub fn parse_namespace(s: &str) -> Result<NamespacePath, UltnasCoreError> {
    if s == ROOT_LABEL {
        return Ok(NamespacePath::root());
    }
    NamespacePath::parse(s)
}

/// Namespaces quarantined now, or `None` if the vault has no journal yet
/// (so nothing can be).
pub fn quarantined(vault_root: &Path) -> Result<Option<BTreeSet<String>>, UltnasCoreError> {
    let path = vault_root.join("journal.log");
    if !path.exists() {
        return Ok(None);
    }
    Journal::open_shared(&path)
        .quarantined_namespaces()
        .map(Some)
}

/// Record that the operator lifted `namespace`'s quarantine. A running
/// daemon applies it as soon as it sees the journal change. Check
/// [`quarantined`] first: lifting a namespace that isn't quarantined is
/// harmless but pointless.
pub fn lift_quarantine(
    vault_root: &Path,
    namespace: &NamespacePath,
    actor: Actor,
) -> Result<(), UltnasCoreError> {
    Journal::open_shared(&vault_root.join("journal.log")).write(JournalEntry {
        ts: Utc::now(),
        op: JournalOp::QuarantineLift,
        id: None,
        ns: Some(namespace.as_str()),
        label: Some("operator".into()),
        size: None,
        detail: Some(actor.detail("quarantine lifted")),
        path: None,
    })
}

fn journal(
    vault: &Vault,
    op: JournalOp,
    id: Option<ContentId>,
    namespace: &NamespacePath,
    path: &Path,
    detail: &str,
) -> Result<(), UltnasCoreError> {
    let journal = Journal::open_shared(&vault.root().join("journal.log"));
    journal.write(JournalEntry {
        ts: Utc::now(),
        op,
        id,
        ns: Some(namespace.as_str()),
        label: Some("operator".into()),
        size: None,
        detail: Some(detail.into()),
        path: Some(path.to_path_buf()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    const ZWSP: char = '\u{200B}';

    struct Env {
        _dir: TempDir,
        root: PathBuf,
        vault: Vault,
    }

    fn env() -> Env {
        let dir = TempDir::new().unwrap();
        let root = canonical_path(dir.path()).unwrap();
        let vault = Vault::init(&root.join("vault"), "t").unwrap();
        Env {
            _dir: dir,
            root,
            vault,
        }
    }

    fn file(env: &Env, name: &str, text: &str) -> PathBuf {
        let p = env.root.join(name);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, text).unwrap();
        canonical_path(&p).unwrap()
    }

    fn root_ns() -> NamespacePath {
        NamespacePath::parse("").unwrap()
    }

    fn journal_details(env: &Env) -> Vec<String> {
        Journal::open_shared(&env.vault.root().join("journal.log"))
            .iter()
            .unwrap()
            .flatten()
            .filter_map(|e| e.detail)
            .filter(|d| d.contains(" via "))
            .collect()
    }

    #[test]
    fn root_and_large_directories_need_confirmation() {
        let root = if cfg!(windows) { r"C:\" } else { "/" };
        let why = needs_confirmation(Path::new(root), 1, 1);
        assert!(why.iter().any(|w| w.contains("root")), "{why:?}");

        let big = needs_confirmation(Path::new(root).join("x").as_path(), CONFIRM_FILES + 1, 0);
        assert!(big.iter().any(|w| w.contains("candidate files")), "{big:?}");
        let heavy = needs_confirmation(Path::new(root).join("x").as_path(), 1, CONFIRM_BYTES + 1);
        assert!(
            heavy.iter().any(|w| w.contains("candidate files")),
            "{heavy:?}"
        );
    }

    #[test]
    fn small_directory_in_home_needs_no_confirmation() {
        let Some(home) = home() else { return };
        assert!(needs_confirmation(&home.join("project"), 10, 1024).is_empty());
    }

    #[test]
    fn tracking_a_file_reports_each_outcome() {
        let env = env();
        let ns = root_ns();
        let clean = file(&env, "clean.txt", "hello\n");
        let dirty = file(&env, "dirty.txt", &format!("a{ZWSP}b\n"));
        let binary = env.root.join("bin.dat");
        fs::write(&binary, [0xff, 0xfe, 0x00]).unwrap();

        assert!(matches!(
            track_file(&env.vault, &clean, &ns, false, Actor::Gui).unwrap(),
            TrackOutcome::Tracked(_)
        ));
        assert!(matches!(
            track_file(&env.vault, &clean, &ns, false, Actor::Gui).unwrap(),
            TrackOutcome::AlreadyTracked
        ));
        assert!(matches!(
            track_file(&env.vault, &binary, &ns, false, Actor::Gui).unwrap(),
            TrackOutcome::NotText
        ));
        match track_file(&env.vault, &dirty, &ns, false, Actor::Gui).unwrap() {
            TrackOutcome::HasInvisible(found) => assert_eq!(found.len(), 1),
            other => panic!("expected HasInvisible, got {other:?}"),
        }
        assert!(matches!(
            track_file(&env.vault, &dirty, &ns, true, Actor::Gui).unwrap(),
            TrackOutcome::Tracked(_)
        ));
        assert_eq!(
            journal_details(&env),
            ["tracked via GUI", "tracked via GUI"]
        );
    }

    #[test]
    fn tracking_a_directory_sorts_and_reports_progress() {
        let env = env();
        file(&env, "proj/a.txt", "one\n");
        file(&env, "proj/b.txt", "two\n");
        let dirty = file(&env, "proj/c.txt", &format!("x{ZWSP}\n"));
        fs::write(env.root.join("proj/d.bin"), [0xff, 0x00]).unwrap();
        let dir = canonical_path(&env.root.join("proj")).unwrap();

        let preview = preview_dir(&env.vault, &dir, &root_ns(), &[]).unwrap();
        assert_eq!(preview.candidates.len(), 4);
        let mut seen = vec![];
        let report = track_dir(&env.vault, preview, false, Actor::Cli, &mut |p| {
            seen.push(match p {
                DirProgress::Reading { .. } => "reading",
                DirProgress::Skipped { .. } => "skipped",
                DirProgress::ReadingDone { .. } => "read",
                DirProgress::Tracking { .. } => "tracking",
                DirProgress::TrackingDone { .. } => "tracked",
            })
        })
        .unwrap();
        assert_eq!(report.tracked, 2);
        assert_eq!(report.binary, 1);
        assert_eq!(report.dirty, vec![(dirty.clone(), 1)]);
        assert_eq!(
            seen,
            [
                "reading", "reading", "reading", "reading", "read", "tracking", "tracking",
                "tracked"
            ]
        );
        let dirs = env.vault.tracked_dirs().unwrap();
        assert!(dirs[0].ignored.contains(&dirty));

        assert!(matches!(
            preview_dir(&env.vault, &dir, &root_ns(), &[]),
            Err(UltnasCoreError::AlreadyTracked(_))
        ));
    }

    #[test]
    fn untracking_covers_directories_files_and_strays() {
        let env = env();
        let a = file(&env, "proj/a.txt", "one\n");
        let b = file(&env, "proj/b.txt", "two\n");
        let lone = file(&env, "lone.txt", "solo\n");
        let dir = canonical_path(&env.root.join("proj")).unwrap();
        let preview = preview_dir(&env.vault, &dir, &root_ns(), &[]).unwrap();
        track_dir(&env.vault, preview, false, Actor::Cli, &mut |_| {}).unwrap();
        track_file(&env.vault, &lone, &root_ns(), false, Actor::Cli).unwrap();

        assert_eq!(
            untrack(&env.vault, &a, Actor::Cli).unwrap(),
            UntrackOutcome::File {
                kept_out_of: Some(dir.clone())
            }
        );
        // Now ignored by the directory, so there's nothing left to undo.
        assert_eq!(
            untrack(&env.vault, &a, Actor::Cli).unwrap(),
            UntrackOutcome::NotTracked
        );
        // Created after tracking and not adopted yet (no daemon here).
        let new = file(&env, "proj/new.txt", "fresh\n");
        assert_eq!(
            untrack(&env.vault, &new, Actor::Cli).unwrap(),
            UntrackOutcome::KeptOutOf(dir.clone())
        );
        assert_eq!(
            untrack(&env.vault, &dir, Actor::Cli).unwrap(),
            UntrackOutcome::Dir { files: 1 }
        );
        assert_eq!(
            untrack(&env.vault, &b, Actor::Cli).unwrap(),
            UntrackOutcome::NotTracked
        );
        assert_eq!(
            untrack(&env.vault, &lone, Actor::Cli).unwrap(),
            UntrackOutcome::File { kept_out_of: None }
        );
        assert!(env.vault.tracked_files().unwrap().is_empty());
    }

    #[test]
    fn approving_promotes_the_pending_edit_and_rewrites_if_needed() {
        let env = env();
        let ns = root_ns();
        let live = file(&env, "notes.txt", "old\n");
        track_file(&env.vault, &live, &ns, false, Actor::Gui).unwrap();
        assert_eq!(
            approve(&env.vault, &live, Actor::Gui).unwrap(),
            ApproveOutcome::NoPending
        );
        assert_eq!(
            approve(&env.vault, &env.root.join("nope.txt"), Actor::Gui).unwrap(),
            ApproveOutcome::NotTracked
        );
        assert_eq!(pending_texts(&env.vault, &live).unwrap(), None);

        // What the daemon does for a clean edit in approval mode.
        let pending = env.vault.write_version(&ns, &live, b"new\n").unwrap();
        env.vault
            .update_tracked(&live, |t| {
                t.as_mut().unwrap().pending = Some(pending);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            pending_texts(&env.vault, &live).unwrap(),
            Some(("old\n".to_string(), "new\n".to_string()))
        );
        let overview = tracked_overview(&env.vault).unwrap();
        assert_eq!(overview.files[0].1, FileStatus::Ok);

        // The live file still holds the old text, so approving rewrites it.
        assert_eq!(
            approve(&env.vault, &live, Actor::Gui).unwrap(),
            ApproveOutcome::Approved {
                stable: pending,
                rewrote: true
            }
        );
        assert_eq!(fs::read_to_string(&live).unwrap(), "new\n");
        assert_eq!(
            tracked_overview(&env.vault).unwrap().files[0].1,
            FileStatus::Ok
        );
    }

    #[test]
    fn overview_reports_live_status() {
        let env = env();
        let live = file(&env, "notes.txt", "hello\n");
        track_file(&env.vault, &live, &root_ns(), false, Actor::Cli).unwrap();
        fs::write(&live, "edited\n").unwrap();
        assert_eq!(
            tracked_overview(&env.vault).unwrap().files[0].1,
            FileStatus::Changed
        );
        fs::remove_file(&live).unwrap();
        assert_eq!(
            tracked_overview(&env.vault).unwrap().files[0].1,
            FileStatus::Missing
        );
    }

    #[test]
    fn the_root_namespace_has_a_label() {
        assert_eq!(namespace_label(""), "(root)");
        assert_eq!(namespace_label("etc"), "etc");
        assert!(parse_namespace("(root)").unwrap().is_root());
        assert!(parse_namespace("").unwrap().is_root());
        assert_eq!(parse_namespace("etc").unwrap().as_str(), "etc");
    }

    #[test]
    fn lifting_a_quarantine_is_journaled() {
        let env = env();
        let root = env.vault.root().to_path_buf();
        let ns = NamespacePath::parse("etc").unwrap();
        Journal::open_shared(&root.join("journal.log"))
            .write(JournalEntry {
                ts: Utc::now(),
                op: JournalOp::IntegrityEscalate,
                id: None,
                ns: Some(ns.as_str()),
                label: None,
                size: None,
                detail: None,
                path: None,
            })
            .unwrap();
        assert!(quarantined(&root).unwrap().unwrap().contains("etc"));
        lift_quarantine(&root, &ns, Actor::Gui).unwrap();
        assert!(quarantined(&root).unwrap().unwrap().is_empty());
        assert!(journal_details(&env).contains(&"quarantine lifted via GUI".to_string()));
    }
}
