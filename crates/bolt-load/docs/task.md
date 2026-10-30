High-level download task orchestration.

A [`TaskBuilder`] configures a download synchronously and without network
I/O; [`TaskBuilder::spawn`] starts it and returns a [`Task`], an awaitable,
cancel-on-drop handle whose output is a [`TaskOutput`] or a
[`DownloadError`]. [`Task::status`] is a latest-value
[`watch`](bolt_load_sync::watch) receiver of [`TaskStatus`] snapshots.

```rust,ignore
let task = Task::builder(adapter)
    .runtime(rt)
    .save_to("large-file.bin")
    .spawn()?;

let mut status = task.status();
while status.changed().await.is_ok() {
    let s = status.borrow_and_update();
    println!("{:?} {}/{:?}", s.state, s.progress.downloaded, s.progress.total);
}
let output = task.await?;
```

The full contract, with numbered requirements, is in
`docs/task-api-spec.md`.

# Download Modes

| Mode | Description |
|------|-------------|
| [`Singleton`](DownloadMode::Singleton) | Single runner, sequential download |
| [`Concurrent`](DownloadMode::Concurrent) | Multiple runners, parallel range-based download |

The mode is selected while the task is `Initializing`: `Singleton` when
it is preferred via [`TaskBuilder::prefer_mode`] or the size is unknown
(zero), otherwise `Concurrent` when the adapter reports range stream
support. [`TaskInfo::mode`] reports the choice.

# Task Lifecycle

```text
Pending ──► Initializing ──► Downloading ──► Finished
                 │                │
                 └────────────────┴──► Failed | Cancelled
```

A terminal [`TaskState`] never changes. Status is level-triggered: a
receiver is guaranteed the newest value only, so intermediate states and
samples may be coalesced. `progress` is kept in every state, and the
channel closes right after the terminal status.

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
  ends the task.
- **Slow connects**: a runner that is not connected
  [`TaskOptions::connect_timeout`] after it started connecting fails with
  the retryable `TaskError::Timeout`, however much other runners progress
  meanwhile. A concurrent task retries its range; runners still connecting
  are never split. A singleton task fails, and creates its temporary file
  only once connected; a failed connection keeps the adapter's error.
- **Metadata request**: [`TaskBuilder::retrieve_meta`] and the task's
  initialization retry retryable metadata errors as
  [`TaskOptions::meta_retry`] allows, with jittered exponential backoff
  on the runtime's timer, then report the last error. Unretryable errors
  are reported at once. Metadata retrieved by the builder, or injected
  with [`TaskBuilder::meta`], is not requested again.

| Option | Default |
|--------|---------|
| `no_progress_timeout` | 60 s |
| `max_consecutive_failures` | `max(8, 2 × CPU count)` |
| `stream_idle_timeout` | 30 s |
| `connect_timeout` | 30 s |
| `meta_retry` | 3 retries, 200 ms – 2 s per wait |

Keep `no_progress_timeout` larger than `stream_idle_timeout`; otherwise a
stalled stream ends the task with `NoProgress` before its runner can time
out and be retried.

# Cancellation and Cleanup

The optional cancel token passed to [`TaskBuilder::cancel_token`] is
owned by the caller: each task runs on a child token of it, so finishing,
failing or dropping a task never cancels it. Cancelling it, or the task's
own token ([`Task::cancel`], [`Task::cancel_token`]), ends the task in
`Cancelled`, during initialization (metadata and range probe requests,
retry backoff) as well as while downloading.

Success, failure and cancellation share one teardown path: cancel the
runners, wait for connectors, runners, progress publishers, write tasks
and writer workers to exit, then commit or discard the temporary file.
The terminal status is published only after this teardown; when the
temporary file cannot be removed, [`TaskStatus::leftover_temp_file`]
names it.

- Awaiting the [`Task`] joins: it returns once all work owned by
  bolt-load has exited. Work an adapter detaches on its own, or a write
  blocked forever by the OS, cannot be bounded. `(&mut task).await` may
  be repeated, and a dropped await does not cancel the task.
- [`Task::stop`] cancels, then joins the same way.
- Dropping the [`Task`] cancels the download and lets its teardown run in
  the background; [`Task::detach`] keeps it running instead.
- A panic in the attempt ends the task in `Failed` with
  [`DownloadError::Panicked`]; its workers are signalled but not joined.

# Temporary File and Commit Rule

Both modes download into `<save path>.downloading` and rename it to the
save path only after a successful transfer. With
[`TaskBuilder::save_into`], the save path is the directory joined with the
adapter's suggested filename, reduced to its final path segment; a name
that is not a safe file name fails the task with
[`DownloadError::InvalidFilename`]. A cancellation observed
before the rename starts discards the result; once the rename starts,
its outcome decides. On failure the temporary file is removed and an
existing file at the save path is never touched.

# Attempt Steps

Each spawned task runs one attempt on a local runtime, on a `blocking`
pool thread it occupies until the end (rust-lang/rust#100013):

1. **Initializing** — use the cached metadata or retrieve it, select the
   mode, resolve and check the destination, then publish [`TaskInfo`].
   Cancellation during this step ends the task in `Cancelled`, and wins
   when both are ready.
2. **Downloading** — publishes a snapshot with zero speed, then spawns
   runner(s) and publishes sampled progress. Publishing never waits for a
   receiver, so a slow reader cannot stall the transfer.
3. **Finished** / **Failed** / **Cancelled** — published last, after
   every runner and writer has been joined and the output file has been
   committed or removed.

A task is one attempt. To retry, spawn again from a clone of the builder.

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
- [`TaskStatus`] — the state, [`Progress`], [`Speed`] (download and
  write, bytes/sec), [`TaskInfo`] once initialized, and the error.
- `Task::drive_progress_bar` — drives an `indicatif` bar from the status
  on its own thread (feature `progressbar`).

# Builder

[`TaskBuilder`] validates configuration and spawns a [`Task`]:
- Created by [`Task::builder`] with the adapter; requires a runtime and a
  destination ([`TaskBuilder::save_to`] or [`TaskBuilder::save_into`]).
- Accepts [`TaskOptions`] via [`TaskBuilder::options`].
- [`TaskBuilder::validate`] and [`TaskBuilder::spawn`] check the options
  and, best effort, that the destination's directory exists and the
  target is not a directory, returning a [`BuildError`]. Initialization
  repeats the checks.
- `Clone`, so a queue can keep validated builders and spawn them later.

# Key Constants

| Name | Value | Purpose |
|------|-------|---------|
| `DEFAULT_MAX_CONCURRENCY` | CPU core count | Initial concurrent runners |
| `DEFAULT_SAMPLE_INTERVAL` | 250 ms | Speed measurement period |
| `DEFAULT_STRATEGY_TICK_INTERVAL` | 1.5 s | Strategy evaluation period |
| `FILE_WRITER_QUEUE_SIZE` | 2048 | Pending write queue depth |
