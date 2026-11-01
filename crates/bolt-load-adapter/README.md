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
- HEAD response caching for range support detection

## Range Request Detection

Both adapters probe the server to determine range request support:

1. **Reqwest**: sends `Range: bytes=0-0`, checks for `Content-Range` in response
2. **Ureq**: checks `Accept-Ranges: bytes` header, falls back to a probe request

When range requests are available, bolt-load can use concurrent
multi-runner downloads for higher throughput.

## Error Mapping

Both adapters map HTTP statuses through `bolt_load_core::adapter::classify_status`,
so they classify every status the same way. A custom HTTP adapter should
call it too; see the `bolt-load-core` README.

| Status | Error |
|--------|-------|
| 401, 403 | `Unretryable(Unauthorized)` |
| 404, 410 | `Unretryable(NotFound)` |
| 408 | `Retryable(Status)` |
| 416 | `Unretryable(RangeNotSatisfiable)` |
| 429, 503 | `Unretryable(Congestion)`, carrying `Retry-After` |
| Other 4xx | `Unretryable(Status)` |
| Other 5xx | `Retryable(Status)` |
| Network errors | `Retryable(Io)` |

`Retry-After` is parsed with `bolt_load_utils::http::parse_retry_after`
(delay-seconds or an HTTP-date) and clamped to 120 s
(`MAX_RETRY_AFTER`); a value that parses as neither is dropped.

## Features

| Feature | Default | Description |
|---------|---------|-------------|
| `reqwest` | Yes | Async HTTP via reqwest 0.13 |
| `ureq2` | No | Blocking HTTP via ureq 2 |

## Dependencies

Core dependencies include `bolt-load-core` (adapter trait),
`bolt-load-utils` (HTTP header parsing, cross-runtime streams),
`url`, `bytes`, `futures`, and `async-lock`.

## License

See the repository root for license information.
