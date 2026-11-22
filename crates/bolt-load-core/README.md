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

    /// Stream the entire content, with the headers the task decodes it by.
    async fn full_stream(&self) -> Result<FullResponse, AdapterError>;

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
    pub content_encoding: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub validator: Option<Validator>,
}

let meta = BoltLoadAdapterMeta::new(1024).with_filename(Some("file.bin".into()));
```

The struct is `#[non_exhaustive]`: build it with `new` and the `with_*`
setters instead of a struct literal.

`validator` is the validator the adapter sends in `If-Range` with every
range request: a strong entity tag (`Validator::StrongEtag`) or a
`Last-Modified` date (`Validator::LastModified`), as the server sent it.
`None` means the adapter has none it can use, and its ranges are not
conditional. Set it from the same value the range requests send, so the
metadata names what they are checked against. `Validator::matches`
compares two validators by what they name: the same kind, a strong
entity tag byte for byte, a date as a date, whatever its format. The
HTTP adapters pick the validator with `bolt-load-http`, which also
re-exports the type.

### Full responses

`full_stream` returns a `FullResponse`: the body, and the combined
`Content-Encoding` and `Content-Type` of the response. The task decodes
the body as `content_encoding` says, so a source without codings sets
neither:

```rust,ignore
let response = FullResponse::new(body)
    .with_content_encoding(content_encoding)
    .with_content_type(content_type);
```

`content_encoding` describes the body as handed out: when the HTTP
client decodes the body and drops the header, it is `None`. Like
`BoltLoadAdapterMeta`, the struct is `#[non_exhaustive]`.

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
| `RangeContentCoded { coding }` | A range response that would match, but is content-coded |
| `RangeBodyTooLong { got }`, `RangeBodyTooShort { got, received }` | A range body longer or shorter than its `Content-Range` |
| `RangeNotSatisfiable { total }` | 416 |
| `BodyUndecodable { source }` | The body cannot be decoded, by the HTTP client or by the task |
| `ContentCoded { coding }` | A full response coded in a way the task cannot decode |
| `Internal` | Framework-level failures |
| `Cancelled` | User cancellation |
| `Io` | Wrapped `Arc<std::io::Error>` |
| `RangeStreamNotSupported` | Adapter does not support range requests |
| `Whatever` | Catch-all via `snafu` |

### HTTP adapters

An HTTP adapter produces these errors through
[`bolt-load-http`](../bolt-load-http): `classify_status` for error
statuses, and `validate_range_response` and `limit_range_body` for range
responses. The task depends on the result, so a custom HTTP adapter
should call them too. For a full response it hands over the
`Content-Encoding` and `Content-Type` in `FullResponse`; the task picks
what to decode with `codings_to_decode`.

`ContentRange` stays here, since the range errors carry it; it parses a
`Content-Range` value with `str::parse`.

Set `BoltLoadAdapterMeta::with_content_encoding` from the metadata
responses. A coded size counts encoded bytes, so the task then skips
ranges and downloads until the stream ends.

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
| `httpdate` | Comparing `Last-Modified` validators as dates |

## License

See the repository root for license information.
