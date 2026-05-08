Adapter re-exports and content source abstraction.

This module re-exports the full public API from both
[`bolt_load_core::adapter`] and [`bolt_load_adapter`], providing a
single import point for all adapter-related types.

# Core Trait

[`BoltLoadAdapter`] defines the content delivery interface:

```rust,ignore
#[async_trait]
pub trait BoltLoadAdapter: Send + Sync {
    async fn is_range_stream_available(&self) -> bool;
    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError>;
    async fn full_stream(&self) -> Result<AnyBytesStream, AdapterError>;
    async fn range_stream(&self, start: u64, end: u64) -> Result<AnyBytesStream, AdapterError>;
}
```

# Available Adapters

| Adapter | Feature | Crate |
|---------|---------|-------|
| [`ReqwestAdapter`] | `reqwest` | `bolt-load-adapter` |
| [`UreqAdapter`] | `ureq2` | `bolt-load-adapter` |

Custom adapters can be implemented by providing a
`Box<dyn BoltLoadAdapter + Send>` ([`AnyAdapter`]).

# Error Model

[`AdapterError`] distinguishes retryable from unretryable failures:

- **Retryable** — transient I/O errors (connection drops, timeouts).
- **Unretryable** — `NotFound` (404), `Unauthorized` (401/403),
  `ServiceUnavailable` (429/503), `RangeStreamNotSupported`, etc.

# Key Type Aliases

- [`AnyBytesStream`] — `BoxStream<'static, Result<Bytes, AdapterError>>`
- [`AnyAdapter`] — `Box<dyn BoltLoadAdapter + Send>`
