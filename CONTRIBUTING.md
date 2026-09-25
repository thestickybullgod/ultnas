# Contributing to Ultnas

Thank you for your interest! This document covers everything you need — from setup to submitting a PR.

---

## Table of Contents
1. [Code of Conduct](#code-of-conduct)
2. [Ways to Contribute](#ways-to-contribute)
3. [Development Setup](#development-setup)
4. [Coding Standards](#coding-standards)
5. [Testing](#testing)
6. [Commit Style](#commit-style)
7. [Pull Request Process](#pull-request-process)
8. [Security Vulnerabilities](#security-vulnerabilities)

---

## Code of Conduct

All contributors must abide by [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

---

## Ways to Contribute

- **Bug reports** — GitHub Issue with label `bug` + reproduction steps
- **Feature requests** — GitHub Issue with label `enhancement`; describe the problem, not just the feature
- **Documentation** — `/docs`, inline comments, or README improvements
- **Design discussion** — Architecture and API proposals belong in GitHub Discussions
- **Code** — Bug fixes, perf improvements, new features

---

## Development Setup

### Prerequisites

| Tool | Version | Install |
|---|---|---|
| Rust (stable) | ≥ 1.80 | `rustup update stable` |
| clippy | latest | `rustup component add clippy` |
| rustfmt | latest | `rustup component add rustfmt` |
| cargo-nextest | optional | `cargo install cargo-nextest` |

### Clone & Build

```bash
git clone https://github.com/YOUR_USERNAME/ultnas.git
cd ultnas
git remote add upstream https://github.com/sovereignarchivist/ultnas.git
cargo build --workspace
```

### Check Suite (run before every push)

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

---

## Coding Standards

- All public items must have `///` doc comments.
- Use `thiserror` for error types in library code; `anyhow` is allowed in CLI/daemon.
- No bare `unwrap()` in library code. Use `expect("reason")` only in tests.
- Every `unsafe` block requires a `// SAFETY:` comment and two reviewer approvals.
- New dependencies need a brief justification in the PR description. No GPL/AGPL/SSPL deps.

---

## Testing

| Type | Location | Command |
|---|---|---|
| Unit | `#[cfg(test)]` in source | `cargo test --lib` |
| Integration | `tests/` per crate | `cargo test --test '*'` |
| Doc tests | `///` examples | `cargo test --doc` |

- All new public functions need at least one unit test.
- All bug fixes need a regression test.
- Tests must be deterministic — no timing, randomness, or network dependencies.

---

## Commit Style

We follow [Conventional Commits](https://www.conventionalcommits.org/).

```
<type>(<scope>): <short description>
```

**Types:** `feat` | `fix` | `docs` | `refactor` | `perf` | `test` | `chore` | `security`

**Scopes:** `core` | `cli` | `daemon` | `policy` | `docs` | `ci` | `deps`

**Examples:**
```
feat(core): add BLAKE3 content addressing for record sealing
fix(cli): correct inspect output when record has no metadata
docs(policy): add retention policy examples
chore(deps): upgrade blake3 to 1.5.4
```

---

## Pull Request Process

1. Branch from `main`: `git checkout -b feat/core-namespace-resolution`
2. Write/update tests covering your changes
3. Update relevant `/docs` if public API or behavior changed
4. Run the full check suite locally
5. Open PR against `main`; fill out the PR template completely
6. Address review feedback within 14 days; PRs dormant for 30 days will be closed

**Review requirements:**
- 1 approving maintainer review for all PRs
- 2 reviews for any `unsafe` code
- Prior design discussion required for storage format or public API changes

---

## Security Vulnerabilities

**Do not open a public Issue.** See [docs/SECURITY.md](docs/SECURITY.md) for responsible disclosure.
