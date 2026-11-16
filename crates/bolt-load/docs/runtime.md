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
- [`Timer`] — Periodic tick interface for speed sampling and strategy
  evaluation. Implement it with `async fn tick(&mut self)` (the future
  must be `Send`) and wrap a custom timer with [`DynTimer::new_box`] to
  pass it as [`TimerImpl::Custom`].
- [`TimerBuilder`] — Factory for creating [`Timer`] instances with a
  configurable interval, whose first tick completes immediately, and for
  one-shot [`TimerBuilder::sleep`]s such as the metadata retry backoff.
  The default `sleep` waits for the second tick of a delayed timer;
  custom runtimes should override it with their own sleep.
  [`TimerBuilder::now`] is the clock those timers run on, see below.

# Enum Dispatch

Concrete runtime implementations are wrapped in enum-dispatched types
for zero-cost polymorphism:

- [`ThreadedRuntimeImpl`] — Dispatches to `Tokio`, `Smol`, or a custom
  `Arc<dyn ThreadedRuntime>`.
- [`LocalRuntimeImpl`] — Dispatches to `Tokio`, `Smol`, or a custom
  `Rc<dyn LocalRuntime>`.

# Clock and Timers

[`TimerBuilder::now`] reads the clock of a runtime's timers. The engine
reads every instant it compares with a timer from there, so instants and
timers stay in one time domain. The default is the standard clock; a
custom runtime whose timers keep their own time overrides it with them.

The tokio runtimes create their periodic timers and sleeps on their own
handle, and read tokio's clock through it, whatever runtime the caller is
in, or none; a periodic timer is also polled in its runtime, so a tick taken
late schedules the next one on that runtime's clock. A task's timers are so registered with the runtime it was
given, which is where `tokio-console` shows them, and pausing or advancing
that runtime's clock (tokio's `test-util`) moves the engine's time. The smol
runtimes use the standard clock.

# Feature Gates

| Feature | Runtime |
|---------|---------|
| `tokio` | [`TokioThreadedRuntime`] / [`LocalTokioRuntime`] |
| `smol`  | [`SmolThreadedRuntime`] / [`SmolLocalRuntime`]   |

# Utilities

- [`yield_now()`] — Cooperative task yielding (returns [`YieldNow`] future).
- [`timeout()`] — Runtime-agnostic timeout wrapper using `futures-concurrency`.
