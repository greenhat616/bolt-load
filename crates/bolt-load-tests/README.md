# bolt-load-tests

Test utilities and fixtures for the [bolt-load](https://github.com/aspect-build/bolt-load) download framework.

This crate provides reusable test adapters and an HTTP server fixture
for integration testing download scenarios with configurable throughput,
latency, concurrency, and failure modes.

## Test Adapters

### `SimpleTestAdapter`

In-memory adapter with deterministic content, built via a fluent API:

```rust,ignore
use bolt_load_tests::adapter::simple::SimpleTestAdapter;

let adapter = SimpleTestAdapter::builder()
    .content_size(1024 * 1024)        // 1 MB
    .support_range(true)              // enable range requests
    .chunk_size(8192)                 // 8 KB chunks
    .max_speed(100_000)               // 100 KB/s global limit
    .max_per_stream_speed(50_000)     // 50 KB/s per stream
    .max_concurrent_streams(4)        // at most 4 simultaneous streams
    .connecting_delay(Duration::from_millis(100))
    .build()
    .unwrap();
```

**Capabilities:**

| Feature | Description |
|---------|-------------|
| Deterministic content | Reproducible byte sequences with BLAKE3 hash verification |
| Rate limiting | Global and per-stream throughput caps via `governor` |
| Concurrency control | Atomic counter with configurable max stream limit |
| Connection delay | Simulates network latency before stream delivery |
| Failure injection | `should_fail(true)` makes all methods return errors |
| Call counting | Atomic method invocation counter for lifecycle verification |
| Range requests | Optional `[start, end)` byte-range streaming |

**Presets:**

- `SimpleTestAdapter::large()` — 100 MB with defaults
- `SimpleTestAdapter::large_with_range_support()` — 100 MB + range requests

### HTTP Server

Axum-based HTTP server for testing real HTTP downloads:

```rust,ignore
use bolt_load_tests::adapter::http_server::create_http_server;

let (port, handle) = create_http_server().await?;
// GET http://127.0.0.1:{port}/range    — supports Range headers (206)
// GET http://127.0.0.1:{port}/no_range — ignores Range headers (200)
```

- Auto-selects an unused port via `portpicker`
- Serves random file content of configurable size (default 1 MB)
- `/range` endpoint uses `axum-range` for proper HTTP 206 responses
- `/no_range` endpoint always returns the full file

## Content Verification

Test content is generated deterministically via `create_deterministic_content()`
(u64 counter sequences in little-endian), enabling hash-based integrity
checks with `calculate_blake3()`.

## Dependencies

Key testing dependencies: `tokio` (async runtime), `governor`
(rate limiting), `axum` + `axum-range` (HTTP server), `blake3`
(content hashing), `rand` (random file generation), `tempfile`,
`async-stream`.

## License

See the repository root for license information.
