//! A slot for one waker, registered and woken without a lock.
//!
//! The algorithm of `futures::task::AtomicWaker`, built on [`crate::sync`] so
//! loom can model it together with the protocols that use it.

use std::{fmt, task::Waker};

use crate::sync::{AtomicUsize, Ordering, UnsafeCell, yield_now};

/// Nobody touches the waker.
const WAITING: usize = 0;
/// `register` owns the waker.
const REGISTERING: usize = 0b01;
/// `wake` owns the waker, or asked a concurrent `register` to wake it.
const WAKING: usize = 0b10;

pub(crate) struct AtomicWaker {
    state: AtomicUsize,
    waker: UnsafeCell<Option<Waker>>,
}

// SAFETY: `waker` is only accessed by the side that moved `state` out of
// `WAITING`, so it is never accessed from two threads at once.
unsafe impl Send for AtomicWaker {}
// SAFETY: as above.
unsafe impl Sync for AtomicWaker {}

impl AtomicWaker {
    pub(crate) fn new() -> Self {
        Self {
            state: AtomicUsize::new(WAITING),
            waker: UnsafeCell::new(None),
        }
    }

    /// Store `waker` for the next [`Self::wake`].
    ///
    /// Memory written before a `wake` that does not wake this waker is
    /// visible once `register` returns; a `wake` racing it wakes `waker`.
    /// Only one thread may register at a time.
    pub(crate) fn register(&self, waker: &Waker) {
        match self
            .state
            .compare_exchange(WAITING, REGISTERING, Ordering::Acquire, Ordering::Acquire)
            .unwrap_or_else(|state| state)
        {
            WAITING => {
                // SAFETY: `REGISTERING` gives this thread the waker.
                self.waker.with_mut(|slot| unsafe {
                    match &*slot {
                        Some(old) if old.will_wake(waker) => {}
                        _ => *slot = Some(waker.clone()),
                    }
                });
                if let Err(state) = self.state.compare_exchange(
                    REGISTERING,
                    WAITING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    // A `wake` arrived while registering and left the
                    // waking to this thread, which still owns the waker.
                    debug_assert_eq!(state, REGISTERING | WAKING);
                    // SAFETY: still `REGISTERING`.
                    let waker = self.waker.with_mut(|slot| unsafe { (*slot).take() });
                    self.state.swap(WAITING, Ordering::AcqRel);
                    if let Some(waker) = waker {
                        waker.wake();
                    }
                }
            }
            // A `wake` holds the old waker; wake the new one directly.
            WAKING => {
                waker.wake_by_ref();
                // The caller polls again at once; give the `wake` a chance
                // to finish first.
                yield_now();
            }
            state => debug_assert!(
                state == REGISTERING || state == REGISTERING | WAKING,
                "concurrent register"
            ),
        }
    }

    /// Wake the registered waker, if any, and clear it.
    pub(crate) fn wake(&self) {
        if let Some(waker) = self.take() {
            waker.wake();
        }
    }

    fn take(&self) -> Option<Waker> {
        match self.state.fetch_or(WAKING, Ordering::AcqRel) {
            WAITING => {
                // SAFETY: `WAKING` from `WAITING` gives this thread the waker.
                let waker = self.waker.with_mut(|slot| unsafe { (*slot).take() });
                self.state.fetch_and(!WAKING, Ordering::Release);
                waker
            }
            // A `register` in progress sees `WAKING` and wakes, or another
            // `wake` already took the waker.
            _ => None,
        }
    }
}

impl fmt::Debug for AtomicWaker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AtomicWaker").finish_non_exhaustive()
    }
}
