//! `ultnas track | untrack | approve | tracked` — protect live text files.
//!
//! A tracked file is watched in place by the daemon. Writes that add
//! invisible characters are stripped, and after `write_violation_threshold`
//! attempts the file is deleted and recreated from its stable version. Clean
//! edits become the stable version automatically, or wait for `approve`,
//! per the policy's `approval` setting.
//!
//! `track --recursive <dir>` tracks every text file under a directory, and
//! the daemon adopts files created there later.

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::Args;
use std::path::{Path, PathBuf};
use ultnas_core::{
    canonical_path, covering_dir, invisible, read_live, rewrite_file, ContentId, Journal,
    JournalEntry, JournalOp, Live, NamespacePath, TrackedDir, TrackedFile, Vault,
};

#[derive(Args)]
pub struct TrackArgs {
    /// Text file to protect (a directory with --recursive)
    pub file: PathBuf,
    /// Namespace for the file (quarantine applies per namespace)
    #[arg(long, default_value = "")]
    pub namespace: String,
    /// Track even though the file already contains invisible characters;
    /// they become part of the baseline and won't be stripped
    #[arg(long)]
    pub accept_existing: bool,
    /// Track every text file under a directory, including files created later
    #[arg(long, short = 'r')]
    pub recursive: bool,
    /// With --recursive: skip files and directories with this name anywhere
    /// below (repeatable, e.g. --exclude target --exclude node_modules)
    #[arg(long, requires = "recursive")]
    pub exclude: Vec<String>,
}

#[derive(Args)]
pub struct FileArg {
    /// Tracked file (or tracked directory, for untrack)
    pub file: PathBuf,
}

pub fn track(vault_root: &Path, args: TrackArgs) -> Result<()> {
    let vault = Vault::open(vault_root)?;
    let path = canonical_path(&args.file)?;
    let namespace = NamespacePath::parse(&args.namespace)?;
    if path.is_dir() {
        if !args.recursive {
            bail!(
                "{} is a directory — pass --recursive to track every text file in it",
                path.display()
            );
        }
        return track_dir(&vault, path, namespace, &args);
    }
    if args.recursive {
        bail!(
            "--recursive needs a directory, and {} isn't one",
            path.display()
        );
    }

    match track_one(&vault, &path, &namespace, None, args.accept_existing)? {
        Outcome::Tracked(id) => {
            println!("✓ Tracking {}", path.display());
            println!("  stable version {}", short(&id));
            Ok(())
        }
        Outcome::AlreadyTracked => bail!("{} is already tracked", path.display()),
        Outcome::NotText => bail!(
            "{} is not UTF-8 text — only text files can be tracked",
            path.display()
        ),
        Outcome::HasInvisible(found) => {
            eprintln!(
                "{} already contains {} invisible character(s):",
                path.display(),
                found.len()
            );
            for f in found.iter().take(10) {
                eprintln!("  {f}");
            }
            if found.len() > 10 {
                eprintln!("  … and {} more", found.len() - 10);
            }
            bail!("clean the file first, or pass --accept-existing to keep them in the baseline");
        }
    }
}

enum Outcome {
    Tracked(ContentId),
    AlreadyTracked,
    NotText,
    HasInvisible(Vec<invisible::Finding>),
}

fn track_one(
    vault: &Vault,
    path: &Path,
    namespace: &NamespacePath,
    source: Option<&Path>,
    accept_existing: bool,
) -> Result<Outcome> {
    let content = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let Ok(text) = std::str::from_utf8(&content) else {
        return Ok(Outcome::NotText);
    };
    let found = invisible::scan(text);
    if !found.is_empty() && !accept_existing {
        return Ok(Outcome::HasInvisible(found));
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
        return Ok(Outcome::AlreadyTracked);
    };
    journal(
        vault,
        JournalOp::TrackFile,
        Some(id),
        namespace,
        path,
        "tracked via CLI",
    )?;
    Ok(Outcome::Tracked(id))
}

fn track_dir(
    vault: &Vault,
    path: PathBuf,
    namespace: NamespacePath,
    args: &TrackArgs,
) -> Result<()> {
    if vault.tracked_dirs()?.iter().any(|d| d.path == path) {
        bail!("{} is already tracked", path.display());
    }
    let mut dir = TrackedDir {
        path: path.clone(),
        namespace: namespace.clone(),
        exclude: args.exclude.clone(),
        ignored: vec![],
        added_at: Utc::now(),
    };
    let vault_root = canonical_path(vault.root())?;
    if vault_root.starts_with(&path) {
        dir.ignored.push(vault_root);
    }

    // Sort files first, so the directory is recorded with its exceptions
    // before the daemon can see it and adopt (and strip) anything.
    let (mut clean, mut binary, mut dirty) = (vec![], 0usize, vec![]);
    for file in dir.candidates() {
        match std::fs::read(&file).map(String::from_utf8) {
            Ok(Ok(text)) => {
                let n = invisible::scan(&text).len();
                if n == 0 || args.accept_existing {
                    clean.push(file);
                } else {
                    dirty.push((file, n));
                }
            }
            Ok(Err(_)) => binary += 1,
            Err(e) => eprintln!("  skipping {}: {}", file.display(), e),
        }
    }
    dir.ignored.extend(dirty.iter().map(|(p, _)| p.clone()));
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
        "tracked directory via CLI",
    )?;

    // A running daemon may adopt some of these first; they count either way.
    let mut tracked = 0usize;
    for file in &clean {
        if let Outcome::Tracked(_) | Outcome::AlreadyTracked =
            track_one(vault, file, &namespace, Some(&path), true)?
        {
            tracked += 1;
        }
    }

    println!("✓ Tracking directory {}", path.display());
    println!("  {tracked} text file(s) tracked; new files will be adopted as they appear");
    if binary > 0 {
        println!("  {binary} binary file(s) skipped");
    }
    if !dirty.is_empty() {
        println!(
            "  {} file(s) already contain invisible characters and were NOT tracked:",
            dirty.len()
        );
        for (p, n) in &dirty {
            println!("    {} ({n})", p.display());
        }
        println!("  Clean them and `ultnas track` each, or re-run with --accept-existing.");
    }
    Ok(())
}

pub fn untrack(vault_root: &Path, args: FileArg) -> Result<()> {
    let vault = Vault::open(vault_root)?;
    let path = canonical_path(&args.file)?;

    if let Some(dir) = vault.update_tracked_dir(&path, |state| Ok(state.take()))? {
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
                    &vault,
                    JournalOp::UntrackFile,
                    Some(t.stable),
                    &t.namespace,
                    &t.path,
                    "untracked with its directory via CLI",
                )?;
            }
        }
        journal(
            &vault,
            JournalOp::UntrackFile,
            None,
            &dir.namespace,
            &path,
            "untracked directory via CLI",
        )?;
        println!("✓ No longer tracking directory {}", path.display());
        println!("  {n} file(s) untracked; their versions stay in the vault.");
        println!("  Files tracked on their own inside it are unaffected.");
        return Ok(());
    }

    let removed = vault.update_tracked(&path, |state| Ok(state.take()))?;
    // Keep a tracked directory from adopting it straight back.
    let dirs = vault.tracked_dirs()?;
    let parent = covering_dir(&dirs, &path).map(|d| d.path.clone());
    if let Some(dir) = &parent {
        vault.update_tracked_dir(dir, |state| {
            if let Some(d) = state.as_mut() {
                d.ignored.push(path.clone());
            }
            Ok(())
        })?;
    }
    match (removed, parent) {
        (Some(t), dir) => {
            journal(
                &vault,
                JournalOp::UntrackFile,
                Some(t.stable),
                &t.namespace,
                &path,
                "untracked via CLI",
            )?;
            println!("✓ No longer tracking {}", path.display());
            println!("  Its versions stay in the vault.");
            if let Some(dir) = dir {
                println!("  It won't be re-adopted from {}.", dir.display());
            }
        }
        (None, Some(dir)) => {
            println!(
                "✓ {} will no longer be adopted from {}",
                path.display(),
                dir.display()
            );
        }
        (None, None) => bail!("{} is not tracked", path.display()),
    }
    Ok(())
}

pub fn approve(vault_root: &Path, args: FileArg) -> Result<()> {
    let vault = Vault::open(vault_root)?;
    let path = canonical_path(&args.file)?;
    let approved = vault.update_tracked(&path, |state| {
        let Some(t) = state.as_mut() else {
            return Ok(Err(format!("{} is not tracked", path.display())));
        };
        let Some(pending) = t.pending.take() else {
            return Ok(Err(format!(
                "{} has no pending edit to approve",
                path.display()
            )));
        };
        // Fail before changing state if the pending copy is damaged.
        let content = vault.read_verified(&pending)?;
        t.stable = pending;
        t.updated_at = Utc::now();
        Ok(Ok((t.clone(), content)))
    })?;
    let (t, content) = match approved {
        Ok(v) => v,
        Err(msg) => bail!(msg),
    };

    journal(
        &vault,
        JournalOp::VersionApproved,
        Some(t.stable),
        &t.namespace,
        &path,
        "approved via CLI",
    )?;
    // The live file may have been restored to the old version since.
    let observed = read_live(&path)?.content_id();
    if observed != Some(t.stable) {
        rewrite_file(&path, &content, observed)
            .with_context(|| format!("{} was approved, but rewriting it failed", path.display()))?;
        println!("  Rewrote {} with the approved version.", path.display());
    }
    println!("✓ Approved {}", path.display());
    println!("  stable version {}", short(&t.stable));
    Ok(())
}

pub fn list(vault_root: &Path) -> Result<()> {
    let vault = Vault::open(vault_root)?;
    let dirs = vault.tracked_dirs()?;
    let tracked = vault.tracked_files()?;
    if tracked.is_empty() && dirs.is_empty() {
        println!("No tracked files. Add one with `ultnas track <file>`.");
        return Ok(());
    }
    for d in &dirs {
        let n = tracked
            .iter()
            .filter(|t| t.source.as_deref() == Some(d.path.as_path()))
            .count();
        println!("{} (directory)", d.path.display());
        print!("  namespace {}  {n} file(s)", ns_label(&d.namespace));
        if !d.exclude.is_empty() {
            print!("  excluding {}", d.exclude.join(", "));
        }
        println!();
    }
    for t in tracked {
        let status = match read_live(&t.path) {
            Err(e) => format!("unreadable: {e}"),
            Ok(Live::Missing) => "missing".to_string(),
            Ok(Live::NotRegular) => "not a regular file".to_string(),
            Ok(live) => {
                let id = live.content_id();
                if id == Some(t.stable) {
                    "ok".to_string()
                } else if id == t.pending {
                    "pending approval".to_string()
                } else {
                    "changed — the daemon will check it".to_string()
                }
            }
        };
        println!("{}", t.path.display());
        println!(
            "  namespace {}  stable {}  status {status}",
            ns_label(&t.namespace),
            short(&t.stable)
        );
        if let Some(p) = t.pending {
            println!("  pending   {}  (`ultnas approve` to promote)", short(&p));
        }
    }
    Ok(())
}

fn ns_label(ns: &NamespacePath) -> String {
    if ns.is_root() {
        "(root)".to_string()
    } else {
        ns.as_str()
    }
}

fn journal(
    vault: &Vault,
    op: JournalOp,
    id: Option<ContentId>,
    namespace: &NamespacePath,
    path: &Path,
    detail: &str,
) -> Result<()> {
    let journal = Journal::open(&vault.root().join("journal.log"))?;
    journal.write(JournalEntry {
        ts: Utc::now(),
        op,
        id,
        ns: Some(namespace.as_str()),
        label: Some("operator".into()),
        size: None,
        detail: Some(detail.into()),
        path: Some(path.to_path_buf()),
    })?;
    Ok(())
}

fn short(id: &ContentId) -> String {
    id.to_hex()[..12].to_string()
}
