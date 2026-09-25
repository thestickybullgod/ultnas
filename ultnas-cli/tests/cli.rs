//! End-to-end tests of the `ultnas` binary, without a daemon.

mod common;

use common::{Env, ZWSP};
use std::fs;

#[test]
fn help_and_verbose_flag_work() {
    let env = Env::new();
    // Regression: -v was claimed by both --vault and --verbose, so clap
    // panicked on every invocation, --help included.
    env.ultnas(&["--help"]).assert_ok().assert_has("track");
    env.ultnas(&["-v", "tracked"]).assert_ok();
}

#[test]
fn setup_creates_the_default_vault_and_commands_find_it() {
    let env = Env::new();
    env.write("home/.bashrc", "export A=1\n");
    // No vault there yet: commands say how to get one.
    env.ultnas_default(&["tracked"], None)
        .assert_err("ultnas setup");

    env.ultnas_default(&["setup", "--yes"], None)
        .assert_ok()
        .assert_has("Created vault");
    assert!(env.default_vault().join("vault.toml").exists());
    env.ultnas_default(&["tracked"], None)
        .assert_ok()
        .assert_has(".bashrc");
}

#[test]
fn ultnas_vault_env_selects_the_vault() {
    let env = Env::new();
    let f = env.write("work/a.txt", "a\n");
    env.ultnas_default(&["track", f.to_str().unwrap()], Some(&env.vault))
        .assert_ok();
    // The explicitly created test vault, not the default location.
    env.ultnas(&["tracked"]).assert_has("a.txt");
    assert!(!env.default_vault().exists());
}

#[test]
fn completions_and_manpage_need_no_vault() {
    let env = Env::new();
    for shell in ["bash", "zsh", "fish"] {
        let run = env.ultnas_default(&["completions", shell], None);
        run.assert_ok();
        assert!(
            run.stdout.contains("track"),
            "{shell} completions lack commands"
        );
    }
    let man = env.ultnas_default(&["manpage"], None);
    man.assert_ok();
    assert!(
        man.stdout.contains(".TH ultnas"),
        "{}",
        &man.stdout[..man.stdout.len().min(200)]
    );
}

#[test]
fn init_refuses_an_existing_vault() {
    let env = Env::new();
    env.ultnas(&["init", "--name", "again"])
        .assert_err("already exists");
}

#[test]
fn track_a_file_list_it_and_untrack_it() {
    let env = Env::new();
    let f = env.write("work/notes.txt", "hello\n");

    env.ultnas_p(&["track"], &f)
        .assert_ok()
        .assert_has("Tracking");
    env.ultnas_p(&["track"], &f).assert_err("already tracked");
    env.ultnas(&["tracked"])
        .assert_ok()
        .assert_has("notes.txt")
        .assert_has("status ok");

    fs::write(&f, "hello, edited\n").unwrap();
    env.ultnas(&["tracked"]).assert_has("changed");

    env.ultnas_p(&["untrack"], &f)
        .assert_ok()
        .assert_has("No longer tracking");
    env.ultnas(&["tracked"]).assert_has("No tracked files");
    env.ultnas_p(&["untrack"], &f).assert_err("not tracked");
}

#[test]
fn track_refuses_invisible_characters_unless_accepted() {
    let env = Env::new();
    let f = env.write("work/dirty.txt", &format!("pass{ZWSP}word\n"));

    env.ultnas_p(&["track"], &f)
        .assert_err("--accept-existing")
        .assert_has("U+200B");
    env.ultnas_p(&["track", "--accept-existing"], &f)
        .assert_ok();
}

#[test]
fn track_refuses_binary_files_and_bare_directories() {
    let env = Env::new();
    let bin = env.root.join("work/blob.bin");
    fs::write(&bin, [0xffu8, 0xfe, 0x00, 0x01]).unwrap();
    env.ultnas_p(&["track"], &bin).assert_err("not UTF-8");

    let dir = env.root.join("work");
    env.ultnas_p(&["track"], &dir).assert_err("--recursive");
    let f = env.write("work/a.txt", "a\n");
    env.ultnas_p(&["track", "-r"], &f)
        .assert_err("needs a directory");
}

#[test]
fn track_recursive_sorts_files_and_honours_excludes() {
    let env = Env::new();
    env.write("work/proj/src/main.rs", "fn main() {}\n");
    env.write("work/proj/README", "readme\n");
    env.write("work/proj/dirty.txt", &format!("a{ZWSP}b\n"));
    env.write("work/proj/target/out.txt", "build output\n");
    env.write("work/proj/.git/config", "[core]\n");
    fs::write(
        env.root.join("work/proj/logo.png"),
        [0x89u8, 0x50, 0xff, 0x00],
    )
    .unwrap();
    let proj = env.root.join("work/proj");

    // Outside the (fake) home, so it asks; with no terminal it refuses.
    env.ultnas_p(&["track", "-r"], &proj)
        .assert_err("confirmation needed")
        .assert_has("outside your home");
    assert!(env.ultnas(&["tracked"]).all().contains("No tracked files"));

    env.ultnas(&[
        "track",
        "-r",
        "--exclude",
        "target",
        "--yes",
        proj.to_str().unwrap(),
    ])
    .assert_ok()
    .assert_has("2 text file(s) tracked")
    .assert_has("1 binary file(s) skipped")
    .assert_has("dirty.txt");

    let list = env.ultnas(&["tracked"]);
    list.assert_has("(directory)")
        .assert_has("main.rs")
        .assert_has("README")
        .assert_has("excluding target");
    for absent in ["dirty.txt", "out.txt", "config", "logo.png"] {
        assert!(
            !list.stdout.contains(absent),
            "{absent} tracked:\n{}",
            list.stdout
        );
    }

    env.ultnas_p(&["untrack"], &proj)
        .assert_ok()
        .assert_has("2 file(s) untracked");
    env.ultnas(&["tracked"]).assert_has("No tracked files");
}

#[test]
fn untracking_a_file_in_a_tracked_directory_keeps_it_out() {
    let env = Env::new();
    let keep = env.write("home/proj/keep.txt", "keep\n");
    let drop = env.write("home/proj/drop.txt", "drop\n");
    let proj = keep.parent().unwrap().to_path_buf();
    // Inside the fake home and small: no confirmation needed.
    env.ultnas_p(&["track", "-r"], &proj).assert_ok();

    env.ultnas_p(&["untrack"], &drop)
        .assert_ok()
        .assert_has("won't be re-adopted");
    let tdir = fs::read_dir(env.vault.join("tracked"))
        .unwrap()
        .flatten()
        .find(|e| e.path().extension().is_some_and(|x| x == "tdir"))
        .unwrap();
    assert!(fs::read_to_string(tdir.path())
        .unwrap()
        .contains("drop.txt"));
    assert!(env.ultnas(&["tracked"]).stdout.contains("keep.txt"));
}

#[test]
fn approve_needs_a_pending_edit() {
    let env = Env::new();
    let f = env.write("work/a.txt", "a\n");
    env.ultnas_p(&["approve"], &f).assert_err("not tracked");
    env.ultnas_p(&["track"], &f).assert_ok();
    env.ultnas_p(&["approve"], &f).assert_err("no pending edit");
}

#[test]
fn setup_lists_and_tracks_whats_in_home() {
    let env = Env::new();
    env.write("home/.bashrc", "export A=1\n");
    env.write("home/.ssh/config", "Host *\n");
    env.write("home/src/app/main.rs", "fn main() {}\n");
    env.write("home/src/app/node_modules/dep/index.js", "x\n");
    env.write("home/.config/tool/cfg.toml", "a = 1\n");

    let list = env.ultnas(&["setup", "--list"]);
    list.assert_ok()
        .assert_has(".bashrc")
        .assert_has("config")
        .assert_has("[x]")
        .assert_has("[ ]");
    env.ultnas(&["setup"]).assert_err("--yes");

    env.ultnas(&["setup", "--yes"])
        .assert_ok()
        .assert_has("Done: 3 set up, 0 failed");
    let tracked = env.ultnas(&["tracked"]).stdout;
    assert!(
        tracked.contains(".bashrc") && tracked.contains("main.rs"),
        "{tracked}"
    );
    assert!(
        !tracked.contains("index.js"),
        "node_modules must be excluded"
    );
    assert!(!tracked.contains("cfg.toml"), "~/.config isn't pre-checked");
    env.ultnas(&["setup", "--list"])
        .assert_has("already tracked");
}

#[test]
fn purge_rotate_previews_asks_and_spares_current_versions() {
    let env = Env::new();
    let policy = env.write(
        "policy.toml",
        "version = 1\n[[namespaces]]\npath = \"docs\"\n  [namespaces.retention]\n  keep_days = 0\n",
    );
    let p = policy.to_str().unwrap();
    // One tracked file: its only version is current, so nothing may go.
    let f = env.write("work/a.txt", "a\n");
    env.ultnas(&["track", "--namespace", "docs", f.to_str().unwrap()])
        .assert_ok();
    // An ordinary record in the same namespace, which keep_days = 0 expires.
    let other = env.write("work/b.txt", "b\n");
    env.ultnas(&["add", "--namespace", "docs", other.to_str().unwrap()])
        .assert_ok();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert_eq!(env.record_count(), 2);

    env.ultnas(&["purge", "--rotate", "--dry-run", "--policy", p])
        .assert_ok()
        .assert_has("b.txt")
        .assert_has("dry run");
    env.ultnas(&["purge", "--rotate", "--policy", p])
        .assert_err("confirmation needed");
    assert_eq!(env.record_count(), 2);

    env.ultnas(&["purge", "--rotate", "--yes", "--policy", p])
        .assert_ok()
        .assert_has("Purged 1 record(s)");
    assert_eq!(env.record_count(), 1, "the tracked file's version stays");

    let stable = fs::read_dir(env.vault.join("records"))
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .path();
    let id = stable.file_stem().unwrap().to_str().unwrap().to_string();
    env.ultnas(&["purge", "--yes", &id])
        .assert_err("current version");
    env.ultnas(&["purge"]).assert_err("--rotate");
}

#[test]
fn integrity_and_policy_commands_without_a_daemon() {
    let env = Env::new();
    env.ultnas(&["integrity", "status"])
        .assert_ok()
        .assert_has("No journal found");
    env.ultnas(&["integrity", "lift-quarantine", "docs", "--yes"])
        .assert_ok()
        .assert_has("nothing is quarantined");

    let good = env.write(
        "good.toml",
        "version = 1\n[global.integrity]\napproval = \"approved\"\n",
    );
    env.ultnas_p(&["policy", "validate"], &good)
        .assert_ok()
        .assert_has("valid");
    let bad = env.write(
        "bad.toml",
        "version = 1\n[global.integrity]\napproval = \"sometimes\"\n",
    );
    env.ultnas_p(&["policy", "validate"], &bad).assert_err("");
}

#[test]
fn daemon_commands_report_when_no_daemon_runs() {
    let env = Env::new();
    env.ultnas(&["daemon", "status"])
        .assert_err("no daemon is running");
    env.ultnas(&["daemon", "stop"])
        .assert_err("no daemon is running");
}
