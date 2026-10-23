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

## `slot`

A single-producer, single-consumer slot that keeps only the latest `u64`,
for a value the receiver needs only in its newest form (bolt-load sends each
download runner its size limit through one):

- **Latest value, read once**: a send replaces an unread value; the receiver
  takes it, leaving the slot empty.
- **Lock-free, no allocation**: sending is an atomic store and a wake;
  polling an empty slot only registers the waker.
- **Poll-based**: [`Receiver::poll_recv`](slot::Receiver::poll_recv) fits a
  hand-written `poll` over several event sources; `recv().await` wraps it.
- **Close**: dropping the sender wakes the receiver, which still takes the
  last unread value before `None`. A send fails once the receiver is gone.

`u64::MAX` marks the empty slot and cannot be sent.

```rust
use bolt_load_sync::slot;

let (tx, mut rx) = slot::channel();
assert!(tx.send(200));
assert!(tx.send(100));
assert_eq!(rx.try_recv(), Some(100));
assert_eq!(rx.try_recv(), None);
```

See [DESIGN.md](DESIGN.md) for the protocols and the measurements behind them.
