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
    async fn full_stream(&self) -> Result<FullResponse, AdapterError>;
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

## Validators

The HTTP adapters read the validators of the resource from a GET when
they retrieve its metadata ([`BoltLoadAdapterMeta::etag`],
[`BoltLoadAdapterMeta::last_modified`]) and send one in `If-Range` with
every range request: the strong `ETag`, or else the `Last-Modified` when
it is at least a second older than the response's `Date`. With only a
weak `ETag` none is sent, and a field that occurs more than once counts
as missing. A 200 with another validator than the one sent, or none, and
a 206 with a validator that conflicts with it, are
`UnretryableError::ResourceChanged`: the task removes what it wrote and
fails with `DownloadError::ResourceChanged`. A 206 without a validator
passes. A custom adapter that sends no `If-Range` does not notice a
changed resource.

The validator comes from the adapter's `bytes=0-0` probe. When that
probe fails or is rejected, the adapter keeps metadata without one, and
its ranges carry no `If-Range` for its whole life.

The adapters probe once and keep the result. After `ResourceChanged`,
download again with a fresh adapter: a cloned adapter or builder shares
the cached probe, so a task on it fails again against the old validator.
Restarting automatically is tracked in
[#195](https://github.com/greenhat616/bolt-load/issues/195).

`full_stream` returns the body with the `Content-Encoding` and
`Content-Type` of the response, in a [`FullResponse`]. The task decodes a
content-coded body as it writes it, so a source without codings returns
`FullResponse::new(body)`.

# Error Model

[`AdapterError`] distinguishes retryable from unretryable failures:

- **Retryable** — transient I/O errors (connection drops, timeouts),
  408 and 5xx other than 503.
- **Unretryable** — `NotFound` (404), `Unauthorized` (401/403),
  `Congestion` (429/503), `RangeStreamNotSupported`, etc.

A task retries retryable failures up to its consecutive failure budget
and fails on the first unretryable one. The only exception is
`Congestion` (429/503) while other attempts are in flight: it is
answered by lowering concurrency instead (see the task module's
*Termination* section).

# Cancellation

Requests are cancelled by dropping their futures and streams. An adapter
should not detach work that outlives a dropped future: such work is not
owned by bolt-load and cannot be awaited by [`Task::stop`](crate::task::Task::stop).

# Key Type Aliases

- [`AnyBytesStream`] — `BoxStream<'static, Result<Bytes, AdapterError>>`
- [`AnyAdapter`] — `DynBoltLoadAdapter<'static>`, shared as `Arc<AnyAdapter>`
