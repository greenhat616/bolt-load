# Task API: creation, status and completion

| | |
| --- | --- |
| Status | Implemented; see §12 for deviations |
| Date | 2026-10-30 |
| Scope | `bolt_load::task` public API; `bolt_load_sync::watch` additions |
| Supersedes | `Task::run` / `Task::wait` / `Task::stop`, `TaskBuilder::on_task_state_changed`, `Task::with_progress_bar`, `TaskEvent` as a public type |

The key words MUST, MUST NOT, SHOULD, SHOULD NOT and MAY are used as in
RFC 2119. Requirement ids (`BUILD-1`, `STATUS-3`, …) are stable so tests and
reviews can cite them.

## 1. Summary

A task is configured with a synchronous builder, spawned into a `Task`
handle that is a `Future` of its outcome, and observed through a
latest-value [`watch`](../../bolt-load-sync/src/watch.rs) channel of
`TaskStatus` snapshots. Metadata retrieval, mode selection and destination
resolution move into the task's `Initializing` state, so all I/O is
observable and cancellable in one place. Callbacks and the event-forwarding
task are removed.

```rust
let task = Task::builder(adapter)
    .runtime(rt)
    .save_to("large-file.bin")
    .spawn()?;

let mut status = task.status();
while status.changed().await.is_ok() {
    let s = status.borrow_and_update();
    println!("{:?} {}/{:?}", s.state, s.progress.downloaded, s.progress.total);
}
let output = task.await?; // TaskOutput { path, progress, mode }
```

## 2. Motivation

Verified against `dev` at `aee56c1`:

| # | Problem | Evidence |
|---|---|---|
| P1 | Metadata retrieval is invoked twice per task: by the builder and again by each mode's `prepare`. Adapters without a cache (or with retries) send extra requests; the reqwest adapter caches its context, so it sends one. | `task/builder.rs:247`, `singleton_task.rs:161`, `concurrent_task/mod.rs:176`, `bolt-load-adapter/src/reqwest.rs:193` |
| P2 | `with_progress_bar` calls `send_blocking` inside a callback that runs inline on an async runtime worker. | `task/mod.rs:205`, `:311`, `:323` |
| P3 | Periodic progress uses `try_send` on a bounded(32) channel; when it is full the *newest* sample is dropped, so slow readers see stale progress. | `task/mod.rs:278`, `throughput.rs:74`, `singleton_task.rs:229` |
| P4 | `Initializing`, the first `Downloading` and the terminal event `send().await`; a slow consumer delays start-up and the attempt's completion (not the transfer teardown, which precedes the terminal send). | `drive.rs:36`, `:46`, `:61` |
| P5 | Public state is split across `AtomicTaskState`, `Mutex<Option<TaskInstanceError>>`, a callback `Vec` and a spawned forwarding task. `TaskEvent::Failed` carries no progress. | `task/mod.rs:171-177`, `:287-329`, `instance/mod.rs:101` |
| P6 | The builder requires a cancel token; validation errors are `FieldValidationFailed { message: String }`; path setters take `PathBuf` only. | `builder.rs:239`, `:56`, `:162`, `:168` |
| P7 | Cancellation is nested: `TaskInstanceError::Failed { source: Arc<TaskError::Cancelled> }`. | `drive.rs:42`, `instance/mod.rs:148` |
| P8 | `run` is `async` without awaiting; completion needs a separate `wait`; nothing composes with `join_all` / `select!`. | `task/mod.rs:258`, `:277-344` |
| P9 | `save_dir.join(filename)` uses the server-supplied Content-Disposition filename unvalidated: `../x` or an absolute name escapes the directory ([#151](https://github.com/greenhat616/bolt-load/issues/151)). | `builder.rs:123`, `reqwest.rs:185`, `content_disposition.rs:407` |

## 3. Goals and non-goals

Goals:

- One observation primitive: latest-value status snapshots via `watch`.
- One completion primitive: an awaitable, cancel-on-drop handle.
- Configuration is synchronous and I/O free; all I/O happens inside the task.
- Typed, matchable errors; cancellation is a first-class outcome.
- No regression on the data hot path; publishing is free of consumer
  backpressure.

Non-goals:

- Pause / resume. Both are placeholders today (`task/mod.rs:120`,
  `runner_manager.rs:164`); `TaskState` is `#[non_exhaustive]` so a
  `Paused` state can be added later.
- Resumable downloads and state persistence.
- Re-running a `Task`. A new attempt is a new `Task` from the same builder
  (§4.6).
- Runner-level control (`ControlEvent::LimitTotal`) is unchanged.
- A lossless event log. Consumers that need every transition keep their own.

## 4. Specification

### 4.1 Builder

```rust
impl Task {
    pub fn builder(adapter: impl BoltLoadAdapter + 'static) -> TaskBuilder;
    pub fn builder_arc(adapter: Arc<AnyAdapter>) -> TaskBuilder;
}

#[derive(Clone)]
pub struct TaskBuilder { /* … */ }

impl TaskBuilder {
    pub fn runtime(self, rt: ThreadedRuntimeImpl) -> Self;
    pub fn local_runtime_builder(self, builder: LocalRuntimeBuilderImpl) -> Self;
    pub fn save_to(self, path: impl Into<PathBuf>) -> Self;
    pub fn save_into(self, dir: impl Into<PathBuf>) -> Self;
    pub fn prefer_mode(self, mode: DownloadMode) -> Self;
    pub fn options(self, options: TaskOptions) -> Self;
    pub fn cancel_token(self, parent: CancellationToken) -> Self;
    pub fn meta(self, meta: BoltLoadAdapterMeta) -> Self;
    pub fn cached_meta(&self) -> Option<&BoltLoadAdapterMeta>;

    pub async fn retrieve_meta(&mut self) -> Result<&BoltLoadAdapterMeta, AdapterError>;
    pub fn validate(&self) -> Result<(), BuildError>;
    pub fn spawn(self) -> Result<Task, BuildError>;
}

#[derive(Debug, snafu::Snafu)]
#[non_exhaustive]
pub enum BuildError {
    MissingRuntime,
    MissingDestination,
    InvalidOptions { reason: &'static str },
    InvalidDestination { path: PathBuf, reason: &'static str },
}
```

- **BUILD-1** `runtime` MUST be set explicitly; there is no ambient-runtime
  default. A process that merely contains a tokio runtime MUST NOT be bound
  to it silently.
- **BUILD-2** Exactly one of `save_to` / `save_into` MUST be set; the last
  call wins.
- **BUILD-3** `cancel_token` is optional. The task MUST only ever cancel
  child tokens of it; the caller's token is never cancelled.
- **BUILD-4** `retrieve_meta` retries as `TaskOptions::meta_retry` allows,
  sleeping on the runtime's timer when one is set. It caches the result; a
  later `spawn` MUST reuse it and MUST NOT retrieve metadata again. It does
  not observe the cancel token; dropping the future cancels it. `meta`
  injects known metadata with the same effect.
- **BUILD-5** `validate` and `spawn` MUST NOT perform network I/O. They MAY
  perform best-effort synchronous filesystem checks, at most one `metadata`
  call per path: for `save_to`, the parent is a directory and the target is
  not one; for `save_into`, the directory exists. A bare file name's
  parent is the current directory. These checks are repeated during
  `Initializing` (§4.3), which is authoritative and fails with
  `DownloadError::InvalidDestination`.
- **BUILD-6** `TaskBuilder` is `Clone` (the adapter is shared by `Arc`), so a
  queue can keep validated builders and spawn them later.

### 4.2 Status

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TaskState { Pending, Initializing, Downloading, Finished, Failed, Cancelled }

impl TaskState {
    pub fn is_terminal(self) -> bool; // Finished | Failed | Cancelled
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TaskStatus {
    pub state: TaskState,
    pub progress: Progress,
    pub speed: Speed,
    pub info: Option<Arc<TaskInfo>>,
    pub error: Option<DownloadError>,
    pub leftover_temp_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct Speed { pub download: f64, pub write: f64 }

#[derive(Debug)]
#[non_exhaustive]
pub struct TaskInfo {
    pub meta: BoltLoadAdapterMeta,
    pub mode: DownloadMode,
    pub path: PathBuf, // the resolved destination
}

impl TaskStatus {
    pub fn is_terminal(&self) -> bool;
    /// `None` when the total is unknown or zero.
    pub fn fraction(&self) -> Option<f64>;
}

pub type StatusReceiver = bolt_load_sync::watch::Receiver<TaskStatus>;
```

`Progress` becomes `#[non_exhaustive]` and gains `Progress::new`, so
resume-related fields can be added later.

- **STATUS-1** Transitions MUST follow this graph; a terminal state MUST
  never change:

  ```text
  Pending ──► Initializing ──► Downloading ──► Finished
                   │                │
                   └────────────────┴──► Failed | Cancelled
  ```

  `TaskState` deliberately does not derive `Ord`: `Failed` during
  initialization must not compare as "past `Downloading`".
- **STATUS-2** Status is level-triggered. A receiver is guaranteed the
  newest value only; intermediate states, including `Initializing`, MAY be
  coalesced.
- **STATUS-3** `error` is `Some` exactly when `state` is `Failed` or
  `Cancelled`. `info` is `Some` from the end of `Initializing` on, and stays
  `Some` in every later state.
- **STATUS-4** `progress` is kept in every state, including `Failed` and
  `Cancelled`.
- **STATUS-5** `Speed` is in bytes per second, EMA-smoothed (α = 0.33,
  250 ms interval). The first `Downloading` value has zero speed.
- **STATUS-6** The terminal status MUST be the last value published, and it
  MUST be published only after teardown completes: every runner, writer and
  progress publisher has exited, and the output file has been committed or
  the temporary file removed. If removal fails, `leftover_temp_file` names
  the file; the error that caused the discard is not replaced.
- **STATUS-7** The watch sender MUST be dropped right after the terminal
  value, so `while rx.changed().await.is_ok()` ends after the receiver has
  seen it (watch delivers unseen values before `Closed`).
- **STATUS-8** Publishing MUST NOT wait for any receiver. It MAY briefly
  wait for the watch value lock, which is held only for a pointer swap.
  Terminal publication MUST NOT be skipped when `receiver_count() == 0`.

Internal: one `pub(crate) StatusPublisher` owns the only `watch::Sender`.
The driver owns it; transfers and samplers borrow it and never keep a clone
past their return. After the terminal value it ignores further publishes
(and `debug_assert!`s against them). The private
`runner_manager::TaskState` is renamed `ManagerState`.

### 4.3 Initialization

The spawned attempt runs these steps on its local runtime, in order, while
publishing `Initializing`:

1. Use cached or injected metadata, else retrieve it under `meta_retry`.
2. Select the mode: `prefer_mode(Singleton)`, or an unknown or zero content
   size, selects `Singleton`. Otherwise the range probe decides.
3. Resolve the destination (§4.4) and check the filesystem.
4. Publish `info`, then construct the selected transfer from the
   initialized data.

- **INIT-1** Mode-specific `prepare` MUST NOT retrieve metadata; mode
  dispatch moves from `build` into the attempt, after step 2.
- **INIT-2** Cancellation MUST be observed during retrieval, retry backoff
  and the range probe, and ends the task in `Cancelled`.
- **INIT-3** Runtime misconfiguration (a runtime that cannot downcast to a
  local runtime and no `local_runtime_builder`) MUST end the task in
  `Failed` with `DownloadError::Runtime`, not panic (`drive.rs:82` panics
  today).

### 4.4 Destination resolution

- **DEST-1** `save_to(path)` is used as given.
- **DEST-2** `save_into(dir)` joins exactly one filename component taken
  from `meta.filename`. The name is reduced to its final segment, split on
  both `/` and `\`. It MUST be rejected with
  `DownloadError::InvalidFilename` when that segment is empty, `.` or `..`,
  or contains NUL or control characters. On Windows it MUST also be
  rejected when it contains `:` (drive prefix, alternate data stream), is a
  reserved device name (`CON`, `NUL`, `COM1`, …), or ends in a dot or space.
- **DEST-3** Without `meta.filename`, `save_into` fails with
  `DownloadError::MissingFilename`. This spec defines no URL-based fallback;
  adapters may supply one.
- **DEST-4** The commit rule is unchanged: cancellation observed before the
  rename starts discards the download; once the rename starts, its outcome
  decides (`output_file.rs:43`).

### 4.5 Task handle and completion

```rust
#[must_use = "dropping a Task cancels the download; call `.detach()` to keep it running"]
pub struct Task { /* Option<blocking::Task<()>>, StatusReceiver, Option<DropGuard>, Option<outcome> */ }

impl Task {
    pub fn status(&self) -> StatusReceiver;
    pub fn snapshot(&self) -> Arc<TaskStatus>;
    pub fn is_terminal(&self) -> bool;
    pub fn cancel(&self);
    pub fn cancel_token(&self) -> CancellationToken;
    pub async fn stop(self) -> Result<TaskOutput, DownloadError>;
    pub fn detach(self);
    #[cfg(feature = "progressbar")]
    pub fn drive_progress_bar(&self, pb: indicatif::ProgressBar) -> std::thread::JoinHandle<()>;
}

impl Future for Task {
    type Output = Result<TaskOutput, DownloadError>;
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TaskOutput { pub path: PathBuf, pub progress: Progress, pub mode: DownloadMode }

#[derive(Debug, Clone, snafu::Snafu)]
#[non_exhaustive]
pub enum DownloadError {
    Cancelled,
    Metadata { source: AdapterError },
    Transfer { source: Arc<crate::runner::TaskError> }, // NoProgress, RetryLimitExceeded, Timeout, stream errors
    Io { op: IoOp, path: PathBuf, source: Arc<std::io::Error> },
    InvalidDestination { path: PathBuf, reason: &'static str },
    InvalidFilename { name: String },
    MissingFilename,
    Runtime { message: String },
    Panicked { message: String },
    Aborted,
}

#[non_exhaustive]
pub enum IoOp { Inspect, Create, Allocate, Write, Flush, Rename }

impl DownloadError {
    pub fn is_cancelled(&self) -> bool;
    pub fn as_transfer(&self) -> Option<&crate::runner::TaskError>;
}
```

`DownloadError` replaces `TaskInstanceError` in the public API.
`runner::TaskError` keeps its name for now (see Appendix A.4).

- **TASK-1** Join outcome mapping, read from the final status after the
  attempt closure completes:

  | Final status | Output |
  |---|---|
  | `Finished` | `Ok(TaskOutput)` |
  | `Cancelled` | `Err(DownloadError::Cancelled)` |
  | `Failed` | `Err(cause)` |
  | none (closed without a terminal value) | `Err(DownloadError::Aborted)` |

- **TASK-2** `Task` is `Unpin`; `(&mut task).await` MAY be repeated. The
  blocking task is taken out before `Ready` is returned and never polled
  again; the outcome is cached.
- **TASK-3** `Drop` MUST cancel the child token and then detach the
  underlying blocking task, so the attempt still runs its cancellation and
  cleanup path, including when it had not started yet. `Drop` MUST NOT
  join or block.
- **TASK-4** `detach` disarms the drop guard and detaches the blocking task.
  The status stays observable through receivers taken before: write
  `let rx = task.status(); task.detach();`.
- **TASK-5** `cancel` only requests cancellation and returns at once.
  `stop` cancels and joins; it MAY return `Ok` when the rename had already
  started (DEST-4).
- **TASK-6** `is_terminal` reports that a terminal status is available. The
  join MAY still be pending, since the attempt closure returns after
  publishing.
- **TASK-7** `Task`, `TaskStatus`, `DownloadError`, `StatusReceiver`, the
  status stream and every public waiting future MUST be `Send + Sync` where
  applicable, checked by static assertions. The nightly attempt probe
  (`probe_send.rs`) is kept.
- **TASK-8** Panic policy. The attempt runs under `catch_unwind`. On a panic
  it cancels the attempt's child token (signalling, not joining, the
  workers), removes the temporary file synchronously on a best-effort basis,
  and publishes `Failed(Panicked)`. Awaiting the `Task` returns that error
  instead of resuming the unwind (`blocking` builds its tasks with
  `propagate_panic(true)`). After a panic STATUS-6 is weakened: workers are
  signalled but not joined.
- **TASK-9** `drive_progress_bar` runs a dedicated thread that waits with
  `changed_blocking`. It calls `set_length` once the total is known, leaves
  the bar as a spinner while it is unknown, finishes or abandons the bar on
  the terminal state, and exits when the channel closes. The caller may
  style the bar or add it to a `MultiProgress` before passing it in.

Threading model: each running task occupies one `blocking` pool thread for
its whole lifetime, because the attempt is `block_on`-ed on a local runtime
(rust-lang/rust#100013, see `drive.rs:78`). The pool is shared with every
other `blocking` user, filesystem operations included. Queue managers MUST
bound the number of running tasks well below the pool limit
(`BLOCKING_MAX_THREADS`, default 500).

### 4.6 Re-running

- **RERUN-1** A `Task` is one attempt. To retry, spawn again from a clone
  of the builder; each attempt gets a fresh child token and a fresh watch
  channel. This is an intentional breaking change:
  `test_successful_rerun_clears_previous_error`
  (`termination_tests.rs:623`) becomes "a new task from the same builder
  does not see the previous error".

### 4.7 `bolt_load_sync::watch` additions

```rust
impl<T> Receiver<T> {
    pub async fn wait_for(&mut self, f: impl FnMut(&T) -> bool) -> Result<Arc<T>, Closed>;
    #[cfg(not(target_family = "wasm"))]
    pub fn wait_for_blocking(&mut self, f: impl FnMut(&T) -> bool) -> Result<Arc<T>, Closed>;
    #[cfg(feature = "stream")]
    pub fn into_stream(self) -> WatchStream<T>; // impl Stream<Item = Arc<T>>
}
```

- **WATCH-1** `wait_for` is exactly this algorithm:

  ```rust
  loop {
      let value = self.borrow_and_update();
      if f(&value) {
          return Ok(value);
      }
      self.changed().await?;
  }
  ```

  It checks the current value first. A value published between the check
  and `changed` is caught by `changed`, so the final value is always tested
  before `Closed` is returned. The predicate runs outside the channel lock,
  so the lock stays pointer-only and a panicking predicate cannot poison
  the channel. If the future is cancelled, values the predicate rejected
  stay marked as seen, as in tokio.
- **WATCH-2** `wait_for_blocking` uses the same algorithm with
  `changed_blocking`.
- **WATCH-3** `WatchStream`, behind the `stream` feature (`futures-core`):
  - The first poll yields `borrow_and_update()`, even on a closed channel.
  - Later polls yield an unseen version before reporting closure.
  - When nothing is ready: register an owned `EventListener`, re-check, and
    keep the listener across `Pending`. On notification, clear it and
    re-check.
  - After `None`, it always returns `None`.
  - Each pending wait allocates one listener; at four updates per second
    this is negligible.
- **WATCH-4** No `send_modify`: the publisher keeps its own copy and uses
  `send_replace`, so the lock never covers a clone of `T`.

## 5. Scenarios

**S1 — CLI progress bar.** Given a spawned task, when
`task.drive_progress_bar(ProgressBar::no_length())` is called and the task
is awaited, then the bar gets its length once `Initializing` ends, advances
at the sample rate, finishes on `Finished`, and its thread exits when the
channel closes.

**S2 — "Save as" dialog.** Given a builder, when `retrieve_meta().await`
returns, the application shows `meta.filename` and the size. The user picks
a path and calls `save_to(path).spawn()`. Then the task sends no second
metadata request (BUILD-4).

**S3 — GUI thread.** Given `let rx = task.status()` moved to a non-async
thread, when that thread loops on `changed_blocking()` and posts
`borrow_and_update()` to the UI, then the UI never sees progress go
backwards, and the loop ends after the terminal value.

**S4 — Queue.** Given N cloned, validated builders and one parent token,
when a manager spawns at most K at a time and later cancels the parent,
then every running task ends in `Cancelled`, and the caller's token is the
only token the caller has to cancel.

**S5 — Slow reader.** Given a reader that sleeps 5 s between reads, when the
transfer publishes every 250 ms, then the transfer is not slowed and each
read returns the newest sample (P3, P4).

**S6 — Dropped handle.** Given a running task, when the `Task` is dropped,
then the download is cancelled, the temporary file is removed, and a
receiver taken earlier observes `Cancelled` followed by `Closed`. The same
holds when the drop happens before the attempt starts.

**S7 — Hostile filename.** Given `save_into(dir)` and a server sending
`filename="../../.bashrc"`, the final segment `.bashrc` is used inside
`dir`. Given `filename=".."`, the task fails with `InvalidFilename` and
writes nothing.

**S8 — Panic.** Given an adapter whose stream panics, when the task is
awaited, then it returns `Err(Panicked)` instead of unwinding, and the
final status is `Failed`.

## 6. Performance

- Removed: the bounded event channel, the forwarding task spawned on the
  threaded runtime, per-callback event clones, `AtomicTaskState` and the
  `last_error` mutex.
- Per publish (every 250 ms, plus a few state changes): one `Arc`
  allocation and the `Progress` snapshot. For concurrent tasks that
  snapshot collects, sorts and merges the downloaded ranges
  (`chunk_planner.rs:381`); this cost exists today and is unchanged.
- Watch costs from `bolt-load-sync/DESIGN.md` (model benchmark, macOS,
  single run; medium confidence): about 85 ns per publish with 256 ranges
  and a stalled reader; 256 updates to 16 subscribers in about 73 µs.
- The data path (frames, writes) is untouched.
- Before merging, benchmark the real task reporting path: stalled readers,
  several streams, and fragmented ranges.

## 7. Testing

The event-order tests in `event_contract_tests.rs` become status-contract
tests. The teardown and resource assertions are kept. New coverage:

- STATUS-1 transitions, including the terminal state never changing; the
  terminal value is last (STATUS-6) and is published with zero receivers
  (STATUS-8).
- A slow reader sees the newest sample (S5); a late subscriber reads the
  current value.
- Drop before start, drop while downloading, `detach` before start
  (TASK-3, TASK-4).
- Repeated await (TASK-2), cancellation during initialization (INIT-2),
  panic (TASK-8), failed cleanup (`leftover_temp_file`).
- Hostile filenames (DEST-2).
- `Send + Sync` assertions (TASK-7).
- loom models for `wait_for`: the final send racing close, and a send
  during registration (WATCH-1). Stream tests: an already closed channel at
  construction, a send during registration, a final send followed by close,
  and waker replacement (WATCH-3).

## 8. Migration

| Before | After |
|---|---|
| `TaskBuilder::default().adapter(a)` | `Task::builder(a)` |
| `.threaded_runtime(rt)` | `.runtime(rt)` |
| `.save_path(p.into())` / `.save_dir(d.into())` | `.save_to(p)` / `.save_into(d)` |
| `.cancel_token(t)` (required) | optional |
| `.build().await?` then `task.run().await?` | `.spawn()?` |
| `task.wait().await` | `task.await` or `(&mut task).await` |
| `task.stop().await` | `task.stop().await` (consumes, returns the outcome) |
| `.on_task_state_changed(cb)` | `task.status()` plus your own loop |
| `task.with_progress_bar()` | `task.drive_progress_bar(pb)` |
| `task.is_finished()` / `is_initialized()` | `task.is_terminal()` / `snapshot().state` |
| `TaskInstanceError` | `DownloadError` |
| `TaskManagerBuildError` | `BuildError`; `AdapterError` from `retrieve_meta` |

Suggested PR order:

1. watch additions with loom models.
2. `StatusPublisher` behind the existing API.
3. `Task` handle and removal of the old API.
4. Initialization moved into the attempt, plus destination validation.
5. Progress bar, README and `task.md`.

## 9. Decisions

| Date | Decision | Reason |
| --- | --- | --- |
| 2026-10-30 | Status via `watch`, not an event channel | Latest value is what UIs need; a slow reader cannot stall the transfer; the final value survives the sender (P2-P5). |
| 2026-10-30 | Status is a struct with a `state` field, not an enum with payloads | Progress is readable in every state, including failure (STATUS-4); no `match` needed for a progress bar. |
| 2026-10-30 | `TaskState` has no `Ord` | `Failed` during initialization would compare as past `Downloading`; transitions are tested against the graph instead. |
| 2026-10-30 | `Cancelled` is a state and an error variant | UIs tell it apart from failure without digging through nested sources (P7). |
| 2026-10-30 | `Task` is the spawned, awaitable handle; no pre-spawn type | Matches `async_task::Task` and smol; queues keep `TaskBuilder` (`Clone`, `validate`). Avoids two task types, which the review found confusing. |
| 2026-10-30 | Drop cancels, `detach` opts out | Like `async_task::Task`: no orphan download keeps writing files after its owner is gone. `#[must_use]` flags `task.spawn();`. |
| 2026-10-30 | Synchronous, I/O-free `spawn`; I/O in `Initializing` | One observable, cancellable place for all I/O; removes the repeated retrieval (P1). Cheap synchronous path checks keep obvious mistakes failing fast (BUILD-5). |
| 2026-10-30 | Runtime must be set explicitly | Silently binding to an ambient tokio runtime contradicts the runtime-agnostic design. |
| 2026-10-30 | `StatusReceiver` is an alias, not a wrapper | `bolt-load-sync` is versioned with the workspace; an alias exposes the full watch API without delegation boilerplate. Revisit if the crates are versioned separately. |
| 2026-10-30 | One attempt per `Task` | Reopening a watch channel conflicts with "terminal is last" and with transitions only moving forward. |
| 2026-10-30 | Panics become `Failed(Panicked)` | A library handle should return an error, not unwind in the awaiting task; the weaker teardown guarantee is stated (TASK-8). |

## 10. Review

Reviewed on 2026-10-30 by two models through `/ccg:review`: codex for
protocol and correctness, kimi for API ergonomics.

| Finding | Severity | Resolution |
|---|---|---|
| `save_into` path traversal through server filenames | Critical | DEST-2, DEST-3, S7. Exists today (P9). |
| Drop vs `blocking::Task` cancellation | Major | TASK-3, TASK-4 |
| Panic propagation and missing terminal status | Major | TASK-1 (`Aborted`), TASK-8 |
| `local_runtime_builder` dropped; ambient runtime | Major | Kept (§4.1); BUILD-1 |
| Mode dispatch boundary for moved initialization | Major | §4.3, INIT-1 to INIT-3 |
| "Committed or removed" stronger than cleanup | Major | STATUS-6 `leftover_temp_file`; DEST-4 |
| Error and join mapping incomplete | Major | TASK-1, TASK-2, TASK-6 |
| `wait_for` and stream algorithms unspecified | Major | WATCH-1 to WATCH-3 |
| Rerun removed silently | Major | RERUN-1 |
| `stop` semantics unclear | Critical (kimi) | TASK-5 |
| `Task` / `TaskHandle` split confusing | Major | Single `Task` handle (§9) |
| Progress bar built before the total is known | Major | TASK-9 |
| Path validation no longer fails fast | Major | BUILD-5 |
| Threading model unstated | Major | §4.5, threading model |
| Two verbs and error types for metadata | Major | `retrieve_meta` → `AdapterError`; in a task, `DownloadError::Metadata` |
| Performance wording | Minor | §6, STATUS-8 |
| P1 and P4 over-stated | Minor | Corrected in §2 |
| Same-destination temporary file ownership | Major | Open (§11, [#152](https://github.com/greenhat616/bolt-load/issues/152)) |

## 11. Open questions

- **Temporary file ownership**
  ([#152](https://github.com/greenhat616/bolt-load/issues/152)). Two tasks
  with the same destination share `<dest>.downloading`
  (`output_file.rs:20`), and the singleton task opens it with truncation
  (`singleton_task.rs:175`). Candidates: an in-process registry that
  rejects a second running task for the same destination (cross-process
  stays unprotected), unique per-attempt temporary names (conflicts with
  future resume), or a file lock. This exists today and is out of this
  spec's scope; it should be decided before resume work. Room for the fix:
  the temporary path is derived only by `OutputFile::new`, once per
  attempt during initialization, and a new `DownloadError` variant fits
  the `#[non_exhaustive]` enum.
- **Empty content of unknown size**
  ([#153](https://github.com/greenhat616/bolt-load/issues/153)). A zero
  `content_size` means "unknown", so an empty stream fails the singleton
  runner with `TaskError::Empty` instead of producing an empty file. This
  spec only unifies the mode selection (§4.3, step 2).
- Whether `TaskId` is needed for correlation in a future multi-task manager.

## 12. Implementation notes

Implemented in one change after this spec, verified with the workspace
tests and the loom models. Where it differs from or refines §4:

| Item | Implementation |
|---|---|
| Destination errors during `Initializing` | `DownloadError::InvalidDestination { path, reason }` mirrors `BuildError::InvalidDestination`; metadata read errors other than "not found" are `Io { op: Inspect }`. |
| File errors | Singleton: `Io` with `Create`, `Allocate`, `Write`, `Flush` on the temporary path. Concurrent writer errors keep their old mapping, `Transfer(Other { message })`. A failed rename is `Io { op: Rename, path: dest }`. |
| Singleton connect failure | `Transfer(StreamError { source })`, keeping the adapter's error (previously `StreamFailed`). |
| `Task::status()` | Returns a receiver that has seen only the initial `Pending` value, so `while rx.changed().await.is_ok()` also reports a task that already ended. |
| `TaskBuilder::cached_meta` | Added: reads the metadata from `meta` or a completed `retrieve_meta`. |
| Sample cost | Progress samples are skipped while the channel has no receiver; the terminal status is always published (STATUS-8). |
| Panic handler | Holds a second handle to the status sender, so the channel closes when the attempt's thread returns, right after the terminal status. |
| Probe | `probe_send.rs` now probes the single attempt future, `drive::run`. |
| Zero size | Selects the singleton mode; an empty stream still fails with `Empty` (#153). `test_zero_size_file_handling` stays ignored. |
| Removed | `TaskEvent`, `ProgressWithSpeed`, `TaskInstanceError`, `TaskManagerBuildError`, `TaskInstance`, `TaskInstanceImpl`, `SingletonTask`, `ConcurrentTask`, `TaskControl`, the atomic `TaskState`, `TaskStateControl`, `TaskCommand`, the `atomic_enum` dependency. |

Loom: the four new watch models pass with the existing seven watch and
slot models (15 in total). The two "racing close" models are the slowest
(several minutes in release mode on the development machine), since the
second wait or poll doubles the explored interleavings.

## Appendix A. Naming survey

### A.1 Method

Count `pub enum` names by suffix across the local cargo registry cache
(`$CARGO_HOME/registry/src`, 3439 crate directories on 2026-10-30):

```sh
grep -rhoE "pub enum [A-Za-z]*State\b" "$CARGO_HOME/registry/src" --include=*.rs | wc -l
```

Limitations: the sample is the dependency closure of one machine, and
several versions of one crate count separately. The ratios are robust; the
absolute numbers are not.

### A.2 Results

| Suffix | `pub enum` count | Typical meaning |
|---|---|---|
| `State` | 545 | Lifecycle and state machines: `ConnectionState`, `RtcPeerConnectionState`, `TaskState` |
| `Status` | 176 | Outcome or status code: `TaskStatus`, `UsbTransferStatus` |
| `Phase` | 34 | Not entity lifecycles (below) |
| `Stage` | 6 | Pipeline stages: shader stages in `naga`, raster pipeline in `tiny-skia` |

The `Phase` hits are input-event phases (`TouchPhase` in winit and tao), a
syntax variant (`ImportPhase` in oxc: `import source` / `import defer`) and
internal steps of one async operation (turso: `OpenDbAsyncPhase`,
`SubqueryEvalPhase`). Restricted to lifecycle enums
(`Task|Job|Download|Transfer|Connection|Lifecycle` + suffix), every hit is
`State` or `Status`; none is `Phase`.

Using "phase" for a lifecycle is mostly a Kubernetes convention
(`PodStatus.phase`); this was not verified in this survey.

Confidence: high that `State` is the dominant suffix for lifecycle enums;
medium for the exact ratios.

### A.3 Outcome

- Lifecycle enum: `TaskState`. This keeps the existing public name; an
  earlier draft's `TaskPhase` was dropped.
- Snapshot: `TaskStatus { state, … }`. "Status is a data-carrying snapshot,
  state is the discriminant" has precedent: web-sys `RtcPeerConnectionState`
  beside its connection objects, and Kubernetes `PodStatus` holding a
  `phase`.
- The private `runner_manager::TaskState` becomes `ManagerState`, so the
  crate has one `TaskState`.

### A.4 Other names

| Name | Choice | Note |
|---|---|---|
| Spawned handle | `Task` | `async_task::Task` and smol use `Task` for the awaitable, cancel-on-drop handle; std and tokio use `JoinHandle` with detach-on-drop. The drop semantics follow `async_task`, so the name does too. |
| Builder entry | `Task::builder(adapter)` | Required argument in the constructor (Rust API Guidelines, C-BUILDER). |
| Metadata verb | `retrieve_meta` | Same verb as `BoltLoadAdapter::retrieve_meta`; no second verb such as "fetch". |
| Terminal check | `is_terminal` | `is_finished` would be ambiguous next to `TaskState::Finished`, which means success only. |
| Error | `DownloadError` | `TaskError` is taken by `runner::TaskError`. Renaming that type is a separate change. |

### A.5 References

- tokio `watch::Receiver::wait_for`, `Sender::send_if_modified`:
  <https://docs.rs/tokio/latest/tokio/sync/watch/>
- tokio-stream `WatchStream::new` yields the current value first:
  <https://docs.rs/tokio-stream/latest/tokio_stream/wrappers/struct.WatchStream.html>
- `async_task::Task`: drop cancels, `detach` keeps it running:
  <https://docs.rs/async-task/latest/async_task/struct.Task.html>
- tokio `JoinHandle`: drop detaches:
  <https://docs.rs/tokio/latest/tokio/task/struct.JoinHandle.html>
- `blocking` builds tasks with `propagate_panic(true)`: blocking 1.7.0,
  `src/lib.rs:253`
- Rust API Guidelines: <https://rust-lang.github.io/api-guidelines/>
