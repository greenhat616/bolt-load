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

# State Machine Internals

Both [`SingletonTask`] and [`ConcurrentTask`] are driven by `statig`
async state machines with three states:

1. **Stopped** — initial/terminal state, holds the final result.
2. **Initializing** — calls [`retrieve_meta()`] to get content size
   and filename, pre-allocates the output file.
3. **Downloading** — spawns runner(s), samples speed, publishes
   [`TaskEvent`] progress updates.

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
- Requires an adapter and save path.
- Auto-selects download mode based on range stream availability.
- Validates parent directory exists and target is not a directory.
- Optionally attaches a progress bar (feature `progressbar`).

# Key Constants

| Name | Value | Purpose |
|------|-------|---------|
| `DEFAULT_MAX_CONCURRENCY` | CPU core count | Initial concurrent runners |
| `DEFAULT_SAMPLE_INTERVAL` | 250 ms | Speed measurement period |
| `DEFAULT_STRATEGY_TICK_INTERVAL` | 1.5 s | Strategy evaluation period |
| `FILE_WRITER_QUEUE_SIZE` | 2048 | Pending write queue depth |
