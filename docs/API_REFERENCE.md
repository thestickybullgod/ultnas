# Ultnas — API Reference

The authoritative API reference for `ultnas-core` is generated from the
source, so it can't drift from the code:

```bash
cargo doc -p ultnas-core --no-deps --open
```

## Where to start

| To… | Use |
|---|---|
| Check text for invisible characters | `ultnas_core::invisible::{scan, introduced, classify_change}` |
| Open or create a vault | `Vault::open`, `Vault::init`, `Vault::default_root` |
| Track a file | `Vault::write_version`, then `Vault::update_tracked` |
| Track a directory | `Vault::update_tracked_dir` with a `TrackedDir`; `check_trackable` first |
| Read or replace a live file safely | `read_live`, `rewrite_file`, `recreate_file` |
| Record what happened | `Journal::open_shared(…).write(JournalEntry { … })` (the daemon owns `Journal::open`) |
| Work out quarantine state | `Journal::quarantined_namespaces`, or fold entries with `QuarantineFold` |
| Apply retention | `retention::rotate` |
| Ask a running daemon | `ipc::request(vault_root, "status", timeout)` → `DaemonStatus` |
| Load a policy | `Vault::load_policy`, or `Policy::from_toml` |

[MODULE_SPECS.md](MODULE_SPECS.md) describes what each module is for and the
invariants it keeps.
