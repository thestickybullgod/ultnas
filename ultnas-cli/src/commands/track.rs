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

use anyhow::{bail, Result};
use clap::Args;
use std::path::{Path, PathBuf};
use ultnas_core::{
    canonical_path, check_trackable,
    ops::{self, Actor, ApproveOutcome, DirProgress, UntrackOutcome},
    ContentId, NamespacePath, Vault,
};

pub(crate) use ultnas_core::ops::{home, TrackOutcome as Outcome};

use super::prompt::confirm;

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
    /// Don't ask for confirmation (tracking `/`, a directory outside your
    /// home, or a very large one)
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Args)]
pub struct FileArg {
    /// Tracked file (or tracked directory, for untrack)
    pub file: PathBuf,
}

pub fn track(vault_root: &Path, args: TrackArgs) -> Result<()> {
    let vault = Vault::open(vault_root)?;
    let path = canonical_path(&args.file)?;
    check_trackable(&path)?;
    let namespace = NamespacePath::parse(&args.namespace)?;
    if path.is_dir() {
        if !args.recursive {
            bail!(
                "{} is a directory — pass --recursive to track every text file in it",
                path.display()
            );
        }
        let opts = DirOptions {
            exclude: args.exclude.clone(),
            accept_existing: args.accept_existing,
            yes: args.yes,
        };
        return track_dir(&vault, path, namespace, &opts);
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

pub(crate) fn track_one(
    vault: &Vault,
    path: &Path,
    namespace: &NamespacePath,
    source: Option<&Path>,
    accept_existing: bool,
) -> Result<Outcome> {
    Ok(ops::track_file_from(
        vault,
        path,
        namespace,
        source,
        accept_existing,
        Actor::Cli,
    )?)
}

pub(crate) struct DirOptions {
    pub exclude: Vec<String>,
    pub accept_existing: bool,
    pub yes: bool,
}

pub(crate) fn track_dir(
    vault: &Vault,
    path: PathBuf,
    namespace: NamespacePath,
    opts: &DirOptions,
) -> Result<()> {
    // Preview from metadata alone, before reading any file.
    let preview = ops::preview_dir(vault, &path, &namespace, &opts.exclude)?;
    if !preview.reasons.is_empty() {
        println!(
            "About to track {} (up to {} files, {} MiB) because:",
            path.display(),
            preview.candidates.len(),
            preview.bytes >> 20
        );
        for w in &preview.reasons {
            println!("  - {w}");
        }
        println!("  Other filesystems mounted below it are not included.");
        if !confirm("Track it?", opts.yes)? {
            println!("Nothing tracked.");
            return Ok(());
        }
    }

    let mut progress = Progress::new("reading", preview.candidates.len());
    let report = ops::track_dir(
        vault,
        preview,
        opts.accept_existing,
        Actor::Cli,
        &mut |p| match p {
            DirProgress::Reading { .. } => progress.tick(),
            DirProgress::Skipped { path, error } => {
                eprintln!("  skipping {}: {}", path.display(), error)
            }
            DirProgress::ReadingDone { .. } => progress.done(),
            DirProgress::Tracking { n, total } => {
                if n == 1 {
                    progress = Progress::new("tracking", total);
                }
                progress.tick();
            }
            DirProgress::TrackingDone { .. } => progress.done(),
        },
    )?;
    println!("✓ Tracking directory {}", path.display());
    println!(
        "  {} text file(s) tracked; new files will be adopted as they appear",
        report.tracked
    );
    if report.binary > 0 {
        println!("  {} binary file(s) skipped", report.binary);
    }
    if !report.dirty.is_empty() {
        println!(
            "  {} file(s) already contain invisible characters and were NOT tracked:",
            report.dirty.len()
        );
        for (p, n) in &report.dirty {
            println!("    {} ({n})", p.display());
        }
        println!("  Clean them and `ultnas track` each, or re-run with --accept-existing.");
    }
    Ok(())
}

pub fn untrack(vault_root: &Path, args: FileArg) -> Result<()> {
    let vault = Vault::open(vault_root)?;
    let path = canonical_path(&args.file)?;
    match ops::untrack(&vault, &path, Actor::Cli)? {
        UntrackOutcome::Dir { files } => {
            println!("✓ No longer tracking directory {}", path.display());
            println!("  {files} file(s) untracked; their versions stay in the vault.");
            println!("  Files tracked on their own inside it are unaffected.");
        }
        UntrackOutcome::File { kept_out_of } => {
            println!("✓ No longer tracking {}", path.display());
            println!("  Its versions stay in the vault.");
            if let Some(dir) = kept_out_of {
                println!("  It won't be re-adopted from {}.", dir.display());
            }
        }
        UntrackOutcome::KeptOutOf(dir) => {
            println!(
                "✓ {} will no longer be adopted from {}",
                path.display(),
                dir.display()
            );
        }
        UntrackOutcome::NotTracked => bail!("{} is not tracked", path.display()),
    }
    Ok(())
}

pub fn approve(vault_root: &Path, args: FileArg) -> Result<()> {
    let vault = Vault::open(vault_root)?;
    let path = canonical_path(&args.file)?;
    match ops::approve(&vault, &path, Actor::Cli)? {
        ApproveOutcome::Approved { stable, rewrote } => {
            if rewrote {
                println!("  Rewrote {} with the approved version.", path.display());
            }
            println!("✓ Approved {}", path.display());
            println!("  stable version {}", short(&stable));
            Ok(())
        }
        ApproveOutcome::NotTracked => bail!("{} is not tracked", path.display()),
        ApproveOutcome::NoPending => {
            bail!("{} has no pending edit to approve", path.display())
        }
    }
}

pub fn list(vault_root: &Path) -> Result<()> {
    let vault = Vault::open(vault_root)?;
    let overview = ops::tracked_overview(&vault)?;
    if overview.files.is_empty() && overview.dirs.is_empty() {
        println!("No tracked files. Add one with `ultnas track <file>`.");
        return Ok(());
    }
    for (d, n) in &overview.dirs {
        println!("{} (directory)", d.path.display());
        print!("  namespace {}  {n} file(s)", ns_label(&d.namespace));
        if !d.exclude.is_empty() {
            print!("  excluding {}", d.exclude.join(", "));
        }
        println!();
    }
    for (t, status) in overview.files {
        println!("{}", t.path.display());
        println!(
            "  namespace {}  stable {}  status {}",
            ns_label(&t.namespace),
            short(&t.stable),
            status.label()
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

/// A live `label n/total…` counter on a terminal; silent otherwise.
struct Progress {
    label: &'static str,
    total: usize,
    n: usize,
    shown: bool,
    tty: bool,
    last: std::time::Instant,
}

impl Progress {
    fn new(label: &'static str, total: usize) -> Self {
        use std::io::IsTerminal;
        Self {
            label,
            total,
            n: 0,
            shown: false,
            tty: std::io::stderr().is_terminal(),
            last: std::time::Instant::now(),
        }
    }

    fn tick(&mut self) {
        self.n += 1;
        // Only for work that takes a moment, and at most ~10 times a second.
        if !self.tty || self.total < 50 || self.last.elapsed().as_millis() < 100 {
            return;
        }
        self.last = std::time::Instant::now();
        self.shown = true;
        eprint!("\r  {} {}/{}…", self.label, self.n, self.total);
    }

    fn done(&mut self) {
        if self.shown {
            eprintln!("\r  {} {}/{} done.", self.label, self.total, self.total);
            self.shown = false;
        }
    }
}

fn short(id: &ContentId) -> String {
    id.to_hex()[..12].to_string()
}
