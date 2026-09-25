<h1 align="center">Ultnas — Universal Linux Text Normalizer and Sanitizer</h1>

<p align="center">
  <strong>Protects your text files from invisible-character tampering — built in Rust.</strong>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/rust-1.89%2B-orange?style=flat-square" alt="Rust 1.89+"/>
  <img src="https://img.shields.io/badge/license-MIT-blue?style=flat-square" alt="MIT License"/>
  <img src="https://img.shields.io/badge/status-pre--alpha-red?style=flat-square" alt="Pre-Alpha"/>
</p>

---

## What Is Ultnas?

Some Unicode characters render as nothing at all, or silently reorder the text
around them. Written into a file, they change what the file *means* without
changing what a person reviewing it *sees*:

- `is_admin` with a zero-width space after the underscore looks identical,
  but is a different identifier.
- Bidi controls can make code read one way on screen and compile another
  (the "Trojan Source" attack).
- Tag characters and fillers can hide text inside a config file or an
  `authorized_keys` line.

Ultnas watches the files you choose — shell startup files, SSH and Git
config, scripts, source trees, `/etc` — and undoes any write that slips
these characters in, usually within about a tenth of a second.

## How It Works

The daemon, `ultnasd`, keeps a verified **stable copy** of every tracked file
(in memory and in its vault) and reacts to file-system events:

1. **An ordinary edit** — no invisible characters added — is fine. Depending
   on your `approval` setting it becomes the new stable copy at once, or waits
   as *pending* until you run `ultnas approve`.
2. **A write that adds invisible characters** is prevented: the daemon strips
   exactly the characters that write introduced, keeping the file's
   permissions. If the same write also changed visible text, the stripped
   result is held for approval rather than trusted.
3. **Repeated attempts** — `write_violation_threshold` (default 5) within the
   window — and the file is **deleted and recreated** from its stable copy. The
   new file is a new inode, so a writer still holding the old one open is cut
   off.
4. **Repeated restores** quarantine the namespace: the daemon keeps detecting
   and journaling but stops fighting, until you lift it.

Every action is recorded in an append-only journal. Characters with legitimate
uses next to non-ASCII text (ZWJ in emoji, ZWNJ in Persian and Indic scripts,
variation selectors, flag tags) are allowed there; characters already in a file
when you start tracking it are part of its baseline and never count against it.

## Quick Start

```bash
# Prerequisites: Rust 1.89+
git clone https://github.com/thestickybullgod/ultnas.git
cd ultnas
cargo build --workspace --release
export PATH="$PWD/target/release:$PATH"

# Create a vault. A hidden directory in your home keeps it out of the way
# (and out of any home directory you track).
ultnas --vault ~/.ultnas init --name "$USER"

# Pick recommended places to protect from a checklist
ultnas --vault ~/.ultnas setup

# Start the daemon
ultnasd --vault ~/.ultnas &

# Check on it
ultnas --vault ~/.ultnas daemon status
```

`--vault` defaults to the current directory, so running the commands from
inside the vault works too.

## Choosing What to Protect

`ultnas setup` offers what exists on your machine, with the recommended items
pre-checked:

| Suggested | Mode |
|---|---|
| `~/.bashrc`, `~/.profile`, `~/.zshrc`, … | file |
| `~/.ssh/config`, `~/.ssh/authorized_keys` | file |
| `~/.gitconfig` | file |
| `~/bin`, `~/.local/bin` | recursive |
| `~/src`, `~/dev`, `~/projects`, … (skipping `target`, `node_modules`, `venv`, …) | recursive |
| `~/.config` (offered, not pre-checked: it churns) | recursive |
| `/etc` (as root) | recursive |

Or track things yourself:

```bash
ultnas track ~/.bashrc
ultnas track --recursive ~/src --exclude target --exclude node_modules
ultnas tracked                 # what's protected, and each file's status
ultnas untrack ~/src           # stop (versions stay in the vault)
```

A tracked directory also adopts files created in it later. Hidden files,
editor scratch files, binary files, files over 16 MiB, and other filesystems
mounted below it are skipped. Kernel pseudo-filesystems (`/proc`, `/sys`,
`/dev`, `/run`) can't be tracked at all. Tracking `/`, a directory outside
your home, or a very large one shows a preview and asks first.

## Approval: Automatic or Approved

```toml
# policy.toml — start the daemon with --policy policy.toml
version = 1

[global.integrity]
approval = "automatic"          # clean edits become the stable copy at once

[[namespaces]]
path = "etc"
approval = "approved"           # clean edits here wait for `ultnas approve`
```

| `approval` | A clean edit… | Deleting a file inside a tracked directory… |
|---|---|---|
| `"automatic"` (default) | becomes the stable copy immediately | is accepted; the file stops being tracked |
| `"approved"` | stays on disk as pending until `ultnas approve <file>` | is undone; the file is recreated |

Other settings: `write_violation_threshold`, `escalate_after_restores`,
`mirror` (a second copy of everything, ideally on another disk), and
`[namespaces.retention]` (`keep_versions`, `keep_days`) to bound how many old
versions are kept. See the [Policy Guide](docs/POLICY_GUIDE.md).

## Day-to-Day Commands

| Command | What it does |
|---|---|
| `ultnas setup` | Checklist of recommended places to protect |
| `ultnas track [-r] <path>` / `untrack <path>` | Start or stop protecting a file or directory |
| `ultnas tracked` | List tracked files and directories with their status |
| `ultnas approve <file>` | Promote a pending edit to the stable copy |
| `ultnas daemon status [--json]` | Live state: journal health, quarantines, watcher, recent alerts |
| `ultnas daemon stop` | Stop the daemon cleanly |
| `ultnas integrity status` / `violations` | What the journal says has happened |
| `ultnas integrity lift-quarantine <ns>` | Resume protection of a quarantined namespace |
| `ultnas purge --rotate [--dry-run]` | Apply the retention rules now |

The daemon logs to `<vault>/logs/ultnasd.<date>.log` (rotated daily) as well as
stdout, and only one daemon can run per vault.

## Platforms

Ultnas is built for Linux, and CI also runs it on macOS and Windows. It uses
inotify on Linux, FSEvents on macOS, and ReadDirectoryChangesW on Windows; the
CLI talks to the daemon over a Unix socket (a local-only named pipe on
Windows). On Linux, very large tracked trees may need a higher
`fs.inotify.max_user_watches`; `ultnas daemon status` reports directories it
couldn't watch, and a full scan every 5 minutes still covers them.

## Also: Sealed Archiving

Ultnas grew out of a content-addressed archiver, and that part remains:
`ultnas add` stores a file in the vault under its BLAKE3 hash, records can be
sealed, and the daemon repairs tampered sealed records too (from memory, or
the mirror if one is configured). See `ultnas add`, `inspect`, `verify`, and
`ls`.

## Repository Layout

```
ultnas/
├── ultnas-core/     # Library: invisible-character detection, tracking, vault, journal, policy, IPC
├── ultnas-cli/      # `ultnas`: setup, track, approve, status, purge, …
├── ultnas-daemon/   # `ultnasd`: watcher, integrity guard, scheduler, IPC server
├── docs/            # Architecture, policy guide, security model, onboarding
└── policy/          # Example policy files
```

## Documentation

| Document | Description |
|---|---|
| [POLICY_GUIDE.md](docs/POLICY_GUIDE.md) | Every policy setting: approval, thresholds, mirror, retention, tracked directories |
| [ARCHITECTURE.md](docs/ARCHITECTURE.md) | Components, storage layout, IPC protocol, security boundaries |
| [SECURITY.md](docs/SECURITY.md) | Security model, threat model, and vulnerability disclosure |
| [ONBOARDING.md](docs/ONBOARDING.md) | Contributor onboarding and dev environment setup |
| [MODULE_SPECS.md](docs/MODULE_SPECS.md), [API_REFERENCE.md](docs/API_REFERENCE.md) | What each module does and guarantees; where to start with the API (`cargo doc` for the full reference) |
| [TESTING_LINUX.md](TESTING_LINUX.md) | Step-by-step guide to testing the Linux package |
| [CHANGELOG.md](CHANGELOG.md) | What changed |

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) before submitting anything. All
participants must follow [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

```bash
cargo build --workspace          # the end-to-end tests need ultnasd built
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## License

MIT — see [LICENSE](LICENSE).
