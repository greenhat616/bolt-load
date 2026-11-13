# bolt-load-http

HTTP helpers for [bolt-load](https://github.com/greenhat616/bolt-load)
adapters.

The task writes what an adapter returns, so an HTTP adapter has to
classify error statuses the way the task expects, prove that a range
response carries the requested bytes, and keep content codings out of
the file. The functions here do that on plain header values (`&str`)
and status codes (`u16`), so they work with any HTTP client. The error
types they return are in `bolt-load-core`.

## Classifying HTTP statuses

An HTTP adapter should map error statuses with `classify_status`. The
task depends on this mapping: it lowers concurrency on `Congestion` and
retries only `Retryable` errors.

```rust,ignore
use bolt_load_http::classify_status;

// `retry_after`: the parsed `Retry-After` delay, for example from
// `bolt_load_http::header::parse_retry_after`. Clamped to `MAX_RETRY_AFTER`.
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

## Validating range responses

`range_stream` must yield exactly the requested bytes, since the task
writes them at the requested offset. An HTTP adapter should check each
range response with `validate_range_response` before reading the body.
It passes a 206 whose `Content-Range` starts at the requested offset
and ends before the requested end, returns that range, and classifies
everything else. Pass each header as its combined value: repeated lines
joined with `", "`.

```rust,ignore
use bolt_load_http::{RangeHeaders, limit_range_body, validate_range_response};

let headers = RangeHeaders::default()
    .with_content_range(content_range)
    .with_content_type(content_type)
    .with_content_encoding(content_encoding)
    .with_retry_after(retry_after);
// `expected_total`: the content size from the meta, if known.
let got = validate_range_response(status, &headers, start..end, expected_total)?;
Ok(limit_range_body(body, got))
```

| Response | Error |
|----------|-------|
| 206 that would pass but is content-encoded | `Unretryable(RangeContentCoded)` |
| 206 with other offsets, another total, or multipart | `Unretryable(RangeMismatch)` |
| 200 | `Unretryable(RangeIgnored)` |
| 416 | `Unretryable(RangeNotSatisfiable)`, with the total from `bytes */total` |
| other 4xx, 5xx | `classify_status` |

A 206 that ends before the requested end is valid; the stream ends
short and the task requests the rest again.

An HTTP client that decodes a content-coded body also drops
`Content-Encoding`, so the headers look plain. `limit_range_body` holds
the body to its `Content-Range` length:

- past it, the stream fails with `Unretryable(RangeBodyTooLong)` before
  the extra bytes reach the task;
- if it ends cleanly before, the stream fails with
  `Unretryable(RangeBodyTooShort)`. Decoded data that does not compress
  ends this way.

The task answers `RangeIgnored`, `RangeContentCoded`, the two body
length errors and `BodyUndecodable` by downloading over one connection
instead; any other `RangeMismatch` fails the task. So report a body your
client cannot decode as `BodyUndecodable`, not as `Io`.

## Validators

Every range request should carry `If-Range`, so that a server holding
another version of the resource answers with it rather than with a part
of it. `if_range_validator` picks the validator from the `ETag`,
`Last-Modified` and `Date` of a GET: the strong entity tag, else a date
at least a second older than `Date`, none with a weak entity tag.
Pass the one sent to `RangeHeaders::with_if_range`, with the response's
own `ETag` and `Last-Modified`, and `validate_range_response` reports a
changed resource as `Unretryable(ResourceChanged)`.

Report the same value in `BoltLoadAdapterMeta::with_validator`. The
`Validator` type lives in `bolt-load-core` for that, and is re-exported
here, so `bolt_load_http::Validator` still names it.

## Content codings

The task writes the body of a full response as the file. An HTTP
adapter passes the combined `Content-Encoding` and `Content-Type` of the
response in `FullResponse`, and the task decodes the codings that
`codings_to_decode` lists. The list leaves out the only coding of a
response whose `Content-Type` is that coding's format, such as
`application/gzip` with `Content-Encoding: gzip`: a server that marks a
`.tar.gz` file this way means the archive.

```rust,ignore
use bolt_load_http::{ContentCoding, codings_to_decode};

assert_eq!(
    codings_to_decode(Some("gzip"), Some("text/plain")),
    [ContentCoding::Gzip],
);
assert_eq!(codings_to_decode(Some("gzip"), Some("application/gzip")), []);
```

Also set `BoltLoadAdapterMeta::with_content_encoding` from the metadata
responses; see the `bolt-load-core` README.

`ContentCoding::parse_list` parses a combined `Content-Encoding` value
into the codings in the order applied. Names match case-insensitively,
`x-gzip` is `Gzip`, `identity` is skipped, and a coding this crate does
not know is `Other` with its lowercased name.

```rust,ignore
use bolt_load_http::ContentCoding;

assert_eq!(
    ContentCoding::parse_list("identity, GZIP, br"),
    [ContentCoding::Gzip, ContentCoding::Brotli],
);
```

## Header parsing (`header`)

HTTP header parsing utilities adapted from actix-web, implementing:

- **`ContentDisposition`** — RFC 2183 / 6266 / 7578 `Content-Disposition`
  header parser and formatter.
  - Extracts `filename` and `filename*` (RFC 5987 extended values with
    charset and language tag).
  - Supports `inline`, `attachment`, `form-data`, and extension types.
  - Implements the `headers::Header` trait for typed header decoding.

- **`ExtendedValue`** — RFC 5987 extended parameter value parser
  (`charset'language'percent-encoded-value`).
  - Handles UTF-8, ISO-8859-1, and arbitrary charsets.
  - Percent-decodes the value portion.

- **`parse_retry_after`** — RFC 9110 `Retry-After` parser.
  - Accepts delay-seconds and HTTP-dates; a past date is no delay.
  - Leaves the upper bound to the caller.

## Dependencies

| Crate | Purpose |
|-------|---------|
| `bolt-load-core` | Error types and `ContentRange` |
| `bytes`, `futures` | The range body limit |
| `http`, `headers` | The `Header` trait for `ContentDisposition` |
| `regex`, `language-tags`, `percent-encoding` | `Content-Disposition` and extended values |
| `httpdate` | HTTP-dates in `Retry-After` and `Last-Modified` |

## License

See the repository root for license information.
