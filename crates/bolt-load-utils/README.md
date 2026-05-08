# bolt-load-utils

Shared utilities for the [bolt-load](https://github.com/aspect-build/bolt-load) download framework.

This crate provides HTTP header parsing, cross-runtime stream adapters,
and conditional telemetry macros. All functionality is feature-gated to
keep the dependency footprint minimal.

## Modules

### `telemetry` (always available)

Conditional logging macros that delegate to the [`tracing`](https://docs.rs/tracing)
crate when the `tracing` feature is enabled, or compile to no-ops otherwise.

Exports: `info!`, `warn!`, `error!`, `debug!`, `trace!`.

### `http` (feature `http`)

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

### `reader` (feature `reader`)

Cross-runtime stream adapter that bridges synchronous `std::io::Read`
implementations with async `futures::Stream`.

- **`CrossRuntimeStream`** — wraps any `Read + Send` in a
  [`blocking::Unblock`](https://docs.rs/blocking) wrapper, reading into a
  pre-allocated buffer and yielding `Result<Bytes, io::Error>` items.
  Useful for adapters that use blocking HTTP clients (e.g., ureq).

## Features

| Feature | Default | Description |
|---------|---------|-------------|
| `tracing` | No | Enable `tracing` crate delegation |
| `http` | No | HTTP header parsing (`ContentDisposition`, `ExtendedValue`) |
| `reader` | No | `CrossRuntimeStream` sync-to-async adapter |

## License

See the repository root for license information.
