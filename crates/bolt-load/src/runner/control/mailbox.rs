//! A single-slot mailbox carrying the latest size limit to a runner.
//!
//! The runner manager only ever lowers a runner's limit, so a newer limit
//! may replace one the runner has not read yet: however many limits are
//! sent before the runner polls, it reads only the last. Dropping the sender
//! closes the mailbox once its last limit is read.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Context, Poll},
};

use futures::task::AtomicWaker;

/// No limit is waiting; a limit is always below the total, so never this.
const EMPTY: u64 = u64::MAX;

#[derive(Debug)]
struct Shared {
    limit: AtomicU64,
    closed: AtomicBool,
    waker: AtomicWaker,
}

/// Create a mailbox, see the [module docs](self).
pub(crate) fn limit_mailbox() -> (LimitSender, LimitReceiver) {
    let shared = Arc::new(Shared {
        limit: AtomicU64::new(EMPTY),
        closed: AtomicBool::new(false),
        waker: AtomicWaker::new(),
    });
    (LimitSender(shared.clone()), LimitReceiver(shared))
}

#[derive(Debug)]
pub(crate) struct LimitSender(Arc<Shared>);

impl LimitSender {
    /// Replace any unread limit with `limit`; `false` when the receiver is gone.
    pub(crate) fn send(&self, limit: u64) -> bool {
        debug_assert_ne!(limit, EMPTY);
        if Arc::strong_count(&self.0) == 1 {
            return false;
        }
        self.0.limit.store(limit, Ordering::Release);
        self.0.waker.wake();
        true
    }

    /// The limit the receiver has not read yet.
    #[cfg(test)]
    pub(crate) fn unread(&self) -> Option<u64> {
        Some(self.0.limit.load(Ordering::Acquire)).filter(|&limit| limit != EMPTY)
    }
}

impl Drop for LimitSender {
    fn drop(&mut self) {
        self.0.closed.store(true, Ordering::Release);
        self.0.waker.wake();
    }
}

#[derive(Debug)]
pub(crate) struct LimitReceiver(Arc<Shared>);

impl LimitReceiver {
    fn take(&self) -> Option<u64> {
        if self.0.limit.load(Ordering::Relaxed) == EMPTY {
            return None;
        }
        Some(self.0.limit.swap(EMPTY, Ordering::Acquire)).filter(|&limit| limit != EMPTY)
    }

    /// The unread limit, if any.
    #[cfg(test)]
    pub(crate) fn try_recv(&mut self) -> Option<u64> {
        self.take()
    }

    /// The latest limit, or `None` once the sender is gone and every limit read.
    pub(crate) fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<u64>> {
        if let Some(limit) = self.take() {
            return Poll::Ready(Some(limit));
        }
        self.0.waker.register(cx.waker());
        // Read the closed flag before the slot: a sender sends before closing.
        let closed = self.0.closed.load(Ordering::Acquire);
        match self.take() {
            Some(limit) => Poll::Ready(Some(limit)),
            None if closed => Poll::Ready(None),
            None => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::AtomicUsize,
        task::{Context, Poll},
    };

    use futures::task::ArcWake;

    use super::*;

    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl ArcWake for WakeCount {
        fn wake_by_ref(this: &Arc<Self>) {
            this.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn test_latest_limit_replaces_unread_ones() {
        let (sender, mut receiver) = limit_mailbox();
        for limit in (1..=5000).rev() {
            assert!(sender.send(limit));
        }
        assert_eq!(receiver.try_recv(), Some(1));
        assert_eq!(receiver.try_recv(), None);
    }

    #[test]
    fn test_send_and_close_wake_the_receiver() {
        let wakes = Arc::new(WakeCount::default());
        let waker = futures::task::waker(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        let (sender, mut receiver) = limit_mailbox();

        assert_eq!(receiver.poll_recv(&mut cx), Poll::Pending);
        assert!(sender.send(10));
        assert_eq!(wakes.0.load(Ordering::SeqCst), 1);
        assert!(sender.send(5));
        assert_eq!(receiver.poll_recv(&mut cx), Poll::Ready(Some(5)));

        assert_eq!(receiver.poll_recv(&mut cx), Poll::Pending);
        let woken = wakes.0.load(Ordering::SeqCst);
        drop(sender);
        assert!(wakes.0.load(Ordering::SeqCst) > woken);
        assert_eq!(receiver.poll_recv(&mut cx), Poll::Ready(None));
    }

    #[test]
    fn test_limit_sent_before_close_is_received() {
        let (sender, mut receiver) = limit_mailbox();
        assert!(sender.send(7));
        drop(sender);
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert_eq!(receiver.poll_recv(&mut cx), Poll::Ready(Some(7)));
        assert_eq!(receiver.poll_recv(&mut cx), Poll::Ready(None));
    }

    #[test]
    fn test_send_without_receiver_fails() {
        let (sender, receiver) = limit_mailbox();
        drop(receiver);
        assert!(!sender.send(1));
    }

    #[test]
    fn test_receiver_racing_the_sender_sees_decreasing_limits_and_the_last() {
        const LIMITS: u64 = 100_000;
        let (sender, mut receiver) = limit_mailbox();
        let sending = std::thread::spawn(move || {
            for limit in (1..=LIMITS).rev() {
                assert!(sender.send(limit));
            }
        });
        let mut last = u64::MAX;
        futures::executor::block_on(std::future::poll_fn(|cx| {
            loop {
                match receiver.poll_recv(cx) {
                    Poll::Ready(Some(limit)) => {
                        assert!(limit < last, "{limit} after {last}");
                        last = limit;
                    }
                    Poll::Ready(None) => return Poll::Ready(()),
                    Poll::Pending => return Poll::Pending,
                }
            }
        }));
        sending.join().unwrap();
        assert_eq!(last, 1);
    }
}
