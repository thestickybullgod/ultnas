<h1 align="center">Ultnas — Universal Linux Text Normalizer and Sanitizer</h1>

<p align="center">
  <strong>Sovereign, policy-driven digital archiving and namespace sovereignty — built in Rust.</strong>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/rust-2021%20edition-orange?style=flat-square" alt="Rust"/>
  <img src="https://img.shields.io/badge/license-MIT-blue?style=flat-square" alt="MIT License"/>
  <img src="https://img.shields.io/badge/status-pre--alpha-red?style=flat-square" alt="Pre-Alpha"/>
</p>

---

## What Is Ultnas?

**Ultnas** (*ultima nascentem* — "the last that is born") is a sovereign archiving and namespace management system.
It gives individuals and small organizations full, auditable ownership over their digital records — files, metadata,
policies, and provenance — without depending on any cloud provider, platform, or third-party authority.

| Principle | Meaning |
|---|---|
| **Sovereignty** | You own your namespace. No vendor can revoke, restrict, or shadow-delete your archive. |
| **Permanence** | Records are content-addressed (BLAKE3), cryptographically sealed, and built for multi-decade retention. |
| **Legibility** | Every policy, rule, and decision is expressed in plain, human-readable TOML. |

---

## Repository Layout

```
ultnas/
├── ultnas-core/        # Core library: content addressing, sealing, namespace, policy eval
├── ultnas-cli/         # CLI: archive, retrieve, inspect, verify, manage
├── ultnas-daemon/      # Daemon: watch, sync, rotate, enforce policy
├── docs/               # Full documentation suite (6 export-ready docs)
├── policy/             # Starter and example policy TOML files
└── examples/           # Runnable usage examples
```

---

## Quick Start

```bash
# Prerequisites: Rust 1.80+
git clone https://github.com/thestickybullgod/ultnas.git
cd ultnas
cargo build --workspace --release

# Initialize a vault
./target/release/ultnas init --name "my-archive"

# Archive a file
./target/release/ultnas add ./important-document.pdf

# Inspect a sealed record
./target/release/ultnas inspect <record-id>

# Verify all records
./target/release/ultnas verify --all

# Start the daemon
./target/release/ultnasd --policy ./policy/default.toml --vault ./my-archive
```

---

## Documentation

| Document | Description |
|---|---|
| [ARCHITECTURE.md](docs/ARCHITECTURE.md) | System design, component boundaries, data flows |
| [MODULE_SPECS.md](docs/MODULE_SPECS.md) | Per-crate module specs and public API contracts |
| [POLICY_GUIDE.md](docs/POLICY_GUIDE.md) | Writing, validating, and applying policy files |
| [API_REFERENCE.md](docs/API_REFERENCE.md) | Full `ultnas-core` public API reference |
| [ONBOARDING.md](docs/ONBOARDING.md) | Contributor onboarding and dev environment setup |
| [SECURITY.md](docs/SECURITY.md) | Security model, threat model, and vulnerability disclosure |

---

## Roadmap

| Version | Goal |
|---|---|
| `v0.1.0` | Core content-addressing + basic CLI (`add`, `inspect`, `verify`) |
| `v0.2.0` | Namespace resolution and sealed records |
| `v0.3.0` | Daemon with watch-based ingestion and rotation |
| `v0.4.0` | Policy evaluation engine |
| `v0.5.0` | Stable storage format; backward-compat guaranteed |
| `v1.0.0` | Production-ready release |

---

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) before submitting anything. All participants must follow [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

---

## License

MIT — see [LICENSE](LICENSE).
