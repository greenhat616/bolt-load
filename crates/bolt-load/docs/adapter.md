Adapter re-exports and content source abstraction.

This module re-exports the full public API from both
[`bolt_load_core::adapter`] and [`bolt_load_adapter`], providing a
single import point for all adapter-related types.

# Core Trait

[`BoltLoadAdapter`] defines the content delivery interface:

```rust,ignore
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

Custom adapters implement [`BoltLoadAdapter`] with plain `async fn`s
whose futures are `Send`. Only `retrieve_meta` and `full_stream` are
required: by default `is_range_stream_available` returns `false` and
`range_stream` fails with `RangeStreamNotSupported`. Adapters are passed to
[`Task::builder`](crate::task::Task::builder). An already
type-erased adapter, built with [`DynBoltLoadAdapter::new_arc`], is
passed to [`Task::builder_arc`](crate::task::Task::builder_arc);
a `Box<AnyAdapter>` implements the trait itself.

The erased adapter implements the trait rather than inherent methods, so
calling its methods through `AnyAdapter` needs `BoltLoadAdapter` in scope.

# Error Model

[`AdapterError`] distinguishes retryable from unretryable failures:

- **Retryable** — transient I/O errors (connection drops, timeouts).
- **Unretryable** — `NotFound` (404), `Unauthorized` (401/403),
  `ServiceUnavailable` (429/503), `RangeStreamNotSupported`, etc.

A task retries retryable failures up to its consecutive failure budget
and fails on the first unretryable one. The only exception is
`ServiceUnavailable` (429/503) while other attempts are in flight: it is
answered by lowering concurrency instead (see the task module's
*Termination* section).

# Cancellation

Requests are cancelled by dropping their futures and streams. An adapter
should not detach work that outlives a dropped future: such work is not
owned by bolt-load and cannot be awaited by [`Task::stop`](crate::task::Task::stop).

# Key Type Aliases

- [`AnyBytesStream`] — `BoxStream<'static, Result<Bytes, AdapterError>>`
- [`AnyAdapter`] — `DynBoltLoadAdapter<'static>`, shared as `Arc<AnyAdapter>`
