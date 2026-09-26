//! `ultnas setup` — choose recommended places to protect.
//!
//! Suggests the files and directories where an invisible-character edit does
//! real damage, because they are executed, trusted, or reviewed: shell
//! startup files, SSH and Git configuration, script and source directories,
//! and (as root) `/etc`. Only what exists on this machine is offered, each
//! group in its own namespace so a quarantine in one (say, `code`) doesn't
//! pause protection of another (`ssh`).

use anyhow::{bail, Result};
use clap::Args;
use std::{
    collections::BTreeSet,
    io::{self, BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
};
use ultnas_core::{canonical_path, NamespacePath, UltnasCoreError, Vault};

use super::track::{home, track_dir, track_one, DirOptions, Outcome};

#[derive(Args)]
pub struct SetupArgs {
    /// Track the recommended (pre-checked) items without asking
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Only list the suggestions
    #[arg(long, conflicts_with = "yes")]
    pub list: bool,
}

/// Build output, dependencies, and caches under source directories.
const CODE_EXCLUDES: &[&str] = &[
    "target",
    "node_modules",
    "venv",
    "build",
    "dist",
    "__pycache__",
    "vendor",
];
/// Caches and churning application state under `~/.config`.
const CONFIG_EXCLUDES: &[&str] = &[
    "Cache",
    "cache",
    "Code Cache",
    "GPUCache",
    "CachedData",
    "logs",
];

#[derive(Debug, Clone, PartialEq)]
struct Suggestion {
    path: PathBuf,
    recursive: bool,
    namespace: &'static str,
    exclude: &'static [&'static str],
    note: &'static str,
    recommended: bool,
    /// Why it can't be selected, if it can't.
    unavailable: Option<&'static str>,
}

/// Everything worth offering under `home` (and system-wide), whether or
/// not it exists; [`available`] filters to this machine.
fn catalogue(home: Option<&Path>, is_root: bool) -> Vec<Suggestion> {
    let item = |path: PathBuf, recursive, namespace, exclude, note, recommended| Suggestion {
        path,
        recursive,
        namespace,
        exclude,
        note,
        recommended,
        unavailable: None,
    };
    let mut out = vec![];
    if let Some(h) = home {
        for f in [
            ".bashrc",
            ".bash_profile",
            ".profile",
            ".zshrc",
            ".zprofile",
            ".zshenv",
        ] {
            out.push(item(h.join(f), false, "shell", &[], "shell startup", true));
        }
        for f in [".ssh/config", ".ssh/authorized_keys"] {
            out.push(item(
                h.join(f),
                false,
                "ssh",
                &[],
                "who and what you trust",
                true,
            ));
        }
        for f in [".gitconfig", ".config/git/config"] {
            out.push(item(
                h.join(f),
                false,
                "git",
                &[],
                "git configuration",
                true,
            ));
        }
        for d in ["bin", ".local/bin"] {
            out.push(item(h.join(d), true, "scripts", &[], "your scripts", true));
        }
        for d in [
            "src",
            "dev",
            "code",
            "projects",
            "repos",
            "workspace",
            "git",
        ] {
            out.push(item(
                h.join(d),
                true,
                "code",
                CODE_EXCLUDES,
                "source code",
                true,
            ));
        }
        out.push(item(
            h.join(".config"),
            true,
            "config",
            CONFIG_EXCLUDES,
            "large; includes application state that changes often",
            false,
        ));
    }
    if cfg!(unix) {
        let mut etc = item(
            PathBuf::from("/etc"),
            true,
            "etc",
            &[],
            "system configuration; approved mode recommended",
            is_root,
        );
        if !is_root {
            etc.unavailable = Some("requires root");
        }
        out.push(etc);
    }
    out
}

/// The catalogue entries that exist here as the right kind of thing,
/// canonicalized, with already-tracked ones marked unavailable.
fn available(catalogue: Vec<Suggestion>, tracked: &BTreeSet<PathBuf>) -> Vec<Suggestion> {
    let mut seen = BTreeSet::new();
    catalogue
        .into_iter()
        .filter_map(|mut s| {
            let meta = std::fs::metadata(&s.path).ok()?;
            if meta.is_dir() != s.recursive {
                return None;
            }
            s.path = canonical_path(&s.path).ok()?;
            // Two names for one file (e.g. a symlinked dotfile).
            if !seen.insert(s.path.clone()) {
                return None;
            }
            if tracked.contains(&s.path) {
                s.unavailable = Some("already tracked");
            }
            Some(s)
        })
        .collect()
}

/// Apply one line of input to the selection. Returns `Ok(true)` when the
/// user is done choosing.
fn apply_input(
    line: &str,
    items: &[Suggestion],
    chosen: &mut BTreeSet<usize>,
) -> Result<bool, String> {
    let line = line.trim();
    match line {
        "" => return Ok(true),
        "a" | "all" => {
            chosen.extend((0..items.len()).filter(|&i| items[i].unavailable.is_none()));
            return Ok(false);
        }
        "n" | "none" => {
            chosen.clear();
            return Ok(false);
        }
        _ => {}
    }
    for token in line.split([' ', ',']).filter(|t| !t.is_empty()) {
        let n: usize = token
            .parse()
            .map_err(|_| format!("not a number: {token}"))?;
        let i = n
            .checked_sub(1)
            .filter(|&i| i < items.len())
            .ok_or_else(|| format!("no item {n}"))?;
        if let Some(why) = items[i].unavailable {
            return Err(format!("item {n} can't be selected: {why}"));
        }
        if !chosen.remove(&i) {
            chosen.insert(i);
        }
    }
    Ok(false)
}

fn show(items: &[Suggestion], chosen: &BTreeSet<usize>, home: Option<&Path>) {
    let label = |p: &Path| match home.and_then(|h| p.strip_prefix(h).ok()) {
        Some(rel) => format!("~/{}", rel.display()),
        None => p.display().to_string(),
    };
    for (i, s) in items.iter().enumerate() {
        let mark = match (s.unavailable, chosen.contains(&i)) {
            (Some(_), _) => "   ",
            (None, true) => "[x]",
            (None, false) => "[ ]",
        };
        let mode = if s.recursive { "recursive" } else { "file" };
        let why = s
            .unavailable
            .map(|w| format!("  ({w})"))
            .unwrap_or_default();
        println!(
            "  {mark} {:>2}  {:<28} {:<9} {:<8} {}{why}",
            i + 1,
            label(&s.path),
            mode,
            s.namespace,
            s.note
        );
    }
}

pub fn run(vault_root: &Path, args: SetupArgs) -> Result<()> {
    let vault = match Vault::open(vault_root) {
        Ok(v) => v,
        Err(UltnasCoreError::VaultNotFound(_)) if !args.list => {
            let name = std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .unwrap_or_else(|_| "ultnas".into());
            let v = Vault::init(vault_root, &name)?;
            println!("✓ Created vault `{name}` at {}\n", v.root().display());
            v
        }
        Err(e) => return Err(e.into()),
    };
    let home = home();
    let tracked: BTreeSet<PathBuf> = vault
        .tracked_files()?
        .into_iter()
        .map(|t| t.path)
        .chain(vault.tracked_dirs()?.into_iter().map(|d| d.path))
        .collect();
    let items = available(catalogue(home.as_deref(), is_root()), &tracked);
    if items.is_empty() {
        println!("No recommended places found on this machine. Use `ultnas track` directly.");
        return Ok(());
    }

    let mut chosen: BTreeSet<usize> = (0..items.len())
        .filter(|&i| items[i].recommended && items[i].unavailable.is_none())
        .collect();
    println!("Recommended places to protect (found on this machine):\n");
    if args.list {
        show(&items, &chosen, home.as_deref());
        return Ok(());
    }
    if !args.yes {
        if !io::stdin().is_terminal() {
            bail!("setup is interactive; pass --yes to track the recommended items, or --list");
        }
        loop {
            show(&items, &chosen, home.as_deref());
            if chosen.is_empty() {
                print!(
                    "\nNothing is checked. Enter to finish; numbers to check (e.g. 3 7), \
                     a = all, q = quit: "
                );
            } else {
                print!(
                    "\nEnter to track the checked items; numbers to toggle (e.g. 3 7), \
                     a = all, n = none, q = quit: "
                );
            }
            io::stdout().flush()?;
            let line = io::stdin()
                .lock()
                .lines()
                .next()
                .unwrap_or(Ok(String::new()))?;
            if line.trim() == "q" {
                println!("Nothing tracked.");
                return Ok(());
            }
            match apply_input(&line, &items, &mut chosen) {
                Ok(true) => break,
                // Say what changed: the list reprints in full, which is easy
                // to mistake for nothing having happened.
                Ok(false) => {
                    let hint = if chosen.is_empty() {
                        "Enter finishes without tracking anything"
                    } else {
                        "Enter tracks them"
                    };
                    println!("\n  → {} item(s) checked — {hint}\n", chosen.len());
                }
                Err(e) => println!("  {e}\n"),
            }
        }
    }
    if chosen.is_empty() {
        println!("Nothing selected.");
        return Ok(());
    }

    let (mut ok, mut failed) = (0usize, 0usize);
    for &i in &chosen {
        let s = &items[i];
        let namespace = NamespacePath::parse(s.namespace)?;
        let result = if s.recursive {
            let opts = DirOptions {
                exclude: s.exclude.iter().map(|e| e.to_string()).collect(),
                accept_existing: false,
                yes: args.yes,
            };
            track_dir(&vault, s.path.clone(), namespace, &opts)
        } else {
            match track_one(&vault, &s.path, &namespace, None, false) {
                Ok(Outcome::Tracked(_)) => {
                    println!("✓ Tracking {}", s.path.display());
                    Ok(())
                }
                Ok(Outcome::AlreadyTracked) => {
                    println!("  {} is already tracked", s.path.display());
                    Ok(())
                }
                Ok(Outcome::NotText) => Err(anyhow::anyhow!("not UTF-8 text")),
                Ok(Outcome::HasInvisible(found)) => Err(anyhow::anyhow!(
                    "already contains {} invisible character(s) (first: {}); clean it, then \
                     `ultnas track` it",
                    found.len(),
                    found[0]
                )),
                Err(e) => Err(e),
            }
        };
        match result {
            Ok(()) => ok += 1,
            Err(e) => {
                failed += 1;
                println!("✗ {}: {e:#}", s.path.display());
            }
        }
    }
    println!("\nDone: {ok} set up, {failed} failed. `ultnas tracked` shows everything.");
    print_start_hint(vault_root);
    Ok(())
}

/// How to start protecting, if a systemd service is how this machine does it.
fn print_start_hint(vault_root: &Path) {
    if !Path::new("/run/systemd/system").is_dir() {
        println!(
            "Start protecting: ultnasd --vault {} &",
            vault_root.display()
        );
        return;
    }
    let default = Vault::default_root();
    let custom = if vault_root == default {
        String::new()
    } else {
        format!(
            " (the service uses {}; set ULTNAS_VAULT in it for another vault)",
            default.display()
        )
    };
    if is_root() {
        println!("Start protecting: systemctl enable --now ultnasd{custom}");
    } else {
        println!("Start protecting: systemctl --user enable --now ultnasd{custom}");
    }
}

fn is_root() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fake_home() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let h = canonical_path(dir.path()).unwrap();
        std::fs::write(h.join(".bashrc"), "export A=1\n").unwrap();
        std::fs::create_dir_all(h.join(".ssh")).unwrap();
        std::fs::write(h.join(".ssh/config"), "Host *\n").unwrap();
        std::fs::create_dir_all(h.join("src")).unwrap();
        std::fs::create_dir_all(h.join(".config")).unwrap();
        // Wrong kind: a directory where a file is expected.
        std::fs::create_dir_all(h.join(".profile")).unwrap();
        (dir, h)
    }

    fn names(items: &[Suggestion], h: &Path) -> Vec<String> {
        items
            .iter()
            .filter(|s| s.path.starts_with(h))
            .map(|s| s.path.strip_prefix(h).unwrap().display().to_string())
            .collect()
    }

    #[test]
    fn only_existing_items_of_the_right_kind_are_offered() {
        let (_d, h) = fake_home();
        let items = available(catalogue(Some(&h), false), &BTreeSet::new());
        let got = names(&items, &h);
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(
            got,
            vec![
                ".bashrc".to_string(),
                format!(".ssh{sep}config"),
                "src".to_string(),
                ".config".to_string()
            ]
        );
        let config = items.iter().find(|s| s.path.ends_with(".config")).unwrap();
        assert!(!config.recommended);
        assert!(config.recursive);
    }

    #[test]
    fn already_tracked_and_root_only_items_are_unavailable() {
        let (_d, h) = fake_home();
        let tracked: BTreeSet<PathBuf> = [h.join(".bashrc")].into();
        let items = available(catalogue(Some(&h), false), &tracked);
        let bashrc = items.iter().find(|s| s.path.ends_with(".bashrc")).unwrap();
        assert_eq!(bashrc.unavailable, Some("already tracked"));
        if cfg!(unix) && Path::new("/etc").is_dir() {
            // Canonical, since /etc is a link to /private/etc on macOS.
            let etc_path = canonical_path(Path::new("/etc")).unwrap();
            let etc = items.iter().find(|s| s.path == etc_path).unwrap();
            assert_eq!(etc.unavailable, Some("requires root"));
            assert!(!etc.recommended);
        }
    }

    #[test]
    fn selection_input_toggles_and_validates() {
        let (_d, h) = fake_home();
        let tracked: BTreeSet<PathBuf> = [h.join(".bashrc")].into();
        let items = available(catalogue(Some(&h), false), &tracked);
        let mut chosen = BTreeSet::new();

        assert_eq!(apply_input("2 3", &items, &mut chosen), Ok(false));
        assert_eq!(chosen, [1, 2].into());
        assert_eq!(apply_input("3", &items, &mut chosen), Ok(false));
        assert_eq!(chosen, [1].into());
        assert!(apply_input("1", &items, &mut chosen)
            .unwrap_err()
            .contains("already tracked"));
        assert!(apply_input("99", &items, &mut chosen).is_err());
        assert!(apply_input("x", &items, &mut chosen).is_err());
        assert_eq!(apply_input("none", &items, &mut chosen), Ok(false));
        assert!(chosen.is_empty());
        assert_eq!(apply_input("all", &items, &mut chosen), Ok(false));
        assert!(!chosen.contains(&0), "unavailable items stay unselected");
        assert_eq!(apply_input("", &items, &mut chosen), Ok(true));
    }
}
