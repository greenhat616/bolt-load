# bolt-load-sync design

## Goals

- Latest-value state sync from one owner to many subscribers (task
  progress first), without a tokio dependency.
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

## Differences from `tokio::sync::watch`

| | tokio | here |
| --- | --- | --- |
| Reading | lock guard (`Ref`) | owned `Arc<T>` snapshot |
| `send_replace` returns | `T` | `Arc<T>` |
| `has_changed` | `Result<bool>`, errors once closed | `bool`; closure via `is_closed` |
| Sender | `Clone` (1.53) | one handle |
| Blocking wait | none | `changed_blocking` |

## Decisions

| Date | Decision | Reason |
| --- | --- | --- |
| 2026-10-15 | Own `watch` on `event-listener` | `async-watch` (2020) and `postage` (2022) are unmaintained; `futures-signals` is a full FRP stack; tokio is not a dependency of the core. `event-listener` is already in the tree via `async-channel`. |
| 2026-10-15 | `std::sync::Mutex`, not `parking_lot` | Uncontended cost is equal (publish 34.8 vs 38.0 ns, read 6.8 vs 5.2 ns on macOS). Under artificial contention neither wins, and `parking_lot`'s barging starved the writer (80 ns vs 1.2 µs per publish with one spinning reader). |
| 2026-10-15 | `Mutex<Arc<T>>`, not `arc-swap` | No extra dependency; the lock is held for a pointer operation only. |
| 2026-10-15 | `listener!` instead of `Event::listen` | `listen()` boxes the listener on every wait; the boxed variant was 2-4x slower in fan-out. |

## Testing

```sh
cargo test -p bolt-load-sync
RUSTFLAGS="--cfg loom" CARGO_TARGET_DIR=target/loom \
  cargo test -p bolt-load-sync --release --test loom
```

Under `cfg(loom)`, `src/sync.rs` swaps `Arc`, `Mutex` and `AtomicU64` for
loom's, and `event-listener` is built with its `loom` feature; the unit tests,
which use std threads, are compiled out.

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

- `tests/loom.rs` model-checks small cases (one sender, up to two
  receivers, up to two sends): wakeup of async and blocking waiters, the
  last value before `Closed`, the version recorded by `borrow_and_update`,
  and fan-out to two parked receivers. The fan-out model has three threads
  and is bounded to two preemptions; the others are exhaustive. Two injected
  bugs were caught as deadlocks: dropping the re-check after registration in
  `changed`, and `notify(1)` instead of `notify(usize::MAX)`. Larger runs are
  covered by the stress test only.
- The version wraps after 2^63 publications. A receiver that has not looked
  for exactly that many publications would miss the newest value; at one
  publication per nanosecond that takes about 292 years.
- `receiver_count` is a hint and may be stale under concurrent
  subscribe/drop.
