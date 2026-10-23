# AGENTS.md

Instructions for coding agents working in this repository. Humans: see
[README.md](README.md#development).

## Read first

**[docs/code-guide.md](docs/code-guide.md) is the code guide for this
repository.** Read it before writing or reviewing code, and follow it. Its §0
checklist is the minimum bar for every change; cite rules by section
(`code-guide §7.4`) in reviews.

## Project

bolt-load is an embeddable, async, runtime-agnostic download library (Rust 2024,
MSRV 1.88). Every crate except `bolt-load-tests` is published library code, so
public API changes are API design decisions. See the [README](README.md) for
architecture and the crate table, and each crate's `README.md` / `DESIGN.md` /
`docs/*.md` for subsystem design.

## Commands

```bash
cargo fmt --all                                           # format (nightly rustfmt)
cargo clippy --all-targets --all-features -- -D warnings  # lint
cargo test                                                # test
# bolt-load-sync protocols: loom model check
RUSTFLAGS="--cfg loom" CARGO_TARGET_DIR=target/loom \
  cargo test -p bolt-load-sync --release --test loom
```

Run fmt, clippy and the affected tests before reporting a change as done.

## Rules agents most often get wrong

The guide is authoritative; these are reminders, not a replacement.

- Spawn, sleep and block through the runtime traits; no direct `tokio::*` in
  library code outside the runtime modules (§8.3).
- Suppress lints with `#[expect(lint, reason = "...")]`, not `#[allow]` (§10.1).
- `expect("<invariant>")` instead of `unwrap()` in non-test code (§7.4).
- `// SAFETY:` on every unsafe site (§9.2).
- Library errors are `snafu` enums, not `anyhow` (§7.2).
- Comment key logic only, in English; no comments that narrate the change (§11).
- Breaking changes go in directly with `!` and a `BREAKING CHANGE:` footer; no
  `#[deprecated]` step (§14).
- Match the style of the surrounding code; do not reformat or restructure code
  unrelated to the task.
