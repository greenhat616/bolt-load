# bolt-load-sync

Runtime-agnostic synchronization primitives for
[bolt-load](https://github.com/greenhat616/bolt-load).

bolt-load runs on tokio, smol and compio, so its coordination primitives
must not depend on any of them. This crate builds on
[`event-listener`](https://crates.io/crates/event-listener) and the
standard library only.

## `watch`

A single-producer, multi-consumer channel that keeps only the latest value,
with the semantics of `tokio::sync::watch`:

- **Latest value**: publishing replaces the value; slow receivers skip
  intermediate values instead of queueing them.
- **Late subscribers**: [`Sender::subscribe`](watch::Sender::subscribe) and
  [`Receiver::borrow`](watch::Receiver::borrow) read the current value, even
  after the sender is gone.
- **No backpressure**: [`Sender::send_replace`](watch::Sender::send_replace)
  never waits for receivers to consume a value and succeeds with none.
- **Isolation**: values are shared as `Arc<T>`; the lock is held only for a
  pointer clone or swap, never while a receiver uses the value, so a slow
  receiver cannot delay the sender or others.
- **Close**: dropping the sender wakes every receiver; each one still gets
  the last unseen value before [`Closed`](watch::Closed).
- **Any context**: `changed().await` works on any executor,
  `changed_blocking()` on plain threads (not on WebAssembly).

```rust
use bolt_load_sync::watch;

let (tx, mut rx) = watch::channel(0u64);
tx.send_replace(42);
assert!(rx.has_changed());
assert_eq!(*rx.borrow_and_update(), 42);
```

See [DESIGN.md](DESIGN.md) for the protocol and the measurements behind it.
