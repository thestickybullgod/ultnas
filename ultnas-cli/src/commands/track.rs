//! `ultnas track | untrack | approve | tracked` — protect live text files.
//!
//! A tracked file is watched in place by the daemon. Writes that add
//! invisible characters are stripped, and after `write_violation_threshold`
//! attempts the file is deleted and recreated from its stable version. Clean
//! edits become the stable version automatically, or wait for `approve`,
//! per the policy's `approval` setting.

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::Args;
use std::path::{Path, PathBuf};
use ultnas_core::{
    canonical_path, invisible, read_live, rewrite_file, ContentId, Journal, JournalEntry,
    JournalOp, Live, NamespacePath, TrackedFile, Vault,
};

#[derive(Args)]
pub struct TrackArgs {
    /// Text file to protect
    pub file: PathBuf,
    /// Namespace for the file (quarantine applies per namespace)
    #[arg(long, default_value = "")]
    pub namespace: String,
    /// Track even though the file already contains invisible characters;
    /// they become part of the baseline and won't be stripped
    #[arg(long)]
    pub accept_existing: bool,
}

#[derive(Args)]
pub struct FileArg {
    /// Tracked file
    pub file: PathBuf,
}

pub fn track(vault_root: &Path, args: TrackArgs) -> Result<()> {
    let vault = Vault::open(vault_root)?;
    let path = canonical_path(&args.file)?;
    let content = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let Ok(text) = std::str::from_utf8(&content) else {
        bail!(
            "{} is not UTF-8 text — only text files can be tracked",
            path.display()
        );
    };

    let found = invisible::scan(text);
    if !found.is_empty() && !args.accept_existing {
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

    let namespace = NamespacePath::parse(&args.namespace)?;
    let id = vault.update_tracked(&path, |state| {
        if state.is_some() {
            return Ok(None);
        }
        let id = vault.write_version(&namespace, &path, &content)?;
        *state = Some(TrackedFile {
            path: path.clone(),
            namespace: namespace.clone(),
            stable: id,
            pending: None,
            updated_at: Utc::now(),
        });
        Ok(Some(id))
    })?;
    let Some(id) = id else {
        bail!("{} is already tracked", path.display());
    };

    journal(
        &vault,
        JournalOp::TrackFile,
        id,
        &namespace,
        &path,
        "tracked via CLI",
    )?;
    println!("✓ Tracking {}", path.display());
    println!("  stable version {}", short(&id));
    Ok(())
}

pub fn untrack(vault_root: &Path, args: FileArg) -> Result<()> {
    let vault = Vault::open(vault_root)?;
    let path = canonical_path(&args.file)?;
    let removed = vault.update_tracked(&path, |state| Ok(state.take()))?;
    let Some(t) = removed else {
        bail!("{} is not tracked", path.display());
    };
    journal(
        &vault,
        JournalOp::UntrackFile,
        t.stable,
        &t.namespace,
        &path,
        "untracked via CLI",
    )?;
    println!("✓ No longer tracking {}", path.display());
    println!("  Its versions stay in the vault.");
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
        t.stable,
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
    let tracked = vault.tracked_files()?;
    if tracked.is_empty() {
        println!("No tracked files. Add one with `ultnas track <file>`.");
        return Ok(());
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
        let ns = if t.namespace.is_root() {
            "(root)".to_string()
        } else {
            t.namespace.as_str()
        };
        println!("{}", t.path.display());
        println!(
            "  namespace {ns}  stable {}  status {status}",
            short(&t.stable)
        );
        if let Some(p) = t.pending {
            println!("  pending   {}  (`ultnas approve` to promote)", short(&p));
        }
    }
    Ok(())
}

fn journal(
    vault: &Vault,
    op: JournalOp,
    id: ContentId,
    namespace: &NamespacePath,
    path: &Path,
    detail: &str,
) -> Result<()> {
    let journal = Journal::open(&vault.root().join("journal.log"))?;
    journal.write(JournalEntry {
        ts: Utc::now(),
        op,
        id: Some(id),
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
