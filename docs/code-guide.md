# bolt-load Code Guide

This guide is for everyone who changes code in this repository, human or agent.
It adapts the [Google Rust Style Guide][google] to bolt-load: an embeddable,
async, runtime-agnostic download library. Where this guide and the Google guide
disagree, this guide wins; each such case is marked **Deviation** with the
reason.

Rules are numbered so reviews can cite them (`see code-guide §4.2`).

## Contents

- [0. Checklist](#0-checklist)
- [1. Principles](#1-principles)
- [2. Tooling and formatting](#2-tooling-and-formatting)
- [3. Code organization](#3-code-organization)
- [4. Imports](#4-imports)
- [5. Naming](#5-naming)
- [6. API design](#6-api-design)
- [7. Errors and panics](#7-errors-and-panics)
- [8. Ownership, async and concurrency](#8-ownership-async-and-concurrency)
- [9. Unsafe code](#9-unsafe-code)
- [10. Lints](#10-lints)
- [11. Documentation and comments](#11-documentation-and-comments)
- [12. Implementation details](#12-implementation-details)
- [13. Tests and benchmarks](#13-tests-and-benchmarks)
- [14. Commits and breaking changes](#14-commits-and-breaking-changes)

## 0. Checklist

Before sending a change, confirm:

- [ ] `cargo fmt --all -- --check` passes.
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` passes.
- [ ] `cargo test` passes; for `bolt-load-sync` changes, the loom models pass
      too (§13.4).
- [ ] No new `#[allow]`, only `#[expect(lint, reason = "...")]` (§10.1).
- [ ] Every `unsafe` block or impl has a `// SAFETY:` comment (§9.2).
- [ ] Non-test code uses `expect("<invariant>")`, not bare `unwrap()` (§7.4).
- [ ] Library code reaches the runtime through the runtime traits, never
      `tokio::spawn` directly (§8.3).
- [ ] New public enums and option structs are `#[non_exhaustive]` (§6.6).
- [ ] Public items have doc comments; comments are in English (§11).
- [ ] The commit message follows Conventional Commits; breaking changes carry
      `!` and a `BREAKING CHANGE:` footer (§14).

## 1. Principles

These come from the Google guide's goals and apply to every rule below.

1. **Readability over writeability.** Code is read far more often than it is
   written; a little extra effort when writing is worth it.
2. **Automate what can be automated.** If rustfmt or clippy can enforce a rule,
   the tool owns it and reviews do not discuss it.
3. **Stay close to the Rust ecosystem.** Follow the official
   [Rust Style Guide][rust-style] and the [Rust API Guidelines][api-guidelines]
   unless this guide says otherwise.
4. **Rules must pull their weight.** Do not add rules here that only settle
   taste.
5. **This is library code.** All workspace crates except `bolt-load-tests`, the
   examples and the benches are libraries consumed by other projects, so the API
   design rules (§6) are binding, not advisory. Tests, examples and benches may
   favor directness over generality.

## 2. Tooling and formatting

### 2.1 rustfmt owns formatting

Run `cargo fmt --all`. The repository's `rustfmt.toml` is authoritative; it
needs nightly rustfmt, which `rust-toolchain.toml` selects. Notable settings:

| Setting               | Value              | Effect                                                    |
| --------------------- | ------------------ | --------------------------------------------------------- |
| `max_width`           | `100`              | Same as the Google guide.                                 |
| `imports_granularity` | `Crate`            | One `use` per crate, see §4.1.                            |
| `group_imports`       | `StdExternalCrate` | `std`, then external crates, then `crate`/`super`/`self`. |
| `format_strings`      | `true`             | Long string literals are wrapped.                         |

### 2.2 Opting out of formatting

`#[rustfmt::skip]` is allowed only for deliberate manual alignment (for example
a table of test vectors) or generated code, never for personal taste.

### 2.3 Toolchain and MSRV

- MSRV is **1.88**, edition **2024** (see the workspace `Cargo.toml`). Language
  and library features used in library code must be stable on 1.88.
- Nightly is used only for rustfmt and for opt-in probes. **Do not** add
  `#![feature(...)]`.
- Probes for nightly or unstable behavior (for example
  `cfg(bolt_load_probe_send)`, `cfg(tokio_unstable)`) live behind a custom
  `cfg`, which must be registered in the crate's `[lints.rust] unexpected_cfgs`
  table and explained in a comment next to it.

## 3. Code organization

### 3.1 Crate boundaries

Each crate has one job; put new code in the crate that owns that job and keep
the dependency direction:

| Crate               | Owns                                           | May depend on           |
| ------------------- | ---------------------------------------------- | ----------------------- |
| `bolt-load-core`    | `BoltLoadAdapter` trait, adapter error types   | —                       |
| `bolt-load-sync`    | Runtime-agnostic synchronization primitives    | `std`, `event-listener` |
| `bolt-load-utils`   | HTTP header parsing, readers, telemetry facade | —                       |
| `bolt-load-adapter` | Concrete adapters (reqwest, ureq)              | core, utils             |
| `bolt-load`         | The engine: task, runner, runtime, writers     | all of the above        |
| `bolt-load-tests`   | Test adapters and fixtures (`publish = false`) | core, utils             |

- Low-level primitives that need loom model checking belong in `bolt-load-sync`,
  not inside `bolt-load`.
- Keep runtime crates (`tokio`, `smol`, `compio`) behind features in
  `bolt-load`; `bolt-load-core` and `bolt-load-sync` must not depend on any
  async runtime.
- Prefer a new small crate over growing `bolt-load` when the code is a
  self-contained abstraction with its own tests (the Google guide's "prefer
  small crates").

### 3.2 Module files

**Deviation:** the Google guide's flat, single-directory layout is
Chromium-specific. We use the standard Cargo layout.

- New modules with submodules use `foo.rs` plus a `foo/` directory, not
  `foo/mod.rs` (as `run_loop.rs` + `run_loop/` and `file_writer.rs` +
  `file_writer/` do). Existing `mod.rs` files may stay; do not churn them in
  unrelated changes.
- Name a file after its primary item: `chunk_planner.rs` holds `ChunkPlanner`.
- A parent module should mostly declare submodules and re-export; keep large
  implementations in leaf modules. A small amount of glue code in the parent is
  fine.
- Large test suites go in a sibling file (`tests.rs`, `*_tests.rs`) declared
  with `#[cfg(test)] mod tests;`; small ones may stay inline at the bottom of
  the file.

### 3.3 Visibility

Use the narrowest visibility that works: private, then `pub(super)` or
`pub(in crate::path)`, then `pub(crate)`, then `pub`. Anything `pub` in a
published crate is API surface (§6).

## 4. Imports

### 4.1 Granularity

**Deviation:** the Google guide wants one module per `use`
(`imports_granularity = Module`). We use `Crate`, enforced by rustfmt, so nested
braces are expected:

```rust
use std::{
    fmt,
    task::{Context, Poll},
};

use crate::{atomic_waker::AtomicWaker, sync::Arc};
```

### 4.2 Glob imports

Do not use glob imports, except:

- `use super::*;` in an inline `mod tests`.
- `use Enum::*;` inside a function that mostly matches on that enum.
- `pub use submodule::*;` in a parent module that re-exports its leaves.
- `use bolt_load_utils::telemetry::*;`: the logging facade, which becomes no-op
  macros when the `tracing` feature is off.
- Trait-only preludes of third-party crates that are designed for it (for
  example `async_ringbuf::traits::*`, `futures_concurrency::prelude::*`).

Do not define new preludes.

### 4.3 Types versus functions

- Import types and traits and use them unqualified: `Bytes`, not `bytes::Bytes`.
  Qualify when names collide (`io::Error`, `fmt::Result`) or rename
  (`use std::io::Error as IoError`).
- Call free functions through their module: `mem::take(&mut x)`,
  `slot::channel()`, not `take(&mut x)` with an imported function. Unqualified
  lowercase calls are then always local.
- Import traits used in one function inside that function.

## 5. Naming

Follow the [Rust API Guidelines naming rules][api-naming]. In addition:

- **Conversions:** `as_` (cheap borrow), `to_` (expensive or owned), `into_`
  (consumes `self`); clippy's `wrong_self_convention` enforces this.
- **Constructors:** `new()`; `new_with_foo()` or a builder when there is more
  than one way; `try_new()` when it returns `Result`. Pair an important `From`
  impl with an inherent `from_foo()`.
- **Accessors:** `foo()`, `foo_mut()`, `set_foo()`, `take_foo()`,
  `replace_foo()`, `with_foo(|foo| ...)` for scoped access to interior state. No
  `get_` prefix.
- **Predicates:** `is_`, `has_`, `can_` (`TaskError::is_retryable`).
- **Extension traits** end in `Ext` (`ThreadedRuntimeExt`).
- **Lifetimes:** `'_` where it works; single letters only to relate inputs to
  outputs; otherwise descriptive (`'buf`).
- **Generic parameters:** short and indicative of the bound
  (`A: BoltLoadAdapter`, `R: ThreadedRuntime`); `T`/`U` only for truly generic
  code.
- **Abbreviations:** avoid project-internal ones. Common ones (`rx`, `tx`, `id`,
  `buf`, `rt`) are fine.

## 6. API design

### 6.1 Parameters

- Borrow the data, not the container: `&str` not `&String`, `&[T]` not
  `&Vec<T>`, `Option<&T>` not `&Option<T>`, `&T` not `&Box<T>`.
- Accept `impl IntoIterator` when the function only iterates.
- Use `impl Into<T>` only where call-site ergonomics clearly matter (builder
  setters); it costs code size and inference.
- Use an `enum` instead of a `bool` parameter whose meaning is not obvious at
  the call site.

### 6.2 Return values

Prefer returning an iterator over a `Vec` when callers may not need ownership.
Return a `Vec` when callers need indexing or ownership anyway, or when the
function naturally builds one.

### 6.3 Generics versus `dyn`

Prefer generics in hot paths and function parameters. Use type erasure where it
serves the user: `bolt-load` erases adapters and runtimes (`AnyAdapter`,
`ThreadedRuntimeImpl::Other`) so builders stay simple. For traits with
`async fn`, generate the `dyn` form with `dynosaur`, as `BoltLoadAdapter` does,
instead of hand-writing boxed futures.

### 6.4 Traits

- Define a trait only for generic programming, for `impl Trait`/`dyn Trait`, or
  as an extension trait. Do not use traits to organize code.
- Implement standard traits instead of look-alike methods (`FromStr`, not
  `fn from_str`; `Default` alongside `new()`).
- Prefer `#[derive]` over manual impls. `Debug` on every public type; use
  `derive_more::Debug` or `finish_non_exhaustive()` to hide fields.
- Implement `Clone`/`Copy`/`PartialEq`/`Eq`/`Hash` when it is clear the type
  will keep supporting them; removing one later is a breaking change.
- Implement expected sets together: `len()` with `is_empty()`.

### 6.5 Types

- Use a newtype instead of a type alias for new types whose values are easy to
  mix up with other integers (an id versus a byte offset) or that need their own
  API.
- Prefer named fields over tuples, except for newtypes and very local code.
- Option structs implement `Default`.

### 6.6 Stability attributes

- Public enums and public structs with public fields get `#[non_exhaustive]` (as
  `TaskOptions` and `MetaRetry` do), unless the set is closed by nature.
- When matching a foreign `#[non_exhaustive]` enum, list the variants you handle
  and put the rest in `_`.
- `#[must_use]` on functions whose result is the point (pure functions, a `bool`
  reporting failure) and on lazy types. Give a reason when it is not obvious:
  `#[must_use = "`false`means the receiver is gone"]`.
- Mark a public function `const fn` only if it will stay `const`.
- Treat `#[doc(hidden)]` items of other crates as private.

## 7. Errors and panics

### 7.1 Choosing the mechanism

- `Option` when absence is the only failure and needs no explanation.
- `Result` for failures the caller can anticipate and handle (I/O, network, bad
  server responses).
- `panic!` only for bugs: a broken invariant or a violated precondition that the
  docs state.

### 7.2 Error types

- Library errors are `snafu` enums. **Do not** use `anyhow` or `Box<dyn Error>`
  in library code; `anyhow` is acceptable in tests, examples and benches.
- `display` messages are lowercase with no trailing period
  (`"task is cancelled"`), and include the source when there is one
  (`"stream error: {source}"`).
- Keep the retryable / unretryable split: adapters classify errors as
  `AdapterError::Retryable` or `Unretryable`, and retry logic decides from that
  classification (`is_retryable()`), never by matching on message text.
- Make error types `Clone` when they are fanned out to several observers; wrap a
  non-`Clone` source in `Arc`.

### 7.3 Panic documentation

A public function that can panic documents it under `# Panics`.

### 7.4 `unwrap` and `expect`

- In non-test code, prefer pattern matching that proves the case (`if let`,
  `let ... else`) over check-then-unwrap.
- When a panic is the right answer, use `expect` with the invariant that makes
  it impossible, phrased as a fact: `.expect("a frame is pushed once")`. Bare
  `unwrap()` in non-test code is not accepted in new code.
- `unwrap()` is fine in tests, benches and examples.
- Never unwrap a lock: `std::sync::Mutex` poisoning is either recovered
  explicitly (`unwrap_or_else(PoisonError::into_inner)`, when the data stays
  valid) or avoided with a non-poisoning lock. Never reach inside a poisoned
  mutex without deciding that its data is still consistent.

### 7.5 `catch_unwind`

Do not use `panic::catch_unwind` in library code. Tests may use it to assert
that something panics or does not panic.

## 8. Ownership, async and concurrency

**Deviation:** the Google guide forbids `async` in Chromium. bolt-load is an
async library, so this section replaces those rules.

### 8.1 Ownership

- Prefer unique ownership and borrows. Do not `.clone()` to silence the borrow
  checker when a borrow works; cloning an `Arc` or `Bytes` handle is fine.
- `Arc` is expected for state shared across tasks or threads. Before adding a
  new `Arc<Mutex<_>>`, consider whether an owner plus a channel works: the
  download loop owns its state and runners talk to it through channels and
  `bolt-load-sync` primitives.
- `Rc` is limited to single-threaded local runtimes.

### 8.2 Hot data paths

- Move payloads as `Bytes`; do not copy them into `Vec<u8>`.
- Keep the backpressure-aware channels (ring buffers) on the data path; do not
  replace them with unbounded channels.
- Performance claims in a PR need numbers from the criterion benches in
  `crates/bolt-load/benches`.

### 8.3 Runtime-agnostic code

- Library code spawns, sleeps and blocks only through the runtime traits
  (`ThreadedRuntime`, `LocalRuntime` and their implementations). Direct
  `tokio::*` or `smol::*` calls belong only in the runtime implementation
  modules, behind their feature flag, and in tests.
- New synchronization primitives must not assume a runtime; build them on `std`,
  atomics and `event-listener`, as `bolt-load-sync` does.
- Futures handed to a threaded runtime must be `Send`. If a change makes a
  future `!Send`, fix the cause instead of adding a local-runtime workaround.

### 8.4 Cancellation and termination

- Every long-running task observes a cancellation token and ends in a defined
  terminal state; a task must never hang after cancellation or after its peers
  fail.
- Do not hold a lock guard across `.await`.
- Prefer polling several event sources in one hand-written `poll` (as the
  download and runner loops do) over rebuilding `select!` futures each iteration
  on hot paths.

### 8.5 Atomics

- Use `Ordering::Relaxed` only for counters and statistics that do not publish
  other memory. Anything that hands over data uses `Acquire`/`Release` or
  stronger, with a comment naming the matching side.
- Lock-free protocols live in `bolt-load-sync`, import their atomics through its
  `sync` module so loom can replace them, and come with a loom model (§13.4).

## 9. Unsafe code

### 9.1 When

Do not write `unsafe` when a safe equivalent exists. Current legitimate uses are
memory-mapped file I/O and its OS calls, and the `UnsafeCell` inside
`bolt-load-sync` primitives. A new unsafe site needs a reason in the PR
description: why safe code does not work, and benchmarks if the reason is
performance.

### 9.2 How

- Encapsulate unsafe code in a small safe abstraction, away from business logic.
- Every `unsafe` block, `unsafe impl` and `unsafe fn` call has a `// SAFETY:`
  comment directly above it that says why the requirements hold:

  ```rust
  // SAFETY: `waker` is only accessed by the side that moved `state` out of
  // `WAITING`, so it is never accessed from two threads at once.
  unsafe impl Send for AtomicWaker {}
  ```

- An `unsafe fn` or `unsafe trait` documents its contract under `# Safety`.
- Keep each `unsafe` block as small as the operation that needs it.

## 10. Lints

### 10.1 Suppressing a lint

Use `#[expect(lint, reason = "...")]`, never `#[allow]`, so that a suppression
that is no longer needed fails the build:

```rust
#[expect(dead_code, reason = "reserved for persisting runner states")]
```

`#[allow]` is acceptable only where `#[expect]` cannot work, for example when a
macro copies the attribute into code that triggers it differently, or when the
lint fires only under some feature combinations; say why in a comment.

Do not delete code or a suppression just because one build reports
`unfulfilled_lint_expectations`; check the other feature combinations first.

### 10.2 Clippy

Clippy runs with `-D warnings` over all targets and features (the pre-commit
hook does this). Fix the warning rather than suppressing it, unless the lint is
wrong for the case, and then use §10.1.

## 11. Documentation and comments

### 11.1 Language

Write all code comments, doc comments and documentation in **English**.

### 11.2 Doc comments

- Every public item has a `///` doc comment; modules and crates get `//!` docs.
  Long module docs live in `docs/*.md` next to the crate and are included with
  `#![doc = include_str!("...")]`.
- The first line is a one-sentence summary. Do not restate the name
  (`/// Removes an element.` on `remove_element` adds nothing); say what the
  reader cannot see from the signature.
- Use the standard sections where they apply: `# Examples`, `# Errors`,
  `# Panics`, `# Safety`.
- Link items with intra-doc links (`[`Receiver::poll_recv`]`).
- Add examples for the main entry points of a crate; not every function needs
  one.
- Individual enum variants may go undocumented when the name says it all.

### 11.3 Implementation comments

- Comment the key logic, not every line: why a design was chosen, which
  invariant holds, which case a branch covers, what a magic value means.
- Do not narrate the code or reference the history of a change
  (`// changed to fix the bug`); that belongs in the commit message.
- `TODO` comments say what is missing and, when there is one, link an issue:
  `// TODO(#123): support resumable download`.
- Mix `///` and `//` freely: public contract in `///`, implementation notes in
  `//`.

### 11.4 Design documents

When a change alters the design of a crate or subsystem, update its `README.md`,
`DESIGN.md` or `docs/*.md` in the same change.

## 12. Implementation details

### 12.1 Type inference

Add explicit types where a reader cannot infer them: results of `.into()`,
`.parse()`, `.collect()` and long iterator chains. Prefer annotating the binding
(`let ids: Vec<RunnerId> = ...`) over a turbofish.

### 12.2 Iterator chains

- Use combinators for short chains and when the combinator carries the logic
  (`any`, `position`, `sum`).
- Use a `for` loop when the body has side effects, branches or early exits.
- Do not build a chain in a `for` loop header; bind it to a named variable.
- Avoid `for_each`; a `for` loop reads better.

### 12.3 Assertions

Give `assert!`, `debug_assert!` and `expect` a message that explains why the
condition holds or what was violated, not one that repeats the condition:

```rust
debug_assert_eq!(state, REGISTERING | WAKING, "a wake arrived while registering");
```

Use `debug_assert!` for invariants on hot paths and `assert!` for preconditions
of public APIs.

### 12.4 Strings

Use `format!`/`write!` with inline arguments (`"{source}"`) for concatenation
and substitution, instead of `+` chains or `str::replace` on a template.

### 12.5 Macros

Prefer functions, generics and traits. Use `macro_rules!` only for small,
repetitive code that functions cannot express (such as the telemetry facade). Do
not add proc macros of our own; use established ones (`snafu`, `derive_more`,
`dynosaur`).

### 12.6 Operator overloading

Overload operators only for their mathematical meaning.

### 12.7 Code before `main`

Do not run code before `main` (no `ctor`-style initializers); initialize lazily
with `LazyLock` or `OnceLock`.

## 13. Tests and benchmarks

### 13.1 Async tests

Use `#[tokio::test]` for async tests and add `#[n0_tracing_test::traced_test]`
when logs help diagnose failures. A runtime implementation is tested on its own
runtime (smol tests run under smol).

### 13.2 Fixtures

Use the adapters and fixtures in `bolt-load-tests` (simple, fault-injection and
HTTP server adapters) instead of writing ad-hoc mocks. Extend them when a test
needs a new behavior.

### 13.3 Assertions in tests

- Use `pretty_assertions::assert_eq` for structured values.
- Tests must be deterministic: wait on events or bounded timeouts, not on fixed
  `sleep`s that race with the code under test.
- Verify downloaded content by hash (`blake3`), not only by length.

### 13.4 Model checking

Every protocol in `bolt-load-sync` has a loom model in
`crates/bolt-load-sync/tests/loom.rs`. Change the protocol, change the model:

```bash
RUSTFLAGS="--cfg loom" CARGO_TARGET_DIR=target/loom \
  cargo test -p bolt-load-sync --release --test loom
```

### 13.5 Characterization first

Before refactoring a behavior that has no tests, add tests that pin the current
behavior (commits like `test(runner): characterise ...`), then refactor.

### 13.6 Benchmarks

Benchmarks use criterion in `crates/bolt-load/benches`. A `perf` change reports
before and after numbers from the relevant bench.

## 14. Commits and breaking changes

- Commit messages follow [Conventional Commits][conventional-commits], as
  checked by `commitlint.config.ts`: a lowercase type from `build`, `chore`,
  `ci`, `docs`, `feat`, `fix`, `perf`, `refactor`, `revert`, `style`, `test`; an
  optional kebab-case scope (usually the crate or subsystem: `runner`, `sync`,
  `concurrent`); a lowercase subject without a trailing period; a header of at
  most 100 characters.
- One logical change per commit.
- **Breaking changes go in directly.** Do not add a `#[deprecated]` step first.
  Mark the commit with `!` after the type or scope and add a `BREAKING CHANGE:`
  footer that says how to migrate:

  ```text
  feat(concurrent)!: send runner limits through a latest-value mailbox (#140)

  <why the change was made and what it does>

  BREAKING CHANGE: `TaskRunnerBuilder` fields are private; use its setters.
  ```

[google]: https://google.github.io/styleguide/rustguide.html
[rust-style]: https://doc.rust-lang.org/nightly/style-guide/
[api-guidelines]: https://rust-lang.github.io/api-guidelines/
[api-naming]: https://rust-lang.github.io/api-guidelines/naming.html
[conventional-commits]: https://www.conventionalcommits.org/
