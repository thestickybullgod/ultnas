# Ultnas — Contributor Onboarding

> Welcome! This guide gets you from zero to a working dev environment in under 10 minutes.

---

## 1. Prerequisites

| Tool | Required Version | Install |
|---|---|---|
| Rust (stable) | ≥ 1.89 | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` |
| Git | ≥ 2.40 | OS package manager |
| clippy | latest | `rustup component add clippy` |
| rustfmt | latest | `rustup component add rustfmt` |
| cargo-nextest | optional | `cargo install cargo-nextest` |

---

## 2. Fork & Clone

```bash
# 1. Fork https://github.com/thestickybullgod/ultnas on GitHub
# 2. Clone your fork
git clone https://github.com/YOUR_USERNAME/ultnas.git
cd ultnas

# 3. Add the upstream remote
git remote add upstream https://github.com/thestickybullgod/ultnas.git
```

---

## 3. Build

```bash
# Build all crates
cargo build --workspace

# Build release binaries
cargo build --workspace --release

# Binaries land at:
#   ./target/release/ultnas      (CLI)
#   ./target/release/ultnasd     (daemon)
```

---

## 4. Run the Test Suite

```bash
# Run all tests
cargo test --workspace

# Or with nextest (faster, better output)
cargo nextest run --workspace

# Run doc tests only
cargo test --workspace --doc
```

---

## 5. Lint & Format

```bash
# Check formatting (does NOT modify files)
cargo fmt --all --check

# Auto-format
cargo fmt --all

# Lint
cargo clippy --workspace --all-targets -- -D warnings
```

---

## 6. First Steps in the Codebase

Start here:

1. **`ultnas-core/src/lib.rs`** — crate root; see what's exported
2. **`ultnas-core/src/address.rs`** — simplest module; great entry point
3. **`ultnas-core/src/record.rs`** — central data model
4. **`docs/ARCHITECTURE.md`** — system design
5. **`docs/MODULE_SPECS.md`** — per-module contracts

---

## 7. Typical Contribution Workflow

```bash
# 1. Sync with upstream
git fetch upstream
git checkout main
git merge upstream/main

# 2. Create a feature branch
git checkout -b feat/core-my-feature

# 3. Make your changes
# ... edit files ...

# 4. Run the check suite
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# 5. Commit (Conventional Commits style)
git commit -m "feat(core): add my feature"

# 6. Push and open a PR
git push origin feat/core-my-feature
```

---

## 8. Project Conventions

- **Error handling:** `thiserror` in `ultnas-core`, `anyhow` in CLI/daemon
- **Async:** only in `ultnas-daemon` (tokio). `ultnas-core` is fully synchronous.
- **Logging:** use `tracing::debug!` / `tracing::info!` / `tracing::warn!` / `tracing::error!`
- **Doc comments:** all public items must have `///` docs with at least one sentence

---

## 9. FAQ

**Q: The build fails with "error: package `foo` not found"**
A: Run `cargo update` to refresh the lock file.

**Q: clippy is failing on my machine but not in CI**
A: Make sure you're on the same Rust version: `rustup update stable && rustup override set stable`

**Q: Can I use nightly features?**
A: No. `ultnas-core` and `ultnas-cli` target stable Rust only. Nightly is never required.

**Q: Where do I ask questions?**
A: Open a GitHub Discussion in the `Q&A` category.
