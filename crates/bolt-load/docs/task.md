High-level download task orchestration.

A [`Task`] is the primary user-facing type — it coordinates metadata
retrieval, file allocation, runner spawning, progress tracking, and
state transitions for a single download.

# Download Modes

| Mode | Implementation | Description |
|------|---------------|-------------|
| [`Singleton`](DownloadMode::Singleton) | [`SingletonTask`] | Single runner, sequential download |
| [`Concurrent`](DownloadMode::Concurrent) | [`ConcurrentTask`] | Multiple runners, parallel range-based download |

Mode selection is automatic when the adapter reports range stream
support, or can be forced via [`TaskBuilder`].

# Task Lifecycle

```text
Idle ──► Spawning ──► Initializing ──► Downloading ──► Finished
                                            │
                                            ▼
                                          Failed
```

[`TaskState`] is stored as an atomic enum for lock-free reads from
any thread.

# Termination

Every task ends in `Finished` or `Failed`; it never retries forever.

- **Progress** means new, non-empty bytes accepted for writing. Opening
  a connection or receiving empty frames is not progress.
- **No progress**: when no progress is made for
  [`TaskOptions::no_progress_timeout`] the task fails with
  `TaskError::NoProgress`. The deadline keeps running while no runner is
  active and while the writer applies backpressure.
- **Consecutive failures** (concurrent mode): a failed attempt is a
  failed connect or a runner stopping with an error. After
  [`TaskOptions::max_consecutive_failures`] retryable failures without a
  successful non-empty write in between, the task fails with
  `TaskError::RetryLimitExceeded`, which carries the last error.
- **Unretryable failures** end the task with the adapter's original
  error. The one exception is `ServiceUnavailable` (429/503) while other
  attempts are still in flight: it is a concurrency signal for the
  concurrency strategy, still bounded by the no-progress deadline.
- **Idle streams**: a runner whose upstream yields no non-empty data
  for [`TaskOptions::stream_idle_timeout`] fails with the retryable
  `TaskError::Timeout`. In singleton mode there is no retry, so this
  ends the task; there `no_progress_timeout` bounds establishing the
  connection.

| Option | Default |
|--------|---------|
| `no_progress_timeout` | 60 s |
| `max_consecutive_failures` | `max(8, 2 × CPU count)` |
| `stream_idle_timeout` | 30 s |

Keep `no_progress_timeout` larger than `stream_idle_timeout`; otherwise a
stalled stream ends the task with `NoProgress` before its runner can time
out and be retried.

# Cancellation and Cleanup

The cancel token passed to [`TaskBuilder`] is owned by the caller: the
task only ever cancels child tokens, so finishing or failing never
cancels it. Cancelling it aborts [`TaskBuilder::build`] (metadata and
range probe requests) with `TaskManagerBuildError::Cancelled`, and a
running task with `TaskError::Cancelled`.

Success, failure and cancellation share one teardown path: cancel the
runners, wait for connectors, runners, progress senders, write tasks and
writer workers to exit, then publish or discard the temporary file.
`TaskEvent::Failed` is emitted only after this teardown.

- [`Task::stop`] is a join barrier: it cancels and returns once all
  work owned by bolt-load has exited. Work an adapter detaches on its
  own, or a write blocked forever by the OS, cannot be bounded.
- [`Task::wait`] joins the same way and returns the terminal error. Both
  are safe to call repeatedly, before [`Task::run`], or after a dropped
  `wait` future.

# Temporary File and Commit Rule

Both modes download into `<save path>.downloading` and rename it to the
save path only after a successful transfer. A cancellation observed
before the rename starts discards the result; once the rename starts,
its outcome decides. On failure the temporary file is removed and an
existing file at the save path is never touched.

# Task Lifecycle

Both [`SingletonTask`] and [`ConcurrentTask`] run the same sequential
lifecycle on a local runtime, reporting each step as a [`TaskEvent`]:

1. **Initializing** — calls [`retrieve_meta()`] to get the content size.
   Cancellation during this step fails the task with `Cancelled`, and
   wins when both are ready.
2. **Downloading** — publishes a snapshot with zero speed, then spawns
   runner(s) and publishes sampled progress. Periodic samples are
   dropped while the event channel is full; they never stall the transfer.
3. **Finished** / **Failed** — sent last, after every runner and writer
   has been joined and the output file has been committed or removed.

# Concurrent Task Components

The concurrent download path includes several sub-systems:

- **RunnerManager** — tracks multiple [`TaskRunner`] instances,
  their assigned byte ranges, and completion status.
- **FileWriter** — random-access writer with multiple backends:
  `Mmap`, `Pool` (pwrite), `Compio` (io\_uring), `Null` (benchmark).
  Wrapped in [`PendingWriter`] for backpressure queuing.
- **StrategyControl** — dynamic optimization:
  - [`ConcurrencyControlStrategy`] — adjusts runner count based on load.
  - [`DynamicStrategy`] — splits slow runners' remaining ranges.
- **SpeedSampler** — EMA-based speed calculation (α = 0.33,
  interval = 250 ms).
- **ID Generator** — [`BitVec`]-backed runner ID allocator with
  O(1) amortized allocation.

# Progress Tracking

- [`Progress`] — total size, downloaded bytes, and downloaded chunk
  ranges (for resumable downloads).
- [`ProgressWithSpeed`] — extends [`Progress`] with download and
  write speed metrics (bytes/sec).
- [`TaskEvent`] — enum of lifecycle events:
  `Initializing`, `Downloading(ProgressWithSpeed)`,
  `Failed(TaskInstanceError)`, `Finished(Progress)`.

# Builder

[`TaskBuilder`] validates configuration and constructs a [`Task`]:
- Requires an adapter, save path and cancel token.
- Accepts [`TaskOptions`] via [`TaskBuilder::options`].
- Auto-selects download mode based on range stream availability.
- Metadata retrieval and the range probe are cancellable.
- Validates parent directory exists and target is not a directory.
- Optionally attaches a progress bar (feature `progressbar`).

# Key Constants

| Name | Value | Purpose |
|------|-------|---------|
| `DEFAULT_MAX_CONCURRENCY` | CPU core count | Initial concurrent runners |
| `DEFAULT_SAMPLE_INTERVAL` | 250 ms | Speed measurement period |
| `DEFAULT_STRATEGY_TICK_INTERVAL` | 1.5 s | Strategy evaluation period |
| `FILE_WRITER_QUEUE_SIZE` | 2048 | Pending write queue depth |
