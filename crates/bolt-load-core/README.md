# bolt-load-core

Core types and traits for the [bolt-load](https://github.com/aspect-build/bolt-load) download framework.

This crate defines the foundational abstractions that adapters and the
download engine build upon. It is intentionally minimal — no runtime,
no I/O — so that adapter implementations can depend on it without
pulling in the full framework.

## Adapter Trait

The central abstraction is [`BoltLoadAdapter`], an async trait for
streaming content delivery:

```rust,ignore
#[async_trait]
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
pub struct BoltLoadAdapterMeta {
    pub content_size: u64,
    pub filename: Option<String>,
}
```

### Type Aliases

| Alias | Definition |
|-------|-----------|
| `AnyBytesStream` | `BoxStream<'static, Result<Bytes, AdapterError>>` |
| `AnyStream<'a, T>` | `BoxStream<'a, T>` |
| `AnyAdapter` | `Box<dyn BoltLoadAdapter + Send>` |

## Error Model

`AdapterError` is a two-variant enum that separates retryable from
terminal failures:

```rust,ignore
pub enum AdapterError {
    Retryable { source: RetryableError },
    Unretryable { source: UnretryableError },
}
```

### Unretryable Errors

| Variant | Meaning |
|---------|---------|
| `Unauthorized` | 401 / 403 |
| `NotFound` | 404 |
| `ServiceUnavailable` | 429 / 503 |
| `Internal` | Framework-level failures |
| `Cancelled` | User cancellation |
| `Io` | Wrapped `Arc<std::io::Error>` |
| `RangeStreamNotSupported` | Adapter does not support range requests |
| `Whatever` | Catch-all via `snafu` |

### Retryable Errors

| Variant | Meaning |
|---------|---------|
| `Io` | Transient I/O errors (connection drops, timeouts) |

I/O errors are wrapped in `Arc` to satisfy `Clone` while remaining
thread-safe.

## Dependencies

| Crate | Purpose |
|-------|---------|
| `snafu` | Structured error context |
| `futures` | `BoxStream` and stream utilities |
| `async-trait` | Async methods in traits |
| `bytes` | Zero-copy byte buffers |

## License

See the repository root for license information.
