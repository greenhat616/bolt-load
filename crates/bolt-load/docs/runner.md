Low-level stream consumer that buffers and forwards downloaded data.

A [`TaskRunner`] is the smallest download unit — it owns a single byte
stream, reads chunks into an internal buffer, and emits [`DataFrame`]
messages to a downstream consumer (file writer or aggregator).

# Architecture

```text
AnyBytesStream ──► TaskRunner ──► DataFrameSender ──► FileWriter
                       │
                       ▼
                 LifecycleSender ──► RunnerManager
```

Each runner operates on a dedicated async task and communicates through
two ring-buffer channels:

- **Lifecycle channel** (capacity 2) — reports [`Started`] (connected)
  and [`Stopped`] events to the runner manager.
- **Data channel** (capacity 32) — emits [`DataFrame`] chunks for
  the file writer to consume.

# Control Flow

Runners accept [`ControlSignal`] messages for dynamic adjustments:

- `LimitTotal(u64)` — cap the maximum bytes this runner should download.

A runner built with [`TaskRunnerBuilder::control_signal`] applies every
signal in order and fails with [`ExceededTotalSize`] on one that does not
lower a known total. The runners of a concurrent task instead read only the
latest limit the runner manager sent, so any number of limits can be sent to
a runner that is still connecting.

A runner takes a bounded number of steps per poll and then yields, waking
itself, so a stream whose items are always ready, empty ones included,
leaves the executor and its timers their turn.

# Timeout & Error Handling

- **Idle stream detection**: if no non-empty data arrives within the
  idle timeout (default 30 s, configured per task via
  [`TaskOptions::stream_idle_timeout`](crate::task::TaskOptions::stream_idle_timeout)
  or per runner via [`TaskRunnerBuilder::idle_timeout`]), buffered bytes are flushed and the
  runner fails with the retryable [`TaskError::Timeout`]. Empty frames do
  not reset the timer; waiting for the consumer (backpressure) does not
  count as idle.
- **Connect timeout**: a runner given a [`TaskRunnerBuilder::connector`]
  that is not connected within the connect timeout (default 30 s, see
  [`TaskRunnerBuilder::connect_timeout`]) fails with the retryable
  [`TaskError::Timeout`] without reporting `Started`. Control signals do not
  extend it.
- **Data drain timeout**: when the stream ends, waits up to 5 seconds
  for the consumer to accept remaining buffered data. A stream that ends
  short of its total waits too, so that the consumer can retry from the
  bytes it took.
- **Size validation**: checks that downloaded bytes match the declared
  total (if known), producing [`ExceededTotalSize`] or
  [`SmallerThanTotalSize`] errors on mismatch. An unknown total accepts
  any length, zero included: a stream that ends without an error finishes,
  even with no bytes.

# Clock and Timers

A runner built with [`TaskRunnerBuilder::timer_source`] reads the time
from that runtime's clock and runs its connect, idle and drain timeouts
on that runtime's timers, as the runners of a task do with the task's
runtime: the times it stamps its read activity with compare with the
loop's on one clock, and its timeouts follow that clock when it is paused
or advanced. On tokio the timeouts are tokio timers of that runtime, which
`tokio-console` shows. The runner keeps a tokio runtime's handle, and
nothing of a smol runtime, so dropping it shuts neither down. Of a custom
runtime it keeps a clone of the `Arc` it is shared in: if that runtime
must not be dropped where the runner is, for one owning the tokio runtime
the runner runs on, another clone of it must outlive the runner.

Without a timer source, a runner reads the standard clock and its timeouts
are `async-io` timers: a runner used on its own needs no runtime of the
crate.

# Key Types

- [`TaskRunner`] — the stream consumer itself.
- [`TaskRunnerBuilder`] — fluent builder with validation.
  A runner is stopped by cancelling the token passed to its builder.
- [`StreamConnector`] — lazy or pre-connected stream source. A runner given
  one through [`TaskRunnerBuilder::connector`] connects it itself: it reports
  [`Started`] once connected, and a failed or cancelled connection reports
  only [`Stopped`].
- [`TaskError`] — comprehensive error enum with retryability classification.

# Constants

| Name | Value | Purpose |
|------|-------|---------|
| `BUFFER_SIZE` | 32 KB | Internal read buffer |
| `DEFAULT_IDLE_TIMEOUT` | 30 s | No-data threshold before `Timeout` |
| `DEFAULT_CONNECT_TIMEOUT` | 30 s | Connect threshold before `Timeout` |
| `DATA_DRAIN_TIMEOUT` | 5 s | Backpressure wait on stream end |
| `LIFECYCLE_CHANNEL_CAPACITY` | 2 | Lifecycle ring-buffer slots |
| `DATA_FRAME_CHANNEL_CAPACITY` | 32 | Data frame ring-buffer slots |
