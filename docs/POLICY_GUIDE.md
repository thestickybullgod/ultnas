# Ultnas — Policy Guide

> Version: 0.1 | Last Updated: 2026-09-25

---

## Overview

Ultnas policy files are TOML documents that define how your vault behaves: what it accepts, how long it keeps records, how it handles conflicts, and what is required before a record can be sealed. Policies are first-class citizens — they are version-controlled, hash-embedded in every seal, and evaluated at multiple lifecycle points.

---

## Policy File Location

By convention, store your policy file at `<vault-root>/policy/default.toml`. Reference it in `vault.toml`:

```toml
[vault]
name = "my-archive"
policy_path = "policy/default.toml"
```

---

## Complete Policy Reference

### Top-level Keys

```toml
version = 1           # Policy schema version. Required. Must be 1 for now.

[global]
# Applied to all namespaces unless overridden at the namespace level.
conflict              = "reject"    # "reject" | "version" | "replace"
max_record_size_bytes = 104857600   # 100 MiB — omit for no limit
require_seal_before_rotation = true

[[namespaces]]
# Per-namespace rules. Multiple [[namespaces]] blocks allowed.
path = "projects"           # Exact namespace path (no wildcards yet)
conflict = "version"        # Overrides global.conflict for this namespace
tags_required = ["project"] # Records missing these tags are rejected

  [namespaces.retention]
  keep_versions = 10        # Keep at most 10 versions per label
  keep_days     = 365       # Purge records older than 365 days
```

---

## Integrity and Tracked Files

`ultnas track <file>` protects a live text file in place. The daemon compares
every change against the file's newest clean version:

- **A write that adds invisible characters** (zero-width, bidi controls, tag
  characters, fillers, other format characters) is a violation. Below
  `write_violation_threshold`, each one is prevented by stripping the
  characters it added. At the threshold, the file is deleted and recreated
  from its stable version: from memory first, then the vault's copy. After
  `escalate_after_restores` restores, the namespace is quarantined and left
  alone until `ultnas integrity lift-quarantine`.
- **A clean edit** is handled per `approval`:

| `approval` | A clean edit… | Restores use… |
|---|---|---|
| `"automatic"` (default) | becomes the stable version immediately | the latest clean edit |
| `"approved"` | stays on disk as *pending* until `ultnas approve <file>` | the last approved version (the pending one is kept) |

```toml
[global.integrity]
write_violation_threshold = 5     # prevented writes before delete-and-recreate
violation_window_secs     = 300   # counts reset after this much quiet
escalate_after_restores   = 3     # restores before the namespace is quarantined
restore_source            = "memory_then_store"   # "memory" | "store" | "memory_then_store"
approval                  = "automatic"           # "automatic" | "approved"
mirror                    = "/mnt/backup/ultnas"  # optional second copy (see below)

[[namespaces]]
path     = "legal"
approval = "approved"   # overrides the global setting for legal/ and below
```

A namespace's `approval` applies to it and every namespace under it; the most
specific one wins. Daemon flags such as `--violation-threshold` override the
policy's values.

A write that arrives with invisible characters is never trusted as an edit.
If stripping them leaves visible changes too, the result is held as pending
whatever the `approval` setting, and needs `ultnas approve <file>`.

### Mirror: a second copy

A sealed record's object in the vault *is* the thing being protected, so if
it is damaged while not in the daemon's memory cache, there is nothing to
restore it from, and the namespace is quarantined instead. Set `mirror` (or
start `ultnas daemon` with `--mirror <dir>`) to keep a second copy of every
sealed object and tracked-file version in another directory, ideally on
another disk: the daemon warns if it's on the same filesystem as the vault.

Every full scan copies what the mirror lacks, and replaces damaged copies,
from verified vault bytes. Restores try it last, after memory (and, for
tracked files, the vault), and like every source it is hash-checked, so a
damaged mirror copy is never used. A relative path is relative to the vault
root.

### Getting started: `ultnas setup`

`ultnas setup` offers the places on this machine where an invisible-character
edit does the most damage, with the recommended ones pre-checked:

| Suggested | Mode | Namespace |
|---|---|---|
| `~/.bashrc`, `~/.bash_profile`, `~/.profile`, `~/.zshrc`, `~/.zprofile`, `~/.zshenv` | file | `shell` |
| `~/.ssh/config`, `~/.ssh/authorized_keys` | file | `ssh` |
| `~/.gitconfig`, `~/.config/git/config` | file | `git` |
| `~/bin`, `~/.local/bin` | recursive | `scripts` |
| `~/src`, `~/dev`, `~/code`, `~/projects`, `~/repos`, `~/workspace`, `~/git` | recursive, excluding `target`, `node_modules`, `venv`, `build`, `dist`, `__pycache__`, `vendor` | `code` |
| `~/.config` (not pre-checked: large, churning app state) | recursive, excluding caches and logs | `config` |
| `/etc` (root only) | recursive | `etc` |

Only what exists is shown, and already-tracked items are marked. Each group
gets its own namespace, so a quarantine in one doesn't pause the others.
`--list` just prints the suggestions; `--yes` tracks the pre-checked ones
without asking.

### Tracked directories

`ultnas track --recursive <dir>` tracks every UTF-8 text file under a
directory, and the daemon adopts files created there later: a new file's
first version becomes its stable version, after any invisible characters in
it are stripped. Skipped: hidden files and directories (`.git`, `.env`, …),
editor scratch files (`*~`, `*.swp`, `*.tmp`, `#…#`, …), names given with
`--exclude` (e.g. `--exclude target --exclude node_modules`), binary files,
files over 16 MiB, a vault inside the directory, and anything on a different
filesystem from the directory: tracking `/` covers the root filesystem only.
Track other filesystems (a separate `/home`, say) on their own.

Kernel pseudo-filesystems (`/proc`, `/sys`, `/dev`, `/run`, and on Linux any
proc, sysfs, cgroup, debugfs, … mount) can't be tracked at all: their files
are live kernel state, and "repairing" one would write a kernel setting.
Tracking `/`, a directory outside your home, or one with more than 10,000
candidate files or 1 GiB of them shows a preview and asks first (`--yes`
skips the question). Every tracked file is copied into the vault, so a large
tree costs that much space. Files that already
contain invisible characters when the directory is tracked are listed and
left untracked unless `--accept-existing` is given.

Deleting a file tracked through a directory follows `approval`:

| `approval` | Deleting a file… |
|---|---|
| `"automatic"` | is accepted; the file stops being tracked |
| `"approved"` | is a violation; the file is recreated (`ultnas untrack <file>` to really remove it) |

`ultnas untrack <file>` on a file inside a tracked directory also keeps the
directory from adopting it again; `ultnas untrack <dir>` stops tracking the
directory and every file tracked through it.

Some invisible characters are legitimate next to non-ASCII text (ZWJ in
emoji, ZWNJ in Persian and Indic scripts, variation selectors, subdivision
flags). Those are flagged only between ASCII characters. Characters already in
a file when it is tracked (`--accept-existing`) are part of its baseline and
never count against it.

---

## Conflict Strategies

| Strategy | Behaviour |
|---|---|
| `reject` | Reject ingest if a record with the same label already exists in the namespace |
| `version` | Accept the new record; keep all previous versions; assign a version number |
| `replace` | Accept the new record; move previous version to `__ultnas__/replaced/` |

---

## Retention Policies

Both `keep_versions` and `keep_days` may be set simultaneously. Records that fail **either** condition are eligible for purging. Purging only occurs when the daemon runs its hourly rotation (first a minute after it starts) or when `ultnas purge --rotate` is called (`--dry-run` to preview; it asks before purging unless `--yes`).

- `keep_days` purges records created more than that many days ago.
- `keep_versions` keeps the newest N versions of each *series*: one tracked file's history, or, for other records, one label in the namespace. Two tracked files that share a name are separate series.
- The most specific namespace with a `retention` table applies (`docs` covers `docs/2026`, not `docsX`).
- With `require_seal_before_rotation = true`, only sealed records are purged.
- A tracked file's current stable or pending version is never purged, though it counts toward `keep_versions`.
- Each purge is journaled (`PURGE_RECORD`) before the record is removed, and removed from the mirror too. The daemon skips rotation while its journal is unwritable.

`ultnas purge <id>` removes one record, and refuses a tracked file's current version.

```toml
[namespaces.retention]
keep_versions = 5     # Keep the 5 most recent versions of each label
keep_days     = 730   # Also purge anything older than 2 years
```

---

## Policy Evaluation Points

| When | What Is Checked |
|---|---|
| **Ingest** | `conflict` strategy; `tags_required`; `max_record_size_bytes` |
| **Seal** | `require_seal_before_rotation`; policy is hashed and embedded |
| **Rotation** | `retention.keep_versions`; `retention.keep_days` |
| **Verify** | Custom verify rules (planned for v0.4) |

---

## Example Policies

### Minimal Policy

```toml
version = 1

[global]
conflict = "version"
```

### Personal Archive

```toml
version = 1

[global]
conflict              = "version"
max_record_size_bytes = 524288000   # 500 MiB
require_seal_before_rotation = false

[[namespaces]]
path = "personal/photos"
  [namespaces.retention]
  keep_versions = 100

[[namespaces]]
path = "personal/documents"
tags_required = ["type"]
  [namespaces.retention]
  keep_versions = 20
  keep_days     = 3650   # 10 years
```

### Professional / Compliance Archive

```toml
version = 1

[global]
conflict              = "reject"
max_record_size_bytes = 209715200   # 200 MiB
require_seal_before_rotation = true

[[namespaces]]
path = "legal"
conflict      = "version"
tags_required = ["matter", "classification"]
  [namespaces.retention]
  keep_versions = 999
  keep_days     = 2555   # ~7 years

[[namespaces]]
path = "finance"
conflict      = "version"
tags_required = ["fiscal_year", "document_type"]
  [namespaces.retention]
  keep_days = 2555
```

### Ephemeral Workspace

```toml
version = 1

[global]
conflict = "replace"
require_seal_before_rotation = false

[[namespaces]]
path = "scratch"
  [namespaces.retention]
  keep_versions = 1
  keep_days     = 30
```

---

## Validating a Policy

```bash
ultnas policy validate ./policy/default.toml
```

This will report all structural and semantic errors without modifying the vault.

---

## Policy Hash Embedding

Every time a record is sealed, the BLAKE3 hash of the current policy file is embedded in the `RecordSeal`. This means you can always reconstruct the exact policy that was in effect when a record was sealed — even if the policy has since changed.

To inspect the policy hash for a sealed record:

```bash
ultnas inspect <record-id> --show-seal
# Output includes: policy_hash = "abcdef1234..."
```
