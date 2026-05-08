Async runtime abstraction layer supporting multiple executors.

This module provides a unified interface over different async runtimes
(Tokio, Smol, or custom implementations), allowing the download engine
to remain runtime-agnostic.

# Runtime Traits

The module defines a trait hierarchy for async execution:

- [`ThreadedRuntime`] — Multi-threaded runtime with [`Spawn`] + [`TimerBuilder`]
  capabilities. Use this to spawn tasks that can run on any thread.
- [`LocalRuntime`] — Single-threaded runtime with [`LocalSpawn`] support.
  Used internally by state machines that require `!Send` futures.
- [`DowncastLocalRuntime`] — Bridges threaded and local runtimes by
  downcasting a [`ThreadedRuntime`] into a [`LocalRuntime`].
- [`Timer`] / [`ObjectSafeTimer`] — Periodic tick interface for speed
  sampling and strategy evaluation.
- [`TimerBuilder`] — Factory for creating [`Timer`] instances with a
  configurable interval.

# Enum Dispatch

Concrete runtime implementations are wrapped in enum-dispatched types
for zero-cost polymorphism:

- [`ThreadedRuntimeImpl`] — Dispatches to `Tokio`, `Smol`, or a custom
  `Arc<dyn ThreadedRuntime>`.
- [`LocalRuntimeImpl`] — Dispatches to `Tokio`, `Smol`, or a custom
  `Rc<dyn LocalRuntime>`.

# Feature Gates

| Feature | Runtime |
|---------|---------|
| `tokio` | [`TokioThreadedRuntime`] / [`LocalTokioRuntime`] |
| `smol`  | [`SmolThreadedRuntime`] / [`SmolLocalRuntime`]   |

# Utilities

- [`yield_now()`] — Cooperative task yielding (returns [`YieldNow`] future).
- [`timeout()`] — Runtime-agnostic timeout wrapper using `futures-concurrency`.
