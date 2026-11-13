# bolt-load

A high-performance, runtime-agnostic download engine for Rust.

bolt-load provides both single-stream and multi-stream concurrent
downloads with automatic range detection, dynamic concurrency control,
and multiple I/O backends. It is designed as an embeddable library —
bring your own HTTP client, pick your async runtime, and bolt-load
handles the rest.

## Features

- **Multi-runtime** — first-class Tokio, Smol, and Compio (io_uring) support via enum dispatch; plug in custom runtimes through traits.
- **Concurrent range downloads** — automatic byte-range splitting with dynamic rebalancing of slow or stalled runners.
- **Adaptive concurrency** — failure-rate-based degradation and recovery strategy keeps throughput high under unstable network conditions.
- **Multiple file-write backends** — memory-mapped I/O, thread-pool `pwrite`, Compio/io_uring, or `/dev/null` for benchmarking.
- **Backpressure-aware** — lock-free SPSC ring-buffer channels between runners and writers with configurable queue depth.
- **Progress & observability** — latest-value status snapshots with EMA speed, optional `indicatif` progress bars, and `tracing` integration.
- **Structured errors** — two-level retryable/unretryable classification via `snafu`, enabling intelligent retry decisions.

## Quick Start

```rust,ignore
use bolt_load::task::Task;
use bolt_load::adapter::reqwest::IntoReqwestAdapter;
use bolt_load::runtime::ThreadedRuntimeImpl;

let client = reqwest::Client::new();
let adapter = client.into_reqwest_adapter((
    reqwest::Method::GET,
    "https://example.com/large-file.bin".parse().unwrap(),
));

let task = Task::builder(adapter)
    .runtime(ThreadedRuntimeImpl::new_tokio_rt())
    .save_to("large-file.bin")
    .spawn()?;

// Observe the latest status; samples may be coalesced.
let mut status = task.status();
while status.changed().await.is_ok() {
    let s = status.borrow_and_update();
    println!("{:?} {}/{:?}", s.state, s.progress.downloaded, s.progress.total);
}
let output = task.await?;
```

## Architecture

```text
┌──────────────────────────────────────────────────────┐
│                       Task                           │
│  ┌──────────────────┐   ┌──────────────────────────┐ │
│  │  Singleton mode   │   │    Concurrent mode       │ │
│  │  (1 runner)       │   │    (N runners)           │ │
│  └────────┬─────────┘   └────────┬─────────────────┘ │
│           │                      │                    │
│           ▼                      ▼                    │
│       TaskRunner[]         RunnerManager              │
│           │                ├─ ChunkPlanner            │
│           │                ├─ StrategyControl         │
│           │                ├─ SpeedSampler            │
│           │                └─ FileWriter              │
│           ▼                                           │
│       AnyAdapter (BoltLoadAdapter trait)               │
└──────────────────────────────────────────────────────┘
```

### Modules

| Module | Description |
|--------|-------------|
| [`adapter`] | Content source abstraction and HTTP implementations (reqwest, ureq) |
| [`runtime`] | Async runtime abstraction (Tokio, Smol, custom) with enum dispatch |
| [`runner`]  | Low-level stream consumer with buffering, backpressure, and lifecycle events |
| [`task`]    | High-level download orchestration, state machine, and progress tracking |

Detailed module documentation is maintained in the [`docs/`](docs/)
directory and included via `#![doc = include_str!(...)]`.

## Download Modes

| Mode | When | How |
|------|------|-----|
| **Singleton** | Adapter lacks range support, the size is unknown, or ranges turn out unusable (fallback) | Single `TaskRunner` streams the full content sequentially |
| **Concurrent** | Adapter supports range requests | Multiple `TaskRunner` instances download non-overlapping byte ranges in parallel, with dynamic splitting of slow ranges |

Mode selection is automatic based on `is_range_stream_available()` and a
known size, or the singleton mode can be forced via `TaskBuilder::prefer_mode()`.

A content-coded full response is decoded as it is written, with up to two
codings: gzip and deflate by default, br and zstd behind features. Its
size is then unknown until the stream ends. A coding the build cannot
decode fails the task with `ContentCoded`. Range responses are never
decoded: a coded range makes the task fall back to one connection.

## Concurrency Control

The concurrent download engine continuously monitors runner outcomes:

1. **Normal** — runners spawn up to `TaskOptions::max_connections` (8 by
   default, whatever the CPU count).
2. **Degraded** — when the failure rate exceeds 30% over a sliding window,
   concurrency is reduced by `degradation_step` at a time.
3. **Recovering** — after a `recovery_delay` (30 s), the engine probes with
   one additional runner and observes for 15 s before committing.

Runners are added by doubling. At most every 2 s, while no runner is
connecting, every running runner delivers data and the task is not held
back by a 429 or 503, the engine doubles the runners up to the current
limit, by splitting the running ranges with the most left. A running
runner that has been silent for 2 s, and running at least that long,
does not hold the growth back: silent means it read nothing of its body
and is not pushing a frame. That is not so while the writer is full, nor
for a runner with frames the engine has not taken yet. A split first
reserves a slot for the new runner, so it never exceeds
`max_connections`, and needs at least 2 s of work at the split runner's
own rate. Once a doubling gains less than 10% in speed, no more runners
are added.

A runner that delivered under a quarter of the median of at least three
runners over the last 10 s, allowing one 32 KiB buffer either way, is
stopped when it has at least twice the minimum chunk size left. So is a
runner silent for 4.5 s, and running at least that long, without three
runners or a full 10 s: it needs at least twice the minimum chunk size
left, no hedge, no frames the engine has not taken yet, a writer that is
not full and no 429 or 503 holding the task back. At most one runner a
strategy tick is stopped. Its range is retried from the bytes already
received after a backoff, which counts against the range's budget only
when the runner had received nothing. In the endgame, when nothing is
left to split, a runner with less than twice the minimum chunk size left
that has delivered nothing for 2 s is hedged: a second runner requests
the rest of its range again, the first to deliver goes on and the other
is stopped, so no byte is written twice.

**Breaking change.** Doubling is the only growth strategy. The unstable
feature flag that chose it, `TaskOptions::growth_strategy`,
`TaskBuilder::growth_strategy` and `GrowthStrategy` are gone; a crate
that enables that feature no longer builds. The speed-threshold splitting
that was the default before is removed.

A failed range is retried after a backoff: 0.5 s after its first
failure, doubling up to 30 s, with jitter. A 429 or 503 holds back every
new connection of the task, splits included, for its `Retry-After` when
that is longer, at most 120 s. The end of a backoff wakes the engine,
which then creates runners as the concurrency control would: only into
free slots, and after a 429 or 503 only once the concurrency has been
lowered. A range it cannot retry then waits for the next strategy
evaluation.

Healthy connections cannot hide a range that keeps failing: once a range
has failed `TaskOptions::max_attempts_per_range` times (5 by default),
the task fails with `TaskError::RangeExhausted`, which names the range
and carries its last error. A timeout or a dropped stream after the
attempt delivered bytes only restarts the range: it backs off, but is
counted neither for the range nor as a consecutive failure. A 429 or 503
is not counted for the range either, nor is a connect refused above what
the server was seen to admit: one that failed before it started, while
another connection had delivered bytes and at least as many others were
in flight as had delivered at once since the connection limit was last
lowered. Every other retryable failure is counted, a lost runner even
after it delivered bytes. While refusals stay exempt nothing bounds them
per range; the task then ends once the other connections have the rest,
or by its no-progress and consecutive-failure limits.

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `tokio` | Yes | Tokio async runtime support |
| `smol` | No | Smol async runtime support |
| `compio` | No | Compio with io_uring file writer backend |
| `reqwest` | Yes | Reqwest HTTP adapter |
| `ureq2` | No | Ureq 2.x HTTP adapter (blocking, wrapped with `blocking` crate) |
| `ureq3` | No | Ureq 3.x HTTP adapter |
| `http` | No | HTTP header parsing utilities (Content-Disposition, etc.) |
| `serde` | Yes | Serialization support for progress types |
| `tracing` | Yes | Distributed tracing instrumentation |
| `progressbar` | Yes | Terminal progress bars via `indicatif` |
| `mmap` | Yes | Memory-mapped file writer backend |
| `gzip`, `deflate` | Yes | Decode content-coded full downloads |
| `br` | No | Decode `br` |
| `zstd` | No | Decode `zstd`; builds the libzstd C library |

## File Writer Backends

| Backend | Feature | Best For |
|---------|---------|----------|
| `MmapWriter` | `mmap` | Random-access writes, medium-size files |
| `PoolWriter` | *(default)* | Portable `pwrite` via thread pool |
| `CompioWriter` | `compio` | io_uring async I/O on Linux |
| `NullWriter` | *(test only)* | Benchmarking download speed without disk I/O |

## Custom Adapters

Implement the `BoltLoadAdapter` trait to download from any source:

```rust,ignore
use bolt_load_core::adapter::*;

// Plain `async fn`s; the futures must be `Send`.
impl BoltLoadAdapter for MyAdapter {
    async fn is_range_stream_available(&self) -> bool { /* ... */ }
    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError> { /* ... */ }
    async fn full_stream(&self) -> Result<FullResponse, AdapterError> { /* ... */ }
    async fn range_stream(&self, start: u64, end: u64) -> Result<AnyBytesStream, AdapterError> { /* ... */ }
}
```

## Workspace Crates

| Crate | Purpose |
|-------|---------|
| **bolt-load** | Main download engine (this crate) |
| **bolt-load-core** | Core `BoltLoadAdapter` trait and error types |
| **bolt-load-adapter** | HTTP adapter implementations (reqwest, ureq) |
| **bolt-load-http** | Status, range and content-coding checks and header parsing for HTTP adapters |
| **bolt-load-decode** | Streaming gzip, deflate, br and zstd decoding of response bodies |
| **bolt-load-utils** | Cross-runtime streams, conditional telemetry |
| **bolt-load-tests** | Test adapters (in-memory, HTTP server) and fixtures |

## MSRV

Rust 1.88.0+ (Edition 2024)
