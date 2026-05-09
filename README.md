<p align="center">
    <img src="./.github/bolt-load-transparent.svg" alt="bolt-load" width="200" height="200">
</p>

<h3 align="center">High-performance, runtime-agnostic download engine for Rust</h3>

<div align="center">

[![CI](https://github.com/greenhat616/bolt-load/actions/workflows/ci.yml/badge.svg)](https://github.com/greenhat616/bolt-load/actions/workflows/ci.yml)
[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/greenhat616/bolt-load)
![MSRV](https://img.shields.io/badge/MSRV-1.88.0-blue)
![Rust](https://img.shields.io/badge/Rust-2024_edition-orange)

</div>

---

**bolt-load** is an embeddable Rust library for single-stream and multi-stream concurrent file downloads. It automatically detects HTTP range support, splits files into parallel byte-range chunks, and dynamically rebalances slow connections — all while staying runtime-agnostic.

## Highlights

- **Pluggable adapters** — bring any download source via the `BoltLoadAdapter` trait; ships with reqwest and ureq.
- **Multi-runtime** — Tokio, Smol out of the box; custom runtimes via traits.
- **Adaptive concurrency** — failure-rate monitoring with automatic degradation and recovery.
- **Dynamic range splitting** — slow runners have their remaining ranges reassigned in real time.
- **Multiple I/O backends** — mmap, pwrite thread pool, compio writer.
- **Backpressure-aware** — lock-free SPSC ring-buffer channels prevent memory blow-up.
- **Structured errors** — retryable vs. unretryable classification drives intelligent retry logic.
- **Observable** — EMA speed sampling, progress events, `indicatif` progress bars, `tracing` spans.

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
bolt-load = "0.1"
reqwest = "0.13"
tokio = { version = "1", features = ["full"] }
```

Download a file with concurrent range requests and a terminal progress bar:

```rust,ignore
use bolt_load::task::TaskBuilder;
use bolt_load::adapter::reqwest::IntoReqwestAdapter;
use bolt_load::runtime::ThreadedRuntimeImpl;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let adapter = client.into_reqwest_adapter((
        reqwest::Method::GET,
        "https://example.com/large-file.bin".parse()?,
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

    Ok(())
}
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

**Task** is the user-facing entry point. It selects between two download strategies automatically:

| Mode | Condition | Behavior |
|------|-----------|----------|
| **Singleton** | Adapter lacks range support | Single `TaskRunner`, sequential stream |
| **Concurrent** | Adapter supports byte ranges | N parallel `TaskRunner`s with dynamic splitting |

### Concurrency Control

The engine monitors runner failure rates over a sliding window:

1. **Normal** — spawn up to CPU-core-count runners.
2. **Degraded** — failure rate > 30% triggers stepwise concurrency reduction.
3. **Recovering** — after 30 s cooldown, probe with +1 runner; commit if stable over 15 s.

### File Writer Backends

| Backend | Feature | Description |
|---------|---------|-------------|
| `MmapWriter` | `mmap` | Memory-mapped random-access writes |
| `PoolWriter` | *(default)* | Thread-pool `pwrite` (portable) |
| `CompioWriter` | `compio` | io_uring-based async writes (Linux) |
| `NullWriter` | — | Discard output for benchmarking |

## Workspace

| Crate | Description |
|-------|-------------|
| [**bolt-load**](crates/bolt-load) | Main download engine |
| [**bolt-load-core**](crates/bolt-load-core) | `BoltLoadAdapter` trait and error types |
| [**bolt-load-adapter**](crates/bolt-load-adapter) | HTTP adapters: reqwest, ureq |
| [**bolt-load-utils**](crates/bolt-load-utils) | HTTP parsing, cross-runtime streams, telemetry |
| [**bolt-load-tests**](crates/bolt-load-tests) | Test adapters and HTTP server fixtures |

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `tokio` | ✓ | Tokio async runtime |
| `smol` | | Smol async runtime |
| `compio` | | Compio (io_uring) file writer |
| `reqwest` | ✓ | Reqwest HTTP adapter |
| `ureq2` | | Ureq 2.x adapter (blocking) |
| `ureq3` | | Ureq 3.x adapter (blocking) |
| `http` | | HTTP header parsing (Content-Disposition) |
| `serde` | ✓ | Serialization for progress types |
| `tracing` | ✓ | Distributed tracing instrumentation |
| `progressbar` | ✓ | `indicatif` terminal progress bars |
| `mmap` | ✓ | Memory-mapped file writer |

## Custom Adapters

Implement `BoltLoadAdapter` to download from any source (S3, FTP, in-memory, …):

```rust,ignore
#[async_trait]
impl BoltLoadAdapter for MySource {
    async fn is_range_stream_available(&self) -> bool { true }
    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError> { /* ... */ }
    async fn full_stream(&self) -> Result<AnyBytesStream, AdapterError> { /* ... */ }
    async fn range_stream(&self, start: u64, end: u64) -> Result<AnyBytesStream, AdapterError> { /* ... */ }
}
```

## Development

Install [Lefthook](https://lefthook.dev/) and [Deno](https://deno.com/) before
working on this repository.

```bash
lefthook install
```

The configured Git hooks run the following checks:

- `cargo fmt --all -- --check` — Rust formatting.
- `cargo clippy --all-targets --all-features -- -D warnings` — Rust linting.
- `deno fmt --check` — TypeScript/JavaScript formatting.
- `deno lint` — TypeScript/JavaScript linting.
- `@commitlint/cli` — Conventional Commits validation.

Commit messages must follow Conventional Commits:

```text
feat: add resumable download support
fix: handle zero-length range responses
refactor: migrate error types to snafu
```

### Thread Control Algorithm

The concurrent download engine uses a two-phase approach to find optimal parallelism:

#### Slow Start

Start with 1 runner, doubling each period until additional runners stop producing significant throughput gains:

```math
\begin{align}
\text{if} \quad &|total\_speed_t - 2 \times total\_speed_{t-1}| < THRESHOLD_1 \\
\text{then} \quad &thread_t = 2 \times thread_{t-1} \\
\text{else} \quad &\text{break}
\end{align}
```

#### Fine-Tuning

After slow start, add one runner at a time by splitting the chunk with the longest remaining range:

```math
\begin{align}
\text{if} \quad &|total\_speed_t - (total\_speed_{t-1} + avg\_speed)| > THRESHOLD_2 \;\&\&\; thread_t < MAX \\
\text{then} \quad &thread_t = thread_{t-1} + 1 \\
\text{else} \quad &thread_t = thread_{t-1}
\end{align}
```

## MSRV

Rust **1.88.0+** (Edition 2024)
