//! A single-producer, multi-consumer channel that only keeps the latest value.
//!
//! The semantics follow `tokio::sync::watch` without depending on tokio: any
//! number of [`Receiver`]s observe the newest value published by one
//! [`Sender`], intermediate values may be skipped, and a receiver created after
//! a value was published still reads it. Waiting works from any executor and
//! from plain threads ([`Receiver::changed_blocking`]).
//!
//! Values are stored as [`Arc<T>`], so reading clones a pointer instead of the
//! value. The channel lock is only held for that pointer clone or swap, never
//! while a reader uses the value: a slow reader does not delay the sender or
//! other readers.
//!
//! # Example
//!
//! ```
//! use bolt_load_sync::watch;
//!
//! let (tx, mut rx) = watch::channel(0u64);
//! let reader = std::thread::spawn(move || {
//!     let mut last = 0;
//!     while rx.changed_blocking().is_ok() {
//!         last = *rx.borrow_and_update();
//!     }
//!     last
//! });
//! for downloaded in 1..=100 {
//!     tx.send_replace(downloaded);
//! }
//! drop(tx);
//! assert_eq!(reader.join().unwrap(), 100);
//! ```

use std::{
    error::Error,
    fmt,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

use event_listener::{Event, Listener, listener};

/// Set in [`Shared::state`] once the sender is dropped.
const CLOSED: u64 = 1;
/// The version increment of [`Shared::state`]; bit 0 is [`CLOSED`].
///
/// The version wraps after 2^63 publications; a receiver that has not looked
/// for exactly that many would mistake the newest value for the one it saw.
const STEP: u64 = 2;

struct Shared<T> {
    value: Mutex<Arc<T>>,
    /// The version of `value` in steps of [`STEP`], bumped under the `value`
    /// lock so that a reader sees a version and its value together.
    state: AtomicU64,
    /// Notified after every version bump and on close.
    changed: Event,
}

impl<T> Shared<T> {
    fn slot(&self) -> MutexGuard<'_, Arc<T>> {
        // The lock only guards a pointer swap or clone, which cannot be left
        // half done, so a poisoned lock still holds a valid value.
        self.value.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn version(&self) -> u64 {
        self.state.load(Ordering::Acquire) & !CLOSED
    }

    fn is_closed(&self) -> bool {
        self.state.load(Ordering::Acquire) & CLOSED != 0
    }
}

/// Create a channel holding `initial`, which the returned receiver has
/// already seen.
pub fn channel<T>(initial: T) -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        value: Mutex::new(Arc::new(initial)),
        state: AtomicU64::new(0),
        changed: Event::new(),
    });
    let rx = Receiver {
        shared: shared.clone(),
        seen: 0,
    };
    (Sender { shared }, rx)
}

/// The error returned once the sender is dropped and the receiver has seen
/// the last value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

impl fmt::Display for Closed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("watch sender dropped")
    }
}

impl Error for Closed {}

/// The publishing half; dropping it closes the channel.
///
/// It is not `Clone`, so one handle publishes. Publishing through a shared
/// reference from several threads is still sound: the calls are serialized.
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Sender<T> {
    /// Publish `value` and wake every waiting receiver, returning the previous
    /// value.
    ///
    /// Never waits for receivers to consume a value and succeeds with none; it
    /// may briefly wait for the lock while a receiver clones the pointer. The
    /// previous value is returned, not dropped, so its destructor runs outside
    /// the lock.
    pub fn send_replace(&self, value: T) -> Arc<T> {
        let value = Arc::new(value);
        let previous = {
            let mut slot = self.shared.slot();
            self.shared.state.fetch_add(STEP, Ordering::Release);
            std::mem::replace(&mut *slot, value)
        };
        self.shared.changed.notify(usize::MAX);
        previous
    }

    /// The current value.
    pub fn borrow(&self) -> Arc<T> {
        self.shared.slot().clone()
    }

    /// A new receiver that has already seen the current value.
    pub fn subscribe(&self) -> Receiver<T> {
        Receiver {
            seen: self.shared.version(),
            shared: self.shared.clone(),
        }
    }

    /// How many receivers exist; a hint, since receivers may be created or
    /// dropped concurrently. Useful to skip building a value nobody reads.
    pub fn receiver_count(&self) -> usize {
        Arc::strong_count(&self.shared) - 1
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.shared.state.fetch_or(CLOSED, Ordering::Release);
        self.shared.changed.notify(usize::MAX);
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender")
            .field("version", &self.shared.version())
            .field("receivers", &self.receiver_count())
            .finish()
    }
}

/// The observing half; clone it to observe independently.
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
    /// The last version this receiver has seen.
    seen: u64,
}

impl<T> Receiver<T> {
    /// The current value, without marking it as seen.
    pub fn borrow(&self) -> Arc<T> {
        self.shared.slot().clone()
    }

    /// The current value, marking it as seen.
    pub fn borrow_and_update(&mut self) -> Arc<T> {
        let slot = self.shared.slot();
        self.seen = self.shared.version();
        slot.clone()
    }

    /// Whether a value newer than the last seen one was published.
    pub fn has_changed(&self) -> bool {
        self.shared.version() != self.seen
    }

    /// Mark the current value as seen without reading it.
    pub fn mark_unchanged(&mut self) {
        self.seen = self.shared.version();
    }

    /// Whether the sender is dropped; unseen values may remain.
    pub fn is_closed(&self) -> bool {
        self.shared.is_closed()
    }

    /// Wait until a value newer than the last seen one is published, then mark
    /// the newest value as seen.
    ///
    /// Returns [`Closed`] once the sender is dropped and no unseen value is
    /// left. Cancel safe: dropping the future loses no update.
    pub async fn changed(&mut self) -> Result<(), Closed> {
        let Self { shared, seen } = self;
        loop {
            if let Some(result) = poll_state(shared, seen) {
                return result;
            }
            listener!(shared.changed => listener);
            // Re-check after registering, so a publish in between is not missed.
            if let Some(result) = poll_state(shared, seen) {
                return result;
            }
            listener.await;
        }
    }

    /// Blocking form of [`Self::changed`], for threads outside any executor.
    ///
    /// Not available on WebAssembly, where threads cannot block.
    #[cfg(not(target_family = "wasm"))]
    pub fn changed_blocking(&mut self) -> Result<(), Closed> {
        let Self { shared, seen } = self;
        loop {
            if let Some(result) = poll_state(shared, seen) {
                return result;
            }
            listener!(shared.changed => listener);
            if let Some(result) = poll_state(shared, seen) {
                return result;
            }
            listener.wait();
        }
    }
}

/// `Some` when [`Receiver::changed`] can return without waiting; marks the
/// newest version as seen when it is new.
fn poll_state<T>(shared: &Shared<T>, seen: &mut u64) -> Option<Result<(), Closed>> {
    let state = shared.state.load(Ordering::Acquire);
    let version = state & !CLOSED;
    if version != *seen {
        *seen = version;
        Some(Ok(()))
    } else if state & CLOSED != 0 {
        Some(Err(Closed))
    } else {
        None
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
            seen: self.seen,
        }
    }
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver")
            .field("seen", &self.seen)
            .field("version", &self.shared.version())
            .field("closed", &self.shared.is_closed())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        pin::pin,
        sync::{atomic::AtomicUsize, mpsc},
        task::{Context, Poll, Wake, Waker},
        thread,
        time::Duration,
    };

    use futures::{FutureExt, executor::block_on};
    use pretty_assertions::assert_eq;

    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    /// A waker counting its wakes.
    #[derive(Default)]
    struct CountingWaker(AtomicUsize);

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counting_waker() -> (Arc<CountingWaker>, Waker) {
        let counter = Arc::new(CountingWaker::default());
        (counter.clone(), Waker::from(counter))
    }

    fn wakes(counter: &CountingWaker) -> usize {
        counter.0.load(Ordering::SeqCst)
    }

    /// Run `f` on a thread, failing instead of hanging if it never returns.
    fn within_timeout<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || tx.send(f()).unwrap());
        rx.recv_timeout(WAIT).expect("timed out")
    }

    #[test]
    fn initial_value_counts_as_seen() {
        let (_tx, mut rx) = channel(1);
        assert_eq!(*rx.borrow(), 1);
        assert!(!rx.has_changed());
        assert_eq!(rx.changed().now_or_never(), None);
    }

    #[test]
    fn sends_coalesce_into_the_latest_value() {
        let (tx, mut rx) = channel(0);
        for v in 1..=3 {
            tx.send_replace(v);
        }
        assert!(rx.has_changed());
        assert_eq!(rx.changed().now_or_never(), Some(Ok(())));
        assert_eq!(*rx.borrow_and_update(), 3);
        assert!(!rx.has_changed());
        assert_eq!(rx.changed().now_or_never(), None);
    }

    #[test]
    fn changed_marks_the_newest_value_as_seen() {
        let (tx, mut rx) = channel(0);
        tx.send_replace(1);
        block_on(rx.changed()).unwrap();
        assert!(!rx.has_changed());
        assert_eq!(*rx.borrow(), 1);
    }

    #[test]
    fn send_replace_returns_the_previous_value() {
        let (tx, rx) = channel(String::from("a"));
        assert_eq!(*tx.send_replace("b".into()), "a");
        assert_eq!(*tx.borrow(), "b");
        assert_eq!(*rx.borrow(), "b");
    }

    #[test]
    fn mark_unchanged_skips_the_current_value() {
        let (tx, mut rx) = channel(0);
        tx.send_replace(1);
        rx.mark_unchanged();
        assert!(!rx.has_changed());
    }

    #[test]
    fn a_pending_waiter_is_woken_by_a_send() {
        let (tx, mut rx) = channel(0);
        let (counter, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        {
            let mut changed = pin!(rx.changed());
            assert_eq!(changed.as_mut().poll(&mut cx), Poll::Pending);
            tx.send_replace(7);
            assert_eq!(wakes(&counter), 1);
            assert_eq!(changed.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
        }
        assert_eq!(*rx.borrow(), 7);
    }

    #[test]
    fn every_pending_receiver_is_woken() {
        let (tx, rx) = channel(0);
        let mut receivers = [rx.clone(), rx.clone(), rx];
        let (counter, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        let mut futures: Vec<_> = receivers
            .iter_mut()
            .map(|rx| Box::pin(rx.changed()))
            .collect();
        for f in &mut futures {
            assert_eq!(f.as_mut().poll(&mut cx), Poll::Pending);
        }
        tx.send_replace(1);
        assert_eq!(wakes(&counter), 3);
        for f in &mut futures {
            assert_eq!(f.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
        }
    }

    #[test]
    fn a_future_dropped_after_its_wake_loses_no_update() {
        let (tx, mut rx) = channel(0);
        let (counter, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        {
            let mut changed = pin!(rx.changed());
            assert_eq!(changed.as_mut().poll(&mut cx), Poll::Pending);
            tx.send_replace(1);
            assert_eq!(wakes(&counter), 1);
        }
        assert!(rx.has_changed());
        assert_eq!(rx.changed().now_or_never(), Some(Ok(())));
    }

    #[test]
    fn unseen_values_are_delivered_before_closed() {
        let (tx, mut rx) = channel(0);
        tx.send_replace(5);
        drop(tx);
        assert!(rx.is_closed());
        assert_eq!(block_on(rx.changed()), Ok(()));
        assert_eq!(*rx.borrow_and_update(), 5);
        assert_eq!(block_on(rx.changed()), Err(Closed));
        assert_eq!(rx.changed_blocking(), Err(Closed));
    }

    #[test]
    fn dropping_the_sender_wakes_a_pending_waiter() {
        let (tx, mut rx) = channel(0);
        let (counter, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        let mut changed = pin!(rx.changed());
        assert_eq!(changed.as_mut().poll(&mut cx), Poll::Pending);
        drop(tx);
        assert_eq!(wakes(&counter), 1);
        assert_eq!(changed.as_mut().poll(&mut cx), Poll::Ready(Err(Closed)));
    }

    /// The sleep only makes it likely that the waiter is parked; the test
    /// passes either way.
    #[test]
    fn dropping_the_sender_wakes_a_blocking_waiter() {
        let (tx, mut rx) = channel(0);
        let waiter = thread::spawn(move || rx.changed_blocking());
        thread::sleep(Duration::from_millis(20));
        drop(tx);
        let result = within_timeout(move || waiter.join().unwrap());
        assert_eq!(result, Err(Closed));
    }

    #[test]
    fn a_late_subscriber_reads_the_final_value() {
        let (tx, rx) = channel(0);
        drop(rx);
        tx.send_replace(9);
        let late = tx.subscribe();
        assert_eq!(*late.borrow(), 9);
        assert!(!late.has_changed());
    }

    #[test]
    fn receivers_track_their_own_progress() {
        let (tx, mut first) = channel(0);
        let mut second = first.clone();
        tx.send_replace(1);
        first.borrow_and_update();
        assert!(!first.has_changed());
        assert!(second.has_changed());
        assert_eq!(*second.borrow_and_update(), 1);
    }

    #[test]
    fn sending_without_receivers_succeeds() {
        let (tx, rx) = channel(0);
        drop(rx);
        assert_eq!(tx.receiver_count(), 0);
        assert_eq!(*tx.send_replace(1), 0);
        assert_eq!(*tx.borrow(), 1);
    }

    #[test]
    fn receiver_count_follows_receivers() {
        let (tx, rx) = channel(0);
        let other = rx.clone();
        let late = tx.subscribe();
        assert_eq!(tx.receiver_count(), 3);
        drop((rx, other, late));
        assert_eq!(tx.receiver_count(), 0);
    }

    /// The sleep only makes it likely that the waiter is parked; the test
    /// passes either way.
    #[test]
    fn a_blocking_waiter_is_woken_by_a_send() {
        let (tx, mut rx) = channel(0);
        let waiter = thread::spawn(move || rx.changed_blocking().map(|()| *rx.borrow()));
        thread::sleep(Duration::from_millis(20));
        tx.send_replace(3);
        let result = within_timeout(move || waiter.join().unwrap());
        assert_eq!(result, Ok(3));
    }

    #[test]
    fn a_dropped_changed_future_loses_no_update() {
        let (tx, mut rx) = channel(0);
        assert_eq!(rx.changed().now_or_never(), None);
        tx.send_replace(1);
        assert_eq!(rx.changed().now_or_never(), Some(Ok(())));
    }

    /// Readers racing a fast writer must only see non-decreasing values and
    /// always reach the final one: a lost wakeup would hang or end early.
    #[test]
    fn concurrent_readers_reach_the_final_value() {
        const LAST: u64 = 50_000;
        const READERS: usize = 4;
        for _ in 0..4 {
            let (tx, rx) = channel(0u64);
            let readers: Vec<_> = (0..READERS)
                .map(|i| {
                    let mut rx = rx.clone();
                    thread::spawn(move || {
                        let mut last = 0;
                        let wait = |rx: &mut Receiver<u64>| {
                            if i % 2 == 0 {
                                block_on(rx.changed())
                            } else {
                                rx.changed_blocking()
                            }
                        };
                        while wait(&mut rx).is_ok() {
                            let v = *rx.borrow_and_update();
                            assert!(v >= last, "went back from {last} to {v}");
                            last = v;
                        }
                        last
                    })
                })
                .collect();
            drop(rx);
            for v in 1..=LAST {
                tx.send_replace(v);
            }
            drop(tx);
            for reader in readers {
                let last = within_timeout(move || reader.join().unwrap());
                assert_eq!(last, LAST);
            }
        }
    }

    #[test]
    fn handles_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Sender<Vec<u8>>>();
        assert_send_sync::<Receiver<Vec<u8>>>();
        fn assert_send<T: Send>(_: &T) {}
        let (_tx, mut rx) = channel(0);
        assert_send(&rx.changed());
    }
}
