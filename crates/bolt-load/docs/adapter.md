Adapter re-exports and content source abstraction.

This module re-exports the full public API from both
[`bolt_load_core::adapter`] and [`bolt_load_adapter`], providing a
single import point for all adapter-related types.

# Core Trait

[`BoltLoadAdapter`] defines the content delivery interface:

```rust,ignore
pub trait BoltLoadAdapter: Send + Sync {
    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError>;
    async fn full_stream(&self) -> Result<FullResponse, AdapterError>;

    // Provided:
    async fn is_range_stream_available(&self) -> bool {
        false
    }
    async fn range_stream(&self, start: u64, end: u64) -> Result<AnyBytesStream, AdapterError> {
        // fails with `RangeStreamNotSupported`
    }
    async fn range_stream_if(
        &self,
        start: u64,
        end: u64,
        expected: &RangeExpectation,
    ) -> Result<AnyBytesStream, AdapterError> {
        self.range_stream(start, end).await
    }
    fn refresh(&self) {}
}
```

## Validators

The task requests every range with `range_stream_if` and a
[`RangeExpectation`]: the size and the validator of the representation
whose metadata the task retrieved ([`BoltLoadAdapterMeta::validator`]).
The task builds it once per download, from its own metadata.

The HTTP adapters send the validator in `If-Range`, and check the
response against it and against the size. A 200 with another validator,
or none, and a 206 with a validator that conflicts with it, are
`UnretryableError::ResourceChanged`; a 206 without one passes. The
validator is the strong `ETag` of a GET, or else its `Last-Modified` when
that is at least a second older than its `Date`. With only a weak `ETag`
there is none, so no `If-Range` is sent. A field that occurs more than
once counts as missing.

A custom adapter that does not override `range_stream_if` keeps working:
its ranges are requested with `range_stream`, and a resource that
changes during the download goes unnoticed.

## Refresh

The task calls `refresh` before it downloads a changed resource again
from scratch. An adapter that caches what it learned about the resource
must forget all of it: the metadata, whether ranges work, and the
validators, so that the next `retrieve_meta` and
`is_range_stream_available` ask the server again.

- A call made before the refresh may still complete with what it
  retrieved; a call made after it must not be served a result retrieved
  before it.
- Ranges do not depend on the cache: each carries the expectation of the
  task that requests it, so a refresh by one task never changes the
  ranges of another task sharing the adapter.
- An adapter that caches nothing keeps the default, which does nothing.

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
