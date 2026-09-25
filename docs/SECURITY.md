# Ultnas — Security Model & Vulnerability Disclosure

> Version: 0.1 | Last Updated: 2026-09-25

---

## Security Model

Ultnas protects text files on one machine from writes that change their
meaning invisibly. It is not a network security product, and it can't stop
an attacker who can already act as you (or as root) from doing anything
you could do, including stopping it.

### What Ultnas Protects Against

| Threat | Mechanism |
|---|---|
| Invisible characters written into a tracked file (zero-width, bidi / "Trojan Source", tag characters, fillers) | Detected on the file-system event and stripped; repeated attempts get the file deleted and recreated from a verified stable copy; repeated restores quarantine the namespace |
| A clean-looking edit riding in with invisible characters | The stripped result is held for approval, even in automatic mode |
| Any unapproved edit or deletion (approved mode) | Clean edits wait as pending; deletions inside tracked directories are undone |
| A tracked file swapped for a symbolic link (e.g. to `/etc/shadow`) or a FIFO | Tracked paths are opened without following links or blocking; anything that isn't a regular file is replaced, and the link's target is never read, written, or copied |
| A link planted at a temp-file name to redirect a write | Temp files are created with `create_new`, never reusing an existing name |
| Overwriting an edit made while the daemon was inspecting the file | Every replacement is abandoned if the file changed since it was read |
| A writer holding the old file open | Delete-and-recreate gives the file a new inode |
| "Repairing" kernel state | `/proc`, `/sys`, `/dev`, `/run` and other pseudo-filesystems can't be tracked; tracked directories never cross filesystems |
| Damaged restore sources | Every copy (memory, vault, mirror) is hash-checked before use |
| Two daemons fighting over one vault | OS file lock on `.ultnas-lock` |
| Other users talking to your daemon | Unix socket mode `0600`; on Windows a local-only named pipe claimed as first instance |
| Partial writes on crash | Atomic `write → fsync → rename` for vault and live-file writes |

### What Ultnas Does NOT Protect Against

| Threat | Notes |
|---|---|
| Look-alike characters (homoglyphs) | A Cyrillic `а` in place of a Latin `a` is visible text, not an invisible character, and isn't detected |
| Clean malicious edits in automatic mode | Anything that can write clean text can change a file, and automatic mode accepts it. Use `approval = "approved"` where that matters |
| An attacker running as you (or as root) | They can stop the daemon, edit the vault, or untrack files. Protect `/etc` with the system service as root |
| Changes made while the daemon isn't running | They are caught at the next start, when each file is compared with its stable copy, not prevented |
| Contexts where the detection is only a heuristic | Context-sensitive characters (ZWJ, ZWNJ, LRM/RLM, variation selectors) are flagged only between ASCII characters; right-to-left text using direction marks next to digits may be flagged |
| Encryption at rest | The vault stores file versions in plaintext. Use full-disk encryption |
| A tampered journal | The journal is append-only by convention, not tamper-evident |

---

## Cryptographic Primitives

| Primitive | Use | Library |
|---|---|---|
| BLAKE3 | Content addressing and every integrity check | `blake3` crate |
| Ed25519 | Record seal signatures (archiving) | `ed25519-dalek` crate |

No custom cryptographic code is written.

---

## Vulnerability Disclosure

**Please do not open a public GitHub Issue for security vulnerabilities.**

### Reporting Process

1. **Email** `sovereignarchivist@outlook.com` with subject: `[SECURITY] Ultnas — <brief description>`
2. Include: affected version, reproduction steps, impact assessment, and any proof-of-concept
3. You will receive an acknowledgment within **48 hours**
4. A fix will be targeted within **14 days** for critical issues, **30 days** for moderate issues
5. You will be credited in the release notes unless you prefer to remain anonymous

Alternatively, use **GitHub's private security advisory** feature:
`https://github.com/thestickybullgod/ultnas/security/advisories/new`

### Severity Classification

| Severity | Examples |
|---|---|
| Critical | Remote code execution, cryptographic bypass, vault data destruction, writing outside a tracked path |
| High | Local privilege escalation, an invisible-character write that isn't repaired, integrity check bypass |
| Moderate | Denial of service, information disclosure, policy bypass |
| Low | Minor information leakage, non-exploitable edge cases |

### Scope

In scope: `ultnas-core`, `ultnas-cli`, `ultnas-daemon`, the vault file format, the IPC protocol.

Out of scope: Third-party dependency vulnerabilities (report upstream), social engineering, and attacks requiring physical device access.
