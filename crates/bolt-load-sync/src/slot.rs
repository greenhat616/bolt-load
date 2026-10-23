//! A single-producer, single-consumer slot that only keeps the latest `u64`.
//!
//! A newer value replaces one the receiver has not read yet: however many
//! values are sent before the receiver polls, it reads only the last, and
//! each value is read at most once. Dropping the sender closes the slot once
//! its last value is read.
//!
//! Unlike [`watch`](crate::watch), the slot is lock-free and never allocates
//! after [`channel`]: sending is an atomic store and a wake, and polling an
//! empty slot only registers the waker. [`Receiver::poll_recv`] fits a
//! hand-written `poll` that drives several event sources in place.
//!
//! `u64::MAX` marks the empty slot, so it cannot be sent.
//!
//! # Example
//!
//! ```
//! use bolt_load_sync::slot;
//!
//! let (tx, mut rx) = slot::channel();
//! for limit in [300, 200, 100] {
//!     assert!(tx.send(limit));
//! }
//! assert_eq!(rx.try_recv(), Some(100));
//! assert_eq!(rx.try_recv(), None);
//! drop(tx);
//! assert_eq!(futures::executor::block_on(rx.recv()), None);
//! ```

use std::{
    fmt,
    future::poll_fn,
    task::{Context, Poll},
};

use crate::{
    atomic_waker::AtomicWaker,
    sync::{Arc, AtomicBool, AtomicU64, Ordering},
};

/// The slot holds no value.
const EMPTY: u64 = u64::MAX;

struct Shared {
    value: AtomicU64,
    closed: AtomicBool,
    waker: AtomicWaker,
}

/// Create an empty slot, see the [module docs](self).
pub fn channel() -> (Sender, Receiver) {
    let shared = Arc::new(Shared {
        value: AtomicU64::new(EMPTY),
        closed: AtomicBool::new(false),
        waker: AtomicWaker::new(),
    });
    (Sender(shared.clone()), Receiver(shared))
}

/// The sending half; dropping it closes the slot.
pub struct Sender(Arc<Shared>);

impl Sender {
    /// Replace any unread value with `value` and wake the receiver; `false`
    /// when the receiver is gone.
    ///
    /// # Panics
    ///
    /// If `value` is `u64::MAX`, which marks the empty slot.
    #[must_use = "`false` means the receiver is gone"]
    pub fn send(&self, value: u64) -> bool {
        assert_ne!(value, EMPTY, "u64::MAX marks the empty slot");
        if Arc::strong_count(&self.0) == 1 {
            return false;
        }
        self.0.value.store(value, Ordering::Release);
        self.0.waker.wake();
        true
    }

    /// The value the receiver has not read yet.
    pub fn unread(&self) -> Option<u64> {
        Some(self.0.value.load(Ordering::Acquire)).filter(|&value| value != EMPTY)
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.0.closed.store(true, Ordering::Release);
        self.0.waker.wake();
    }
}

impl fmt::Debug for Sender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender")
            .field("unread", &self.unread())
            .finish()
    }
}

/// The receiving half.
pub struct Receiver(Arc<Shared>);

impl Receiver {
    /// Take the unread value, if any, without waiting.
    pub fn try_recv(&mut self) -> Option<u64> {
        // The plain load keeps an empty poll from taking the cache line.
        if self.0.value.load(Ordering::Relaxed) == EMPTY {
            return None;
        }
        Some(self.0.value.swap(EMPTY, Ordering::Acquire)).filter(|&value| value != EMPTY)
    }

    /// Take the latest value, or `None` once the sender is gone and its last
    /// value read.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<u64>> {
        if let Some(value) = self.try_recv() {
            return Poll::Ready(Some(value));
        }
        self.0.waker.register(cx.waker());
        // Read the closed flag before the slot: a sender sends before closing.
        let closed = self.0.closed.load(Ordering::Acquire);
        match self.try_recv() {
            Some(value) => Poll::Ready(Some(value)),
            None if closed => Poll::Ready(None),
            None => Poll::Pending,
        }
    }

    /// Wait for the latest value, see [`Self::poll_recv`]. Cancel safe.
    pub async fn recv(&mut self) -> Option<u64> {
        poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// Whether the sender is gone; an unread value may remain.
    pub fn is_closed(&self) -> bool {
        self.0.closed.load(Ordering::Acquire)
    }
}

impl fmt::Debug for Receiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver")
            .field("closed", &self.is_closed())
            .finish()
    }
}

// Uses std threads, which loom's primitives reject; see `tests/loom.rs`.
#[cfg(all(test, not(loom)))]
mod tests {
    use std::{
        sync::atomic::AtomicUsize,
        task::{Wake, Waker},
        thread,
    };

    use futures::executor::block_on;
    use pretty_assertions::assert_eq;

    use super::*;

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

    #[test]
    fn the_latest_value_replaces_unread_ones() {
        let (tx, mut rx) = channel();
        for value in (1..=5000).rev() {
            assert!(tx.send(value));
        }
        assert_eq!(tx.unread(), Some(1));
        assert_eq!(rx.try_recv(), Some(1));
        assert_eq!(rx.try_recv(), None);
        assert_eq!(tx.unread(), None);
    }

    #[test]
    fn send_and_close_wake_the_receiver() {
        let (counter, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        let (tx, mut rx) = channel();

        assert_eq!(rx.poll_recv(&mut cx), Poll::Pending);
        assert!(tx.send(10));
        assert_eq!(wakes(&counter), 1);
        assert!(tx.send(5));
        assert_eq!(rx.poll_recv(&mut cx), Poll::Ready(Some(5)));

        assert_eq!(rx.poll_recv(&mut cx), Poll::Pending);
        let woken = wakes(&counter);
        drop(tx);
        assert!(wakes(&counter) > woken);
        assert!(rx.is_closed());
        assert_eq!(rx.poll_recv(&mut cx), Poll::Ready(None));
    }

    #[test]
    fn a_value_sent_before_close_is_received() {
        let (tx, mut rx) = channel();
        assert!(tx.send(7));
        drop(tx);
        assert_eq!(block_on(rx.recv()), Some(7));
        assert_eq!(block_on(rx.recv()), None);
    }

    #[test]
    fn send_without_receiver_fails() {
        let (tx, rx) = channel();
        drop(rx);
        assert!(!tx.send(1));
    }

    #[test]
    #[should_panic = "u64::MAX marks the empty slot"]
    fn sending_the_empty_marker_panics() {
        let (tx, _rx) = channel();
        let _ = tx.send(u64::MAX);
    }

    #[test]
    fn a_receiver_racing_the_sender_sees_decreasing_values_and_the_last() {
        const VALUES: u64 = 100_000;
        let (tx, mut rx) = channel();
        let sending = thread::spawn(move || {
            for value in (1..=VALUES).rev() {
                assert!(tx.send(value));
            }
        });
        let mut last = u64::MAX;
        while let Some(value) = block_on(rx.recv()) {
            assert!(value < last, "{value} after {last}");
            last = value;
        }
        sending.join().unwrap();
        assert_eq!(last, 1);
    }

    #[test]
    fn handles_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Sender>();
        assert_send_sync::<Receiver>();
        fn assert_send<T: Send>(_: &T) {}
        let (_tx, mut rx) = channel();
        assert_send(&rx.recv());
    }
}
