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
- **Progress & observability** — EMA speed sampling, progress events, optional `indicatif` progress bars, and `tracing` integration.
- **Structured errors** — two-level retryable/unretryable classification via `snafu`, enabling intelligent retry decisions.

## Quick Start

```rust,ignore
use bolt_load::task::TaskBuilder;
use bolt_load::adapter::reqwest::IntoReqwestAdapter;
use bolt_load::runtime::ThreadedRuntimeImpl;

let client = reqwest::Client::new();
let adapter = client.into_reqwest_adapter((
    reqwest::Method::GET,
    "https://example.com/large-file.bin".parse().unwrap(),
));

let runtime = ThreadedRuntimeImpl::new_tokio_rt();

let mut task = TaskBuilder::default()
    .adapter(Box::new(adapter))
    .save_path("large-file.bin".into())
    .threaded_runtime(runtime)
    .build()
    .await?;

task.with_progress_bar();
task.run().await?;
task.wait().await?;
```

## Architecture

```text
┌──────────────────────────────────────────────────────┐
│                       Task                           │
│  ┌──────────────────┐   ┌──────────────────────────┐ │
│  │  SingletonTask    │   │    ConcurrentTask        │ │
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
| **Singleton** | Adapter lacks range support, or content is small | Single `TaskRunner` streams the full content sequentially |
| **Concurrent** | Adapter supports range requests | Multiple `TaskRunner` instances download non-overlapping byte ranges in parallel, with dynamic splitting of slow ranges |

Mode selection is automatic based on `is_range_stream_available()`,
or can be forced via `TaskBuilder::prefer_mode()`.

## Concurrency Control

The concurrent download engine continuously monitors runner outcomes:

1. **Normal** — runners spawn up to `DEFAULT_MAX_CONCURRENCY` (CPU core count).
2. **Degraded** — when the failure rate exceeds 30% over a sliding window,
   concurrency is reduced by `degradation_step` at a time.
3. **Recovering** — after a `recovery_delay` (30 s), the engine probes with
   one additional runner and observes for 15 s before committing.

Slow or stalled runners have their remaining byte ranges split and
reassigned to new runners via the `DynamicStrategy`.

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

#[async_trait]
impl BoltLoadAdapter for MyAdapter {
    async fn is_range_stream_available(&self) -> bool { /* ... */ }
    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError> { /* ... */ }
    async fn full_stream(&self) -> Result<AnyBytesStream, AdapterError> { /* ... */ }
    async fn range_stream(&self, start: u64, end: u64) -> Result<AnyBytesStream, AdapterError> { /* ... */ }
}
```

## Workspace Crates

| Crate | Purpose |
|-------|---------|
| **bolt-load** | Main download engine (this crate) |
| **bolt-load-core** | Core `BoltLoadAdapter` trait and error types |
| **bolt-load-adapter** | HTTP adapter implementations (reqwest, ureq) |
| **bolt-load-utils** | HTTP parsing, cross-runtime streams, conditional telemetry |
| **bolt-load-tests** | Test adapters (in-memory, HTTP server) and fixtures |

## MSRV

Rust 1.88.0+ (Edition 2024)
