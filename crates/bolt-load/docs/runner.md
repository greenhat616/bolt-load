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

- **Lifecycle channel** (capacity 2) — reports [`Started`] and
  [`Stopped`] events to the runner manager.
- **Data channel** (capacity 32) — emits [`DataFrame`] chunks for
  the file writer to consume.

# Control Flow

Runners accept [`ControlSignal`] messages for dynamic adjustments:

- `LimitTotal(u64)` — cap the maximum bytes this runner should download.

# Timeout & Error Handling

- **Slow stream detection**: if no data arrives for 5 seconds, emits
  a warning and eventually yields a [`TaskError::Timeout`].
- **Data drain timeout**: when the stream ends, waits up to 5 seconds
  for the consumer to accept remaining buffered data.
- **Size validation**: checks that downloaded bytes match the declared
  total (if known), producing [`ExceededTotalSize`] or
  [`SmallerThanTotalSize`] errors on mismatch.

# Key Types

- [`TaskRunner`] — the stream consumer itself.
- [`TaskRunnerBuilder`] — fluent builder with validation.
- [`TaskRunnerGuard`] — RAII handle held by the runner manager;
  auto-cancels the runner on drop.
- [`StreamConnector`] — lazy or pre-connected stream source.
- [`TaskError`] — comprehensive error enum with retryability classification.

# Constants

| Name | Value | Purpose |
|------|-------|---------|
| `BUFFER_SIZE` | 32 KB | Internal read buffer |
| `SLOW_STREAM_TIMEOUT` | 5 s | No-data warning threshold |
| `DATA_DRAIN_TIMEOUT` | 5 s | Backpressure wait on stream end |
| `LIFECYCLE_CHANNEL_CAPACITY` | 2 | Lifecycle ring-buffer slots |
| `DATA_FRAME_CHANNEL_CAPACITY` | 32 | Data frame ring-buffer slots |
