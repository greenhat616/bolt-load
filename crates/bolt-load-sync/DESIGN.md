# bolt-load-sync design

## Goals

- Latest-value state sync from one owner to many subscribers (task
  progress first), without a tokio dependency.
- A lock-free latest-value slot from one owner to one poller (runner
  limits), model-checked.
- Publishing must never wait for a subscriber to consume a value, fail
  because of one, or allocate per subscriber. It may wait for the value lock,
  which is only ever held for a pointer clone or swap.
- Late subscribers read the current state; the final state survives the
  sender.

## Non-goals

- Event queues where every message matters (use a channel).
- Time-based throttling. Subscribers combine `has_changed` / `changed` with
  their own timer; the crate stays timer-free and runtime-free.
- Multiple writer handles. `Sender` is not `Clone`; concurrent
  `send_replace` through a shared `&Sender` is sound but serialized.

## `watch` protocol

```
Shared<T>
├── value:   Mutex<Arc<T>>   pointer swap / clone only
├── state:   AtomicU64       version << 1 | CLOSED
└── changed: event_listener::Event
```

- `send_replace`: allocate the `Arc` outside the lock; under the lock swap
  the pointer and add `STEP` (2) to `state`; unlock; `notify(usize::MAX)`.
  The previous `Arc` is returned so its destructor runs outside the lock.
- `borrow_and_update`: under the lock read `state` and clone the `Arc`, so
  the seen version always matches the returned value.
- `changed`: check `state`; if nothing is new, register a listener with
  `listener!` (stack pinned, no allocation), check again, then wait. A
  publish between the two checks is caught by the second check; a publish
  after it notifies the registered listener. Nothing but the seen version is
  mutated, and only when the future resolves, so it is cancel safe.
- Close: `Drop for Sender` sets `CLOSED` and notifies. A receiver reports
  `Closed` only when its seen version equals the final one.
- `wait_for(f)`: `loop { v = borrow_and_update(); if f(&v) { return v } changed().await? }`.
  The current value is tested first. `borrow_and_update` marks the tested
  version as seen, so a publish after it makes `changed` return, and the
  final value is always tested before `Closed`. `f` runs outside the lock,
  so a slow or panicking predicate cannot hold or poison it.
  `wait_for_blocking` is the same loop over `changed_blocking`.
- `WatchStream` (feature `stream`): the first poll yields
  `borrow_and_update()`, even on a closed channel. Later polls check
  `state` as `changed` does; when nothing is new they register an owned
  `EventListener`, check again, and keep the listener across `Pending`. A
  notified listener is dropped and the state checked again. After `None` it
  stays ended.

### Why no wakeup is lost

`listener!` registers the listener immediately, before the second check.
In `event-listener` registration and notification serialize through its
internal lock. If a notification precedes the registration, that lock makes
the state update before it visible to the second check; otherwise the
registered listener receives the notification, even one that arrives before
`.await` / `.wait()`. Publishing and closing both update `state` before
notifying, so the argument covers both.

### Why Acquire / Release is enough

`send_replace` bumps the version before swapping the pointer, so a receiver
can see the new version while the sender still holds the lock. Any read of
the value takes the same lock and therefore cannot return the old pointer;
`borrow_and_update` reads the version and the value under that lock, so
they always match. `state` needs no `SeqCst`; `event-listener` itself uses
a `SeqCst` fence inside `notify`.

## `slot` protocol

```
Shared
├── value:  AtomicU64     latest unread value, u64::MAX when empty
├── closed: AtomicBool    set when the sender drops
└── waker:  AtomicWaker   the receiver's waker
```

- `send`: fail if the receiver is gone (`Arc::strong_count == 1`); store the
  value (`Release`), then wake.
- `try_recv`: a `Relaxed` load skips the write when the slot is empty;
  otherwise `swap(EMPTY, Acquire)` takes the value, so each is read once.
- `poll_recv`: take; else register the waker, read `closed`, take again.
  `closed` is read before the second take: the sender stores its last value
  before setting `closed`, so a receiver that sees `closed` also sees that
  value. Reading it after the take loses the last value (the
  `slot_last_value_arrives_before_closed` model catches this).
- Close: `Drop for Sender` sets `closed` (`Release`) and wakes.

A send either precedes the registration, and the second take sees it, or
follows it, and wakes the registered waker: `AtomicWaker` orders the two.
Dropping the second take loses that send (caught by the two-send models).

`AtomicWaker` is the algorithm of `futures::task::AtomicWaker` on
`src/sync.rs` primitives, so loom models it with the slot instead of
treating it as opaque. When `register` races a `wake`, it wakes the new
waker itself and yields: the caller polls again at once, and under loom a
spin without a yield never lets the `wake` finish.

The slot is specific to `u64` so the value fits one atomic. A generic slot
would need `Mutex<Option<T>>` or an allocation per send, which is what
`watch` already offers.

## Differences from `tokio::sync::watch`

| | tokio | here |
| --- | --- | --- |
| Reading | lock guard (`Ref`) | owned `Arc<T>` snapshot |
| `send_replace` returns | `T` | `Arc<T>` |
| `has_changed` | `Result<bool>`, errors once closed | `bool`; closure via `is_closed` |
| Sender | `Clone` (1.53) | one handle |
| Blocking wait | none | `changed_blocking`, `wait_for_blocking` |
| `wait_for` returns | `Ref<T>` | `Arc<T>` |
| Stream | `tokio_stream::wrappers::WatchStream` | `WatchStream`, feature `stream` (`futures-core`) |

## Decisions

| Date | Decision | Reason |
| --- | --- | --- |
| 2026-10-15 | Own `watch` on `event-listener` | `async-watch` (2020) and `postage` (2022) are unmaintained; `futures-signals` is a full FRP stack; tokio is not a dependency of the core. `event-listener` is already in the tree via `async-channel`. |
| 2026-10-15 | `std::sync::Mutex`, not `parking_lot` | Uncontended cost is equal (publish 34.8 vs 38.0 ns, read 6.8 vs 5.2 ns on macOS). Under artificial contention neither wins, and `parking_lot`'s barging starved the writer (80 ns vs 1.2 µs per publish with one spinning reader). |
| 2026-10-15 | `Mutex<Arc<T>>`, not `arc-swap` | No extra dependency; the lock is held for a pointer operation only. |
| 2026-10-15 | `listener!` instead of `Event::listen` | `listen()` boxes the listener on every wait; the boxed variant was 2-4x slower in fan-out. |
| 2026-10-24 | `slot` beside `watch`, not `watch` for runner limits | The runner polls its limits in place; `watch` has no poll API, allocates per send and registers an `event-listener` node on every pending poll. The slot moved here from `bolt-load` to get loom coverage. |
| 2026-10-24 | Own `AtomicWaker` | `futures`' and `atomic-waker`'s use std atomics, which loom cannot model. |
| 2026-10-30 | `wait_for` tests the predicate outside the lock | tokio holds the read lock while the predicate runs; here the lock only guards a pointer, and a panicking predicate must not poison the channel. |
| 2026-10-30 | `WatchStream` keeps an owned `EventListener` | A `Stream` cannot keep `listener!`'s stack-pinned node across polls. One allocation per pending wait; task status changes about four times per second. |
| 2026-10-24 | loom pinned to tokio-rs/loom#391 (`c673a8d`) | loom 0.7.2 lets an RMW read a value older than the latest store (tokio-rs/loom#254). A standalone repro (`swap` reads the initial value, a store is published, a second `swap` still reads 0) fails on 0.7.2 and passes on `c673a8d`; the two-send slot models deadlock spuriously on 0.7.2. Remove the `[patch]` once a release carries the fix. |

## Testing

```sh
cargo test -p bolt-load-sync          # with the `stream` feature, via a dev-dependency on itself
RUSTFLAGS="--cfg loom" CARGO_TARGET_DIR=target/loom \
  cargo test -p bolt-load-sync --release --test loom
```

Under `cfg(loom)`, `src/sync.rs` swaps `Arc`, `Mutex`, the atomics,
`UnsafeCell` and `yield_now` for loom's, and `event-listener` is built with
its `loom` feature; the unit tests, which use std threads, are compiled out.
The workspace `[patch.crates-io]` pins loom to a fork with the fix for
tokio-rs/loom#254 (see Decisions); it reaches `event-listener`'s loom too,
so both share one runtime.

## Measurements

From `benches/progress.rs` on the `bench/progress-reporting` branch, a
model of the owner and subscribers, not the real download path (macOS,
single run):

| Case | tokio `watch` | this `watch` |
| --- | --- | --- |
| publish, 256 ranges, stalled reader | 101 ns | 85 ns |
| 256 updates to 4 subscribers | 61 µs | 41 µs |
| 256 updates to 16 subscribers | 114 µs | 73 µs |
| fast reader beside a 50 ms reader, p99 age | 0.63 ms | 0.90 ms |

## Known limits

- `tests/loom.rs` model-checks small cases. For `slot` (one sender, up to
  two sends): wakeup by a send and by close, the last value before `None`,
  reading the last of two racing sends in order, re-registering between
  sends, and failed sends once the receiver is gone; all exhaustive. Two
  injected bugs were caught: dropping the take after registration
  (deadlock) and reading `closed` after the take (lost last value). For
  `watch` (one sender, up to two
  receivers, up to two sends): wakeup of async and blocking waiters, the
  last value before `Closed`, the version recorded by `borrow_and_update`,
  and fan-out to two parked receivers. The fan-out model has three threads
  and is bounded to two preemptions; the others are exhaustive. Two injected
  bugs were caught as deadlocks: dropping the re-check after registration in
  `changed`, and `notify(1)` instead of `notify(usize::MAX)`. `wait_for`
  and `WatchStream` each have two models: a send during registration, and
  the final send racing close (the predicate or stream must still see it).
  The racing-close models are the slowest of the suite. An injected bug,
  returning `Pending` right after registering the stream's listener without
  checking again, was caught as a deadlock. Larger runs are
  covered by the stress tests only.
- The version wraps after 2^63 publications. A receiver that has not looked
  for exactly that many publications would miss the newest value; at one
  publication per nanosecond that takes about 292 years.
- `receiver_count` is a hint and may be stale under concurrent
  subscribe/drop.
