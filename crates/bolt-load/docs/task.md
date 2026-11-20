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
it is preferred via [`TaskBuilder::prefer_mode`], the metadata shows a
content coding, or the size is unknown (zero), otherwise `Concurrent`
when the adapter reports range stream support. A preferred `Singleton`
gives way in one case: a task with [`Resume::On`] that can be resumed
runs `Concurrent` with one connection, so that it keeps a state file.
[`TaskInfo::mode`] reports the choice.

When ranges turn out unusable during a concurrent download (the server
ignores them or content-codes them), the task discards what it wrote and
starts over on one connection, with the size unknown. It does so at most
once. The status turns `Downloading` again with progress from zero, and
[`TaskInfo::fallback`] says why; a content coding seen in the metadata
sets it too.

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
- **Failures of one range** (concurrent mode): the other connections'
  writes do not reset them. Once a range has failed
  [`TaskOptions::max_attempts_per_range`] times, the task fails with
  `TaskError::RangeExhausted`, which names the range and carries its last
  error. A `Congestion` failure is not counted, nor is a connect refused
  above what the server was seen to admit: it failed before `Started`,
  another attempt had delivered bytes, and at least as many other
  attempts were in flight as had delivered bytes at once since the
  connection limit was last lowered. While refusals stay exempt, nothing
  bounds them per range: the task then ends when the other connections
  have the rest, or by the no-progress deadline and the consecutive
  failures. The count goes with the range like its backoff, see below. When a failure reaches both limits,
  the task fails with `RangeExhausted`.
- **Restarts** (concurrent mode): a timeout or a dropped stream after the
  attempt delivered bytes counts neither as a consecutive failure nor
  for its range. The range keeps the delivered bytes and only backs off,
  so every restart makes progress. A timeout before any byte, a failed
  connect and any other retryable failure count, a lost runner even
  after it delivered bytes.
- **Unretryable failures** end the task with the adapter's original
  error. The one exception is `Congestion` (429/503) while other
  attempts are still in flight: it is a concurrency signal for the
  concurrency strategy, still bounded by the no-progress deadline.
- **Backoff** (concurrent mode): the range of a failed attempt is not
  retried before a backoff of 0.5 s for its first failure, doubling up
  to 30 s, plus jitter. A range counts on from its failures when it is
  retried; a range split off a running one starts at none. A
  `Congestion` failure waits for its `Retry-After` when that is longer,
  at most 120 s, and holds back every new connection of the task until
  then, splits included. The end of a backoff wakes the task, which then
  creates runners only as the concurrency strategy would: into free
  slots, and after an unretryable failure only once the strategy has
  lowered the concurrency. A range it cannot retry then waits for the
  next strategy tick. A `Retry-After` longer than
  `no_progress_timeout` ends the task with `NoProgress` unless other
  runners keep making progress.
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
- **Write failures** end the task at once and are never retried. The
  concurrent mode reports `TaskError::Write` with the range and the io
  error. The singleton mode writes each frame through before it takes
  the next: it reports `DownloadError::Io` with `IoOp::Write`, without
  a range. Both keep the
  `io::ErrorKind`, and a full disk or an exceeded quota adds a hint to
  the message.
- **Other file failures** end the task the same way. Both modes report
  creating and sizing the temporary file as `DownloadError::Io` with
  `IoOp::Create` or `IoOp::Allocate`. The concurrent mode reports its
  final sync, after the writer workers exit, with `IoOp::Flush`.
- **Metadata request**: [`TaskBuilder::retrieve_meta`] and the task's
  initialization retry retryable metadata errors as
  [`TaskOptions::meta_retry`] allows, with jittered exponential backoff
  on the runtime's timer, then report the last error. Unretryable errors
  are reported at once. Metadata retrieved by the builder, or injected
  with [`TaskBuilder::meta`], is not requested again.

| Option | Default |
|--------|---------|
| `max_connections` | 8, whatever the CPU count |
| `no_progress_timeout` | 60 s |
| `max_consecutive_failures` | `max(8, 2 × max_connections)` |
| `max_attempts_per_range` | 5 |
| `stream_idle_timeout` | 30 s |
| `connect_timeout` | 30 s |
| `meta_retry` | 3 retries, 200 ms – 2 s per wait |
| `checkpoint_bytes` | 8 MiB |
| `checkpoint_interval` | 2 s |

`checkpoint_bytes` and `checkpoint_interval` schedule the checkpoints of a
task that keeps a state file for a later resume: its complete blocks,
with their CRCs, recorded once their data is synced. A periodic
checkpoint never starts sooner than 0.5 s after the last one ended, so a
fast source does not sync the file every few milliseconds; the checkpoint
at teardown does not wait for that. They have no effect on a task that
keeps no state file (see Resuming).

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
  [`DownloadError::Panicked`], after its workers that hold the temporary
  file have exited, the temporary file was removed and the destination's
  lock released. A panic after the rename leaves the task `Finished`.

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

# One Task per Destination

A task holds an exclusive lock on `<save path>.boltload` from
initialization to its end, so two tasks, in one process or several,
never write the same temporary file. A second task on a destination in
use fails at once with [`DownloadError::InUse`], before it writes
anything; try again once the first has ended. The lock goes with the
process, so a crashed task does not keep it.

- The lock file sits next to the destination while the task runs, and is
  removed when it ends. A task that finds one left by a crash takes it
  over, and empties it before it touches the temporary file.
- The destination's directory must be writable for the lock file. On a
  file system that cannot lock files the task warns and goes on unlocked,
  leaving the lock file behind, empty;
  a missing permission fails it with [`DownloadError::Io`] and
  [`IoOp::Lock`].

# Resuming

With [`TaskBuilder::resume`]`(`[`Resume::On`]`)`, a task that does not finish
keeps what it wrote: the temporary file and a state file,
`<save path>.boltload`, which records every block of 1 MiB whose data was
written and synced, with its CRC32C. [`TaskStatus::kept_for_resume`]
names the temporary file then. The next task with `Resume::On` for the
same destination continues from them:

1. It reads the state file and compares it with the metadata the adapter
   retrieves: the size, the validator ([`Validator`](crate::adapter::Validator)),
   and the source given with [`TaskBuilder::resume_source`].
2. It reads every recorded block back and checks its CRC. A block that
   does not match, lies past the end of the file, or whose record is
   torn, bad or contradicted, is dropped and downloaded again;
   [`TaskInfo::resumed`] reports what was kept and what was dropped, and
   why.
3. It downloads the rest only, then renames the file as usual. Its first
   `Downloading` status reports the kept bytes as progress.

A task can resume when the size is known, the response is not
content-coded, the server takes ranges and sends a usable validator (a
strong `ETag`, or a `Last-Modified` old enough); [`TaskInfo::resumable`]
says whether it does. Otherwise it downloads as with `Resume::Off`. A
task that prefers one connection resumes over ranges with one connection.

A task never starts over on its own. When the files cannot be continued,
because the resource changed, the state file is bad, or the temporary
file is gone, it fails with [`DownloadError::ResumeRejected`] and
changes neither file. Starting over is the caller's choice:

```rust,ignore
use bolt_load::task::{DownloadError, Resume, Task, discard_partial};

let task = Task::builder(adapter()).runtime(rt.clone()).save_to("big.iso").resume(Resume::On);
match task.spawn()?.await {
    Err(DownloadError::ResumeRejected { reason, .. }) => {
        log::warn!("cannot resume: {reason}; starting over");
        discard_partial("big.iso").await?;
        Task::builder(adapter()).runtime(rt).save_to("big.iso").resume(Resume::On).spawn()?.await?;
    }
    other => {
        other?;
    }
}
```

- Resume with a new adapter. An adapter caches the metadata it
  retrieved, so on the same adapter the state file is compared with that,
  and a change since is caught only by the `If-Range` of the range
  requests the resume makes, as [`DownloadError::ResourceChanged`]; a
  resume that kept every block makes none. A new adapter checks against
  the resource as it is now.
- [`discard_partial`] removes what a task kept for a destination; it fails
  with [`DownloadError::InUse`] while a task holds it.
- A resume needs the destination's lock: on a file system that cannot
  lock files, a task with `Resume::On` fails with [`DownloadError::Io`]
  and [`IoOp::Lock`].
- The checks find what a crash, a power loss or a bad sector leaves; the
  state file is not protected against deliberate changes. On Windows the
  directory is not synced; NTFS journals the metadata.
- With the ureq2 adapter, a blocking request or read still running when
  its range is given up keeps its connection open until the call returns.
  Until then a task can hold more connections than
  [`TaskOptions::max_connections`], and repeated retries or restarts can
  hold several such connections; without timeouts on the agent, a stalled
  server can keep them open indefinitely
  ([#207](https://github.com/greenhat616/bolt-load/issues/207)). Give the
  agent timeouts, as the bolt-load-adapter README describes.

# Attempt Steps

Each spawned task runs one attempt on a local runtime, on a `blocking`
pool thread it occupies until the end (rust-lang/rust#100013):

1. **Initializing** — use the cached metadata or retrieve it, select the
   mode, resolve and check the destination, take the destination's lock,
   then publish [`TaskInfo`].
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
  - `DoublingStrategy` — adds the runners. At the first strategy tick
    at least 2 s after its last growth, while no runner connects, every
    running runner had bytes accepted in the tick and no 429 or 503
    holds the task back, it doubles the runners up to the current limit.
    A running runner silent for 2 s, and running at least that long,
    does not count against it: silent means it read nothing of its body
    and is not pushing a frame. None is while the writer is full, nor is
    a runner with frames the loop has not taken yet. Each new
    runner splits the running range with the most left, if that range
    holds at least 2 s of work at its runner's own rate; a split reserves
    the new runner's slot first, so a task never runs more than
    `max_connections` runners. A doubling that
    gains less than 10% in speed stops it for the rest of the task. It
    only adds runners; lowering the limit stays with the concurrency
    strategy. At most one runner a strategy tick is restarted: one that
    delivered under a quarter of the median of at least three runners
    over the last 10 s, allowing one 32 KiB buffer either way, with at
    least twice the minimum chunk size left, outside a 429 or 503 hold;
    or else, without three runners or a full window, the one silent the
    longest, for at least 4.5 s, and running at least that long, with at
    least twice the minimum chunk size left, outside a hedged pair, with
    no frames the loop has not taken yet, while the writer can write
    and outside a 429 or 503 hold.
    It is stopped alone, unless it is already ending with a result of
    its own, which it then reports, and its
    range retried from the accepted bytes after a backoff; the range's
    failure budget is charged only when it had none accepted. In the
    endgame, when no range is left to claim or split, the runner silent
    longest, for at least 2 s, with less than twice the minimum chunk
    size left, is hedged: a second runner requests the rest of its range,
    the first non-empty frame decides which goes on, and the other is
    stopped and ignored. The two count as one attempt on the range's
    budget.
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
- Accepts [`TaskOptions`] via [`TaskBuilder::options`], and the
  connection limit alone via [`TaskBuilder::max_connections`].
- [`TaskBuilder::validate`] and [`TaskBuilder::spawn`] check the options
  and, best effort, that the destination's directory exists and the
  target is not a directory, returning a [`BuildError`]. Initialization
  repeats the checks.
- `Clone`, so a queue can keep validated builders and spawn them later.

# Key Constants

| Name | Value | Purpose |
|------|-------|---------|
| `DEFAULT_MAX_CONNECTIONS` | 8 | Default of `TaskOptions::max_connections` |
| `DEFAULT_SAMPLE_INTERVAL` | 250 ms | Speed measurement period |
| `DEFAULT_STRATEGY_TICK_INTERVAL` | 1.5 s | Strategy evaluation period |
| `MIN_RANGE_BACKOFF` | 500 ms | Backoff of a range after its first failure |
| `MAX_RANGE_BACKOFF` | 30 s | Longest backoff of a range, jitter included |
| `MAX_CONGESTION_BACKOFF` | 120 s | Longest wait for a `Retry-After` |
| `FILE_WRITER_QUEUE_SIZE` | 2048 | Pending write queue depth |
