# bolt-load-adapter

HTTP adapter implementations for the [bolt-load](https://github.com/aspect-build/bolt-load) download framework.

This crate provides concrete [`BoltLoadAdapter`] implementations for
popular HTTP client libraries, enabling bolt-load to download content
over HTTP/HTTPS with features like range requests, streaming, and
request customization.

## Available Adapters

### Reqwest (default)

Async HTTP adapter built on [reqwest](https://docs.rs/reqwest).

```rust,ignore
use reqwest::Client;
use bolt_load_adapter::reqwest::IntoReqwestAdapter;

let client = Client::new();
let adapter = client.into_reqwest_adapter((reqwest::Method::GET, url));
```

**Features:**
- Async/await streaming via `reqwest::Response::bytes_stream()`
- Lazy metadata initialization — a single `Range: bytes=0-0` probe
  detects range support and caches content size
- Request interceptors via `before_request()` callback
- Automatic error classification (404 → `NotFound`, 429 → `Congestion`, etc.)

### Ureq (optional, feature `ureq2`)

Blocking HTTP adapter built on [ureq](https://docs.rs/ureq/2).

```rust,ignore
use bolt_load_adapter::ureq2::IntoUreqAdapter;

let agent = ureq::agent();
let adapter = agent.into_ureq_adapter(("GET", url));
```

**Features:**
- Blocking I/O offloaded to `blocking` thread pool for async compatibility
- Request interceptors via `before_request()` callback
- Custom request execution via `call()` callback (useful for mocking)
- One cached HEAD response for the meta, range detection and the total
  that range responses must name

**Known limitation:** a request, or a body read, that is already running
on the `blocking` thread pool is not stopped when its future or stream
is dropped. The thread keeps the connection until the call returns, so
after an idle timeout or a cancellation the task may open its next
connection while the old one is still open, briefly exceeding
`max_connections` (forever, if the server stalls; ureq has no timeouts
by default). `AgentBuilder::timeout_read` bounds only a single stalled
socket read: a server that trickles bytes, such as chunk extensions, keeps
the read alive, and blocked writes are not covered. The broader bound is
`AgentBuilder::timeout`, a deadline for the whole request, including DNS
resolution, connecting, redirects and reading the body; it must exceed
the longest range transfer. As ureq notes, slow DNS resolution may still
exceed it, because the DNS request cannot be interrupted.

## Range Request Detection

Both adapters probe the server to determine range request support:

1. **Reqwest**: sends `Range: bytes=0-0`; range support needs a 206 whose
   `Content-Range` is `bytes 0-0/…`. Otherwise `Accept-Ranges: bytes` on
   a HEAD response counts.
2. **Ureq**: checks `Accept-Ranges: bytes` on a HEAD response, falls back
   to the same `bytes=0-0` probe.

`Accept-Ranges` is only a hint, so every `range_stream` response is
checked with `bolt_load_http::validate_range_response` before
its body is read: a 200 is `RangeIgnored`, a content-encoded 206 is
`RangeContentCoded`, and a 206 whose `Content-Range` does not start at
the requested offset, runs past the requested end, names another total,
or comes multipart is `RangeMismatch`. Repeated header lines are
combined first, so a second `Content-Range` or `Content-Encoding` line
is seen.

Every request, HEAD and full downloads included, sends
`Accept-Encoding: identity`: without the header a server may encode the
body (RFC 9110 §12.5.3), and the file would hold the encoded bytes.
`before_request` can still replace it.

Range requests carry `If-Range` with the validator of the metadata,
`BoltLoadAdapterMeta::validator`, and every range response is checked
against that cached value, whatever was sent. A `before_request` hook
(or a ureq `call` hook, or default headers of the reqwest client) must
not replace that `If-Range`, nor add one when the metadata has none:
a range of another version could then pass as this one, or a range of
this one fail as `ResourceChanged`.

A client that decodes bodies removes `Content-Encoding` before the check
can see it. The body is therefore held to its `Content-Range` length: it
fails with `RangeBodyTooLong` past it, and with `RangeBodyTooShort` if it
ends cleanly before.

The task answers `RangeIgnored`, `RangeContentCoded`, these two errors
and `BodyUndecodable` by starting over on one connection, with the size
unknown. A full download hands the task the `Content-Encoding` and
`Content-Type` of the response, and the task decodes the body with
`bolt-load-decode`, unless its `Content-Type` is the coding's own format
(a `.tar.gz` served as `Content-Encoding: gzip`). The metadata records a
coding it sees in `content_encoding`, and the task then skips ranges.

A client that decodes cannot make a download write the wrong bytes: its
decoded ranges fail the length check, and over one connection its
decoded body is the file. It still costs the ranges, and it also decodes
a `.tar.gz` served as `Content-Encoding: gzip`. So the adapters keep
the clients from decoding where they can:

- **Ureq** depends on ureq 2 without its default `gzip` feature. Another
  crate in your build can still enable it.
- **Reqwest** uses the `Client` you pass in. Build it with `no_gzip()`,
  `no_brotli()`, `no_zstd()` and `no_deflate()` if it may enable
  decoding; these exist even without the features.

When range requests are available, bolt-load can use concurrent
multi-runner downloads for higher throughput.

## Error Mapping

Both adapters map HTTP statuses through `bolt_load_http::classify_status`,
so they classify every status the same way. A custom HTTP adapter should
call it too; see the `bolt-load-http` README.

| Status | Error |
|--------|-------|
| 401, 403 | `Unretryable(Unauthorized)` |
| 404, 410 | `Unretryable(NotFound)` |
| 408 | `Retryable(Status)` |
| 416 | `Unretryable(RangeNotSatisfiable)`, carrying the total from `bytes */total` on range requests |
| 429, 503 | `Unretryable(Congestion)`, carrying `Retry-After` |
| Other 4xx | `Unretryable(Status)` |
| Other 5xx | `Retryable(Status)` |
| Network errors, including a body cut off mid-stream | `Retryable(Io)` |
| A body that cannot be decoded, such as invalid gzip from a decoding reqwest `Client`, or `InvalidData` on ureq2 | `Unretryable(BodyUndecodable)` |
| ureq2 only: a body read error of another kind, such as `InvalidInput` | `Unretryable(Io)` |

`Retry-After` is parsed with `bolt_load_http::header::parse_retry_after`
(delay-seconds or an HTTP-date) and clamped to 120 s
(`MAX_RETRY_AFTER`); a value that parses as neither is dropped.

## Features

| Feature | Default | Description |
|---------|---------|-------------|
| `reqwest` | Yes | Async HTTP via reqwest 0.13 |
| `ureq2` | No | Blocking HTTP via ureq 2 |

## Dependencies

Core dependencies include `bolt-load-core` (adapter trait),
`bolt-load-http` (status, range and coding checks, header parsing),
`bolt-load-utils` (cross-runtime streams, telemetry),
`url`, `bytes`, `futures`, and `async-lock`.

## License

See the repository root for license information.
