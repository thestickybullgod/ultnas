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

[[namespaces]]
path     = "legal"
approval = "approved"   # overrides the global setting for legal/ and below
```

A namespace's `approval` applies to it and every namespace under it; the most
specific one wins. Daemon flags such as `--violation-threshold` override the
policy's values.

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

Both `keep_versions` and `keep_days` may be set simultaneously. Records that fail **either** condition are eligible for purging. Purging only occurs when the daemon runs a scheduled rotation or when `ultnas purge --rotate` is called.

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
