# bolt-load-core

Core types and traits for the [bolt-load](https://github.com/aspect-build/bolt-load) download framework.

This crate defines the foundational abstractions that adapters and the
download engine build upon. It is intentionally minimal — no runtime,
no I/O — so that adapter implementations can depend on it without
pulling in the full framework.

## Adapter Trait

The central abstraction is [`BoltLoadAdapter`], an async trait for
streaming content delivery. Implementations write plain `async fn`s whose
futures must be `Send`; [dynosaur](https://crates.io/crates/dynosaur)
generates the type-erased `DynBoltLoadAdapter`.

```rust,ignore
pub trait BoltLoadAdapter: Send + Sync {
    /// Whether this source supports byte-range requests.
    async fn is_range_stream_available(&self) -> bool { false }

    /// Fetch metadata (content size, suggested filename).
    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError>;

    /// Stream the entire content.
    async fn full_stream(&self) -> Result<AnyBytesStream, AdapterError>;

    /// Stream a byte range `[start, end)`.
    async fn range_stream(&self, start: u64, end: u64) -> Result<AnyBytesStream, AdapterError>;
}
```

### Metadata

```rust,ignore
#[non_exhaustive]
pub struct BoltLoadAdapterMeta {
    pub content_size: u64,
    pub filename: Option<String>,
}

let meta = BoltLoadAdapterMeta::new(1024).with_filename(Some("file.bin".into()));
```

The struct is `#[non_exhaustive]`: build it with `new` and the `with_*`
setters instead of a struct literal.

### Type Aliases

| Alias | Definition |
|-------|-----------|
| `AnyBytesStream` | `BoxStream<'static, Result<Bytes, AdapterError>>` |
| `AnyStream<'a, T>` | `BoxStream<'a, T>` |
| `AnyAdapter` | `DynBoltLoadAdapter<'static>`, usually held as `Arc<AnyAdapter>` |

## Error Model

`AdapterError` is a two-variant enum that separates retryable from
terminal failures:

```rust,ignore
pub enum AdapterError {
    Retryable { source: RetryableError },
    Unretryable { source: UnretryableError },
}
```

Both error enums are `#[non_exhaustive]`; match them with a wildcard arm.

### Unretryable Errors

| Variant | Meaning |
|---------|---------|
| `Unauthorized { status, message }` | 401 / 403 |
| `NotFound { status }` | 404 |
| `Congestion { status, retry_after }` | 429 / 503, with the server's `Retry-After` when usable |
| `Status { status }` | A status with no more specific variant |
| `RangeIgnored` | The server answered a range request with the whole resource |
| `RangeMismatch { requested, got }` | The response does not prove it carries the requested bytes |
| `RangeNotSatisfiable { total }` | 416 |
| `Internal` | Framework-level failures |
| `Cancelled` | User cancellation |
| `Io` | Wrapped `Arc<std::io::Error>` |
| `RangeStreamNotSupported` | Adapter does not support range requests |
| `Whatever` | Catch-all via `snafu` |

### Classifying HTTP statuses

An HTTP adapter should map error statuses with `classify_status`. The
task depends on this mapping: it lowers concurrency on `Congestion` and
retries only `Retryable` errors.

```rust,ignore
use bolt_load_core::adapter::classify_status;

// `retry_after`: the parsed `Retry-After` delay, for example from
// `bolt_load_utils::http::parse_retry_after`. Clamped to `MAX_RETRY_AFTER`.
return Err(classify_status(status, retry_after));
```

| Status | Error |
|--------|-------|
| 401, 403 | `Unretryable(Unauthorized)` |
| 404, 410 | `Unretryable(NotFound)` |
| 408, other 5xx | `Retryable(Status)` |
| 416 | `Unretryable(RangeNotSatisfiable)` |
| 429, 503 | `Unretryable(Congestion)` |
| other 4xx | `Unretryable(Status)` |

### Retryable Errors

| Variant | Meaning |
|---------|---------|
| `Io` | Transient I/O errors (connection drops, timeouts) |
| `Status { status }` | A status worth retrying, such as a 5xx |

I/O errors are wrapped in `Arc` to satisfy `Clone` while remaining
thread-safe.

## Dependencies

| Crate | Purpose |
|-------|---------|
| `snafu` | Structured error context |
| `futures` | `BoxStream` and stream utilities |
| `dynosaur` | Dynamic dispatch for the async adapter trait |
| `bytes` | Zero-copy byte buffers |

## License

See the repository root for license information.
