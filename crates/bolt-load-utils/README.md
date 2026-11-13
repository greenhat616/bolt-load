# bolt-load-utils

Shared utilities for the [bolt-load](https://github.com/aspect-build/bolt-load) download framework.

This crate provides cross-runtime stream adapters and conditional
telemetry macros. HTTP header parsing moved to `bolt-load-http`. All functionality is feature-gated to
keep the dependency footprint minimal.

## Modules

### `telemetry` (always available)

Conditional logging macros that delegate to the [`tracing`](https://docs.rs/tracing)
crate when the `tracing` feature is enabled, or compile to no-ops otherwise.

Exports: `info!`, `warn!`, `error!`, `debug!`, `trace!`.

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
| `reader` | No | `CrossRuntimeStream` sync-to-async adapter |

## License

See the repository root for license information.
