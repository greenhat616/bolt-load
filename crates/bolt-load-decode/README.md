# bolt-load-decode

Streaming content decoding for
[bolt-load](https://github.com/greenhat616/bolt-load).

A full download writes the response body as the file, so a body with a
`Content-Encoding` has to be decoded first. Whether the HTTP client
decodes depends on features that anything in the dependency graph can
turn on, so bolt-load decodes the body itself. `decode` wraps a body
stream and decodes it as the chunks arrive. It works on any
`Stream<Item = Result<Bytes, E>>`; its errors reach the stream through
`E: From<DecodeError>`.

```rust,ignore
use bolt_load_decode::{ContentCoding, DecodeOptions, decode};

// `content_encoding`: the combined `Content-Encoding` value.
let codings = ContentCoding::parse_list(content_encoding);
let body = decode(body, &codings, DecodeOptions::default().with_max_output(limit))?;
```

## Codings

| Coding | Feature | Notes |
|--------|---------|-------|
| `gzip`, `x-gzip` | `gzip` (default) | Several members decode one after another |
| `deflate` | `deflate` (default) | zlib (RFC 9110 §8.4.1.2); raw deflate when the body does not start with a zlib header |
| `br` | `br` | |
| `zstd` | `zstd` | Several frames; windows up to 8 MiB (RFC 9659 §3). Builds the libzstd C library |

A body may stack up to `MAX_LAYERS` (2) codings, such as `gzip, br`.
`decode` fails with `Unsupported` for more, for a coding whose feature
is off, and for one it does not know.

## Errors

| Error | When |
|-------|------|
| `Unsupported` | From `decode`, before reading: see above |
| `Invalid` | The body is corrupt, or ends inside its coding |
| `TrailingData` | Data follows the end of the coding and cannot start another gzip member or zstd frame |
| `TooLarge` | The decoded body would pass `max_output`; no byte past it is yielded |

An empty body decodes to an empty file. The stream ends after the first
error, and an error from the body passes through unchanged.

## Memory

- Input chunks are read in place, never copied.
- Each coding has one output buffer of `buffer_size` (default 64 KiB).
  Each item is at most that long and is handed out without a copy. Once
  the last item is dropped, the allocation is reused.
- Each poll yields at most one item, so a body that compresses well
  cannot take more memory or time per poll.
- Decoder state: 32 KiB for gzip and deflate, up to 16 MiB for br, up to
  8 MiB for zstd.

## Dependencies

| Crate | Purpose |
|-------|---------|
| `bolt-load-http` | `ContentCoding` |
| `compression-codecs` | The decoders; used internally only |
| `bytes`, `futures`, `pin-project-lite` | The stream |
| `snafu` | `DecodeError` |

## License

See the repository root for license information.
