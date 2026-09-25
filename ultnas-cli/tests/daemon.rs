//! End-to-end tests of `ultnas` with a real `ultnasd` running.
//!
//! These need the daemon binary, which `cargo build --workspace` (as CI
//! does before testing) puts next to `ultnas`. Without it they are skipped
//! with a note rather than failing.

mod common;

use common::{eventually, Env, ZWSP};
use std::{fs, time::Duration};
use ultnas_core::ipc::DaemonStatus;

const WAIT: Duration = Duration::from_secs(15);

macro_rules! require_daemon {
    () => {
        if Env::daemon_exe().is_none() {
            eprintln!("skipping: ultnasd isn't built (run `cargo build --workspace`)");
            return;
        }
    };
}

fn status(env: &Env) -> DaemonStatus {
    let run = env.ultnas(&["daemon", "status", "--json"]);
    run.assert_ok();
    serde_json::from_str(&run.stdout).unwrap()
}

#[test]
fn daemon_repairs_attacks_reports_status_and_stops() {
    require_daemon!();
    let env = Env::new();
    let f = env.write("work/config.txt", "is_admin = false\n");
    env.ultnas_p(&["track"], &f).assert_ok();
    let mut daemon = env.start_daemon(&[]);

    fs::write(&f, format!("is_{ZWSP}admin = false\n")).unwrap();
    assert!(
        eventually(WAIT, || env.read(&f) == "is_admin = false\n"),
        "attack not repaired: {:?}",
        env.read(&f)
    );

    let s = status(&env);
    assert_eq!(s.watcher.tracked_files, 1);
    assert!(!s.journal.degraded);
    assert!(
        s.recent_alerts.iter().any(|a| a.kind == "sanitized"),
        "{:?}",
        s.recent_alerts
    );
    env.ultnas(&["daemon", "status"])
        .assert_ok()
        .assert_has("journal    ok")
        .assert_has("sanitized");

    env.ultnas(&["daemon", "stop"])
        .assert_ok()
        .assert_has("stopping");
    assert_eq!(
        daemon.wait_exit(WAIT),
        Some(true),
        "daemon didn't exit cleanly"
    );
    env.ultnas(&["daemon", "status"])
        .assert_err("no daemon is running");
}

#[test]
fn approved_mode_holds_edits_until_approve() {
    require_daemon!();
    let env = Env::new();
    let policy = env.write(
        "policy.toml",
        "version = 1\n[global.integrity]\napproval = \"approved\"\n",
    );
    let f = env.write("work/a.txt", "v1\n");
    env.ultnas_p(&["track"], &f).assert_ok();
    let _daemon = env.start_daemon(&["--policy", policy.to_str().unwrap()]);

    fs::write(&f, "v2\n").unwrap();
    assert!(
        eventually(WAIT, || env
            .ultnas(&["tracked"])
            .stdout
            .contains("pending approval")),
        "clean edit not held pending:\n{}",
        env.ultnas(&["tracked"]).stdout
    );
    assert_eq!(env.read(&f), "v2\n", "a pending edit stays on disk");

    env.ultnas_p(&["approve"], &f)
        .assert_ok()
        .assert_has("Approved");
    env.ultnas(&["tracked"]).assert_has("status ok");
}

#[test]
fn a_second_daemon_is_refused() {
    require_daemon!();
    let env = Env::new();
    let _first = env.start_daemon(&[]);
    let out = std::process::Command::new(Env::daemon_exe().unwrap())
        .arg("--vault")
        .arg(&env.vault)
        .arg("--no-log-file")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("already in use"), "{err}");
}

#[test]
fn files_created_in_a_tracked_directory_are_adopted() {
    require_daemon!();
    let env = Env::new();
    let proj = env.root.join("home/proj");
    fs::create_dir_all(&proj).unwrap();
    env.ultnas_p(&["track", "-r"], &proj).assert_ok();
    let _daemon = env.start_daemon(&[]);

    let new = proj.join("new.txt");
    fs::write(&new, format!("n{ZWSP}ew\n")).unwrap();
    assert!(
        eventually(WAIT, || env.read(&new) == "new\n"
            && env.ultnas(&["tracked"]).stdout.contains("new.txt")),
        "new file not adopted and stripped: {:?}",
        env.read(&new)
    );
}
