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
  one-shot [`TimerBuilder::sleep`]s for a duration. The engine's own
  one-shot waits, the metadata retry's among them, are deadlines, which a
  custom runtime serves with [`TimerBuilder::sleep_until`], see below.
  The default `sleep` waits for the second tick of a delayed timer;
  custom runtimes should override it with their own sleep.
  [`TimerBuilder::now`] is the clock those timers run on, see below, and
  [`TimerBuilder::sleep_until`] sleeps until an instant of it. The tokio
  and smol runtimes register the instant itself. The default sleeps for the
  time left as it reads the clock, so it ends late if the clock moves
  before the sleep starts: a deadline on it still fires once polled at its
  target, but its wake-up may come only at that late end. A runtime whose
  clock can move meanwhile, such as a virtual one, should override it for
  exact wake-ups.

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

The engine waits at most 30 years at once: every wait it arms, a
timeout, a backoff or a metadata retry, generated with jitter or not, is
capped there. For the whole download, the clock must stay at least 30
years plus the longest configured duration (timeouts, metadata retry
delays) before the end of what an `Instant` holds, a duration over 30
years, such as `Duration::MAX`, counting as 30 years: so never more than
60 years. A tokio runtime's clock must likewise
stay that far before the end of its timer's range, the milliseconds a
`u64` counts from the runtime's start, so that every target fits in it
too, as an unadvanced or normally advanced runtime does. The standard
clock and smol's and tokio's, paused or not, meet this by a wide margin.
Within it, no wait the engine arms for a duration configured or sent by
a server overflows an instant or fires before its capped deadline (a
configured 31-year timeout fires at 30 years), and a capped wait always
fits. Outside it there is no guarantee: a deadline may land nearer, at the furthest instant halving
the capped duration finds, and the engine's arithmetic, or tokio's own
timers, may panic.

The tokio runtimes create their periodic timers and sleeps on their own
handle, and read tokio's clock through it, whatever runtime the caller is
in, or none; a periodic timer is also polled in its runtime, so a tick
taken late schedules the next one on that runtime's clock. A task's timers
are so registered with the runtime it was given, which is where
`tokio-console` shows them, and pausing or advancing that runtime's clock
(tokio's `test-util`) moves the engine's time. The smol runtimes use the
standard clock.

The engine's periodic timers, the throughput and strategy ticks of the
concurrent loop and the sampling of the singleton, come from
[`TimerBuilder::create_delayed_timer`]. Its one-shot timers, the loop's
deadline, a runner's connect, idle and drain timeouts and the waits of the
metadata retry, are deadlines at
an instant of that clock on the same runtime: a tokio sleep on its handle, a
smol timer, or a custom runtime's [`TimerBuilder::sleep_until`] the target,
started at the deadline's first poll. A deadline already reached fires
without a sleep, and a custom one reads the clock at every poll: a sleep
that ends late does not hold it back once the deadline is polled, though
on the default `sleep_until` its wake-up may come only at that late end.
Every such wait is capped at 30 years, tokio's far future, which fits
under the clock contract above.

A task's tokio runtime must therefore have its timer, `enable_time` or
`enable_all` on its builder, and be driven for as long as the task runs, in
either mode: a multi-thread runtime drives itself, a current-thread one only
inside its own `block_on`, and its timers do not fire while nothing drives
it. A task on a runtime without its timer fails with a runtime error.

`async-io`'s timers on the standard clock remain in three places only: the
metadata retry before a task has a runtime, a runner built without a timer
source (see the runner docs), and [`timeout()`].

# Feature Gates

| Feature | Runtime |
|---------|---------|
| `tokio` | [`TokioThreadedRuntime`] / [`LocalTokioRuntime`] |
| `smol`  | [`SmolThreadedRuntime`] / [`SmolLocalRuntime`]   |

# Utilities

- [`yield_now()`] — Cooperative task yielding (returns [`YieldNow`] future).
- [`timeout()`] — Runtime-agnostic timeout wrapper using `futures-concurrency`
  and an `async-io` timer.
