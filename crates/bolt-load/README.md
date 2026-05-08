# bolt-load

A high-performance, runtime-agnostic download engine for Rust.

bolt-load provides both single-stream and multi-stream concurrent
downloads with automatic range detection, dynamic load balancing, and
multiple I/O backends.

## Quick Start

```rust,ignore
use bolt_load::task::Task;
use bolt_load::adapter::reqwest::IntoReqwestAdapter;

let client = reqwest::Client::new();
let adapter = client.into_reqwest_adapter((
    reqwest::Method::GET,
    "https://example.com/file.bin".parse().unwrap(),
));

let task = Task::builder()
    .adapter(Box::new(adapter))
    .save_path("file.bin".into())
    .threaded_rt(runtime)
    .build()?;

task.run();
task.wait().await;
```

## Architecture

```text
┌─────────────────────────────────────────────┐
│                  Task                       │
│  ┌────────────────┐  ┌───────────────────┐  │
│  │ SingletonTask   │  │  ConcurrentTask   │  │
│  │ (1 runner)      │  │  (N runners)      │  │
│  └──────┬─────────┘  └──────┬────────────┘  │
│         │                   │               │
│         ▼                   ▼               │
│     TaskRunner[]    RunnerManager           │
│         │           ├─ StrategyControl      │
│         │           ├─ SpeedSampler         │
│         │           └─ FileWriter           │
│         ▼                                   │
│     AnyAdapter (BoltLoadAdapter trait)       │
└─────────────────────────────────────────────┘
```

### Modules

| Module | Description |
|--------|-------------|
| [`adapter`] | Content source abstraction and HTTP implementations |
| [`runtime`] | Async runtime abstraction (Tokio, Smol, custom) |
| [`runner`]  | Low-level stream consumer with buffering and backpressure |
| [`task`]    | High-level download orchestration and state machine |

Detailed module documentation is maintained in the [`docs/`](docs/)
directory and included via `#![doc = include_str!(...)]`.

## Download Modes

| Mode | When | How |
|------|------|-----|
| **Singleton** | Adapter lacks range support, or content is small | Single `TaskRunner` streams the full content sequentially |
| **Concurrent** | Adapter supports range requests | Multiple `TaskRunner` instances download non-overlapping byte ranges in parallel, with dynamic splitting of slow ranges |

Mode selection is automatic based on `is_range_stream_available()`,
or can be forced via `TaskBuilder`.

## Features

| Feature | Default | Description |
|---------|---------|-------------|
| `tokio` | Yes | Tokio async runtime |
| `smol` | No | Smol async runtime |
| `compio` | No | io_uring file writer backend |
| `reqwest` | Yes | Reqwest HTTP adapter |
| `ureq2` | No | Ureq 2 HTTP adapter |
| `http` | No | HTTP header parsing utilities |
| `serde` | Yes | Serialization support |
| `tracing` | Yes | Distributed tracing |
| `progressbar` | Yes | Terminal progress bars via indicatif |
| `mmap` | Yes | Memory-mapped file writer |

## Crate Structure

This workspace contains:

| Crate | Purpose |
|-------|---------|
| **bolt-load** | Main download engine (this crate) |
| **bolt-load-core** | Core adapter trait and error types |
| **bolt-load-adapter** | HTTP adapter implementations (reqwest, ureq) |
| **bolt-load-utils** | HTTP parsing, cross-runtime streams, telemetry |
| **bolt-load-tests** | Test adapters and HTTP server fixtures |

## MSRV

Rust 1.88.0+

## License

See the repository root for license information.
