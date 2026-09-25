//! Shared harness: run the real `ultnas` (and `ultnasd`) binaries against a
//! throwaway vault, with `HOME` pointed at a scratch directory so nothing
//! reads or tracks the real home.

#![allow(dead_code)] // each test file uses a different subset

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;

pub const ZWSP: char = '\u{200B}';

pub struct Env {
    _dir: TempDir,
    /// Canonical scratch root; everything below lives in it.
    pub root: PathBuf,
    pub home: PathBuf,
    pub vault: PathBuf,
    /// A work area outside `home`.
    pub work: PathBuf,
}

pub struct Run {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    fn from(out: Output) -> Self {
        Self {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Both streams, for assertions that don't care which one.
    pub fn all(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }

    #[track_caller]
    pub fn assert_ok(&self) -> &Self {
        assert!(self.ok, "expected success:\n{}", self.all());
        self
    }

    #[track_caller]
    pub fn assert_err(&self, needle: &str) -> &Self {
        assert!(!self.ok, "expected failure:\n{}", self.all());
        assert!(
            self.all().contains(needle),
            "missing {needle:?} in:\n{}",
            self.all()
        );
        self
    }

    #[track_caller]
    pub fn assert_has(&self, needle: &str) -> &Self {
        assert!(
            self.all().contains(needle),
            "missing {needle:?} in:\n{}",
            self.all()
        );
        self
    }
}

impl Env {
    /// A fresh scratch area with an initialized vault.
    pub fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let (home, work) = (root.join("home"), root.join("work"));
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&work).unwrap();
        let env = Self {
            _dir: dir,
            vault: root.join("vault"),
            root,
            home,
            work,
        };
        env.ultnas(&["init", "--name", "test"]).assert_ok();
        env
    }

    fn command(&self, exe: &Path) -> Command {
        let mut cmd = Command::new(exe);
        cmd.env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("LOCALAPPDATA", self.home.join("AppData").join("Local"))
            .env_remove("XDG_DATA_HOME")
            .env_remove("ULTNAS_VAULT")
            .env_remove("RUST_LOG")
            .stdin(Stdio::null());
        cmd
    }

    /// Run `ultnas --vault <vault> <args…>` with no terminal on stdin.
    pub fn ultnas(&self, args: &[&str]) -> Run {
        let out = self
            .command(Path::new(env!("CARGO_BIN_EXE_ultnas")))
            .arg("--vault")
            .arg(&self.vault)
            .args(args)
            .output()
            .unwrap();
        Run::from(out)
    }

    /// Run `ultnas <args…>` without `--vault`, so the default location (or
    /// `vault_env` as `$ULTNAS_VAULT`) applies.
    pub fn ultnas_default(&self, args: &[&str], vault_env: Option<&Path>) -> Run {
        let mut cmd = self.command(Path::new(env!("CARGO_BIN_EXE_ultnas")));
        if let Some(v) = vault_env {
            cmd.env("ULTNAS_VAULT", v);
        }
        Run::from(cmd.args(args).output().unwrap())
    }

    /// Where the default vault is under the fake home.
    pub fn default_vault(&self) -> PathBuf {
        if cfg!(windows) {
            self.home.join("AppData").join("Local").join("ultnas")
        } else {
            self.home.join(".local").join("share").join("ultnas")
        }
    }

    /// [`Env::ultnas`] with path arguments.
    pub fn ultnas_p(&self, args: &[&str], path: &Path) -> Run {
        let mut all: Vec<&str> = args.to_vec();
        let p = path.to_str().unwrap();
        all.push(p);
        self.ultnas(&all)
    }

    /// Write a file (creating parents) and return its canonical path.
    pub fn write(&self, rel: &str, content: &str) -> PathBuf {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
        fs::canonicalize(p).unwrap()
    }

    pub fn read(&self, p: &Path) -> String {
        fs::read_to_string(p).unwrap_or_default()
    }

    pub fn record_count(&self) -> usize {
        fs::read_dir(self.vault.join("records"))
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .is_ok_and(|e| e.path().extension().is_some_and(|x| x == "json"))
            })
            .count()
    }

    /// The daemon binary, built alongside `ultnas` by `cargo build --workspace`.
    pub fn daemon_exe() -> Option<PathBuf> {
        let exe = Path::new(env!("CARGO_BIN_EXE_ultnas"))
            .with_file_name(format!("ultnasd{}", std::env::consts::EXE_SUFFIX));
        exe.exists().then_some(exe)
    }

    /// Start `ultnasd` on the vault and wait until it answers over IPC and
    /// its watcher has finished the first full scan.
    pub fn start_daemon(&self, extra: &[&str]) -> Daemon {
        let exe = Self::daemon_exe().expect("ultnasd not built");
        let child = self
            .command(&exe)
            .arg("--vault")
            .arg(&self.vault)
            .arg("--no-log-file")
            .args(extra)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let daemon = Daemon { child };
        let ready = || {
            let run = self.ultnas(&["daemon", "status", "--json"]);
            run.ok
                && serde_json::from_str::<serde_json::Value>(&run.stdout)
                    .is_ok_and(|s| !s["watcher"]["last_full_scan"].is_null())
        };
        assert!(
            eventually(Duration::from_secs(20), ready),
            "daemon never became ready"
        );
        daemon
    }
}

/// A running daemon, killed if the test ends without stopping it.
pub struct Daemon {
    pub child: Child,
}

impl Daemon {
    /// Wait for the process to exit on its own; returns whether it succeeded.
    pub fn wait_exit(&mut self, within: Duration) -> Option<bool> {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some(status.success());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Poll `ok` until it holds or `within` passes.
pub fn eventually(within: Duration, mut ok: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
