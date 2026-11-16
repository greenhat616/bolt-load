//! A one-shot timer at an absolute instant, re-armed in place.

use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, ready},
    time::{Duration, Instant},
};

#[cfg(feature = "smol")]
use super::smol_impl::SmolDeadline;
#[cfg(feature = "tokio")]
use super::tokio_impl::TokioDeadline;
use super::{Sleep, ThreadedRuntime, ThreadedRuntimeImpl};

/// Fires once the clock of its runtime reaches its target.
///
/// It is polled in place, as the event sources of the loops are. Once
/// fired it stays ready until [`Self::reset`]. A reset registers no waker:
/// the caller polls it again after one.
pub(crate) struct Deadline {
    at: Instant,
    fired: bool,
    timer: DeadlineTimer,
}

enum DeadlineTimer {
    #[cfg(feature = "tokio")]
    Tokio(TokioDeadline),
    #[cfg(feature = "smol")]
    Smol(SmolDeadline),
    /// A custom runtime's [`TimerBuilder::sleep_until`] the target. It is
    /// created at the first poll after the deadline was armed: a runtime
    /// may start a sleep's time only once it is polled.
    Custom {
        source: Arc<dyn ThreadedRuntime + Send + Sync + 'static>,
        sleep: Option<Sleep>,
    },
    /// `async-io`'s timer on the standard clock, for a runner built without
    /// a timer source.
    Fallback(async_io::Timer),
}

/// 30 years, tokio's far future: the longest the engine waits at once,
/// never in practice.
const FAR_FUTURE: Duration = Duration::from_secs(86400 * 365 * 30);

/// `wait` capped at 30 years, the longest the engine waits at once: what
/// [`instant_after`] arms for it, and so what windows derived from a
/// configured duration, such as the strategy's from the connect timeout,
/// count with.
pub(crate) fn capped_wait(wait: Duration) -> Duration {
    wait.min(FAR_FUTURE)
}

/// `now + after`, `after` capped at 30 years, the longest the engine waits
/// at once: every wait of the engine, a timeout, a backoff or a metadata
/// retry, goes through here. On a clock too near the end of what an
/// [`Instant`] holds for that, which the clock contract of
/// [`TimerBuilder::now`](super::TimerBuilder::now) rules out, the furthest
/// instant within it that halving the capped duration finds. It never
/// panics.
pub(crate) fn instant_after(now: Instant, after: Duration) -> Instant {
    let mut far = capped_wait(after);
    loop {
        if let Some(at) = now.checked_add(far) {
            return at;
        }
        // Ends at zero at the latest, which `now` holds.
        far /= 2;
    }
}

impl ThreadedRuntimeImpl {
    /// A deadline at `at` on this runtime's clock.
    pub(crate) fn deadline(&self, at: Instant) -> Deadline {
        match self {
            #[cfg(feature = "tokio")]
            ThreadedRuntimeImpl::Tokio(rt) => Deadline::tokio(rt.handle(), at),
            #[cfg(feature = "smol")]
            ThreadedRuntimeImpl::Smol(_) => Deadline::smol(at),
            ThreadedRuntimeImpl::Other(rt) => Deadline::custom(rt.clone(), at),
        }
    }
}

impl Deadline {
    fn new(at: Instant, timer: DeadlineTimer) -> Self {
        Self {
            at,
            fired: false,
            timer,
        }
    }

    /// A deadline at `at` on the clock of `handle`'s runtime, with its
    /// timer.
    #[cfg(feature = "tokio")]
    pub(crate) fn tokio(handle: &tokio::runtime::Handle, at: Instant) -> Self {
        Self::new(at, DeadlineTimer::Tokio(TokioDeadline::new(handle, at)))
    }

    /// A deadline at `at` on the standard clock, with smol's timer.
    #[cfg(feature = "smol")]
    pub(crate) fn smol(at: Instant) -> Self {
        Self::new(at, DeadlineTimer::Smol(SmolDeadline::new(at)))
    }

    /// A deadline at `at` on the clock of the custom runtime `source`.
    pub(crate) fn custom(
        source: Arc<dyn ThreadedRuntime + Send + Sync + 'static>,
        at: Instant,
    ) -> Self {
        Self::new(
            at,
            DeadlineTimer::Custom {
                source,
                sleep: None,
            },
        )
    }

    /// A deadline at `at` on the standard clock, with `async-io`'s timer.
    pub(crate) fn fallback(at: Instant) -> Self {
        Self::new(at, DeadlineTimer::Fallback(async_io::Timer::at(at)))
    }

    /// When it fires.
    pub(crate) fn at(&self) -> Instant {
        self.at
    }

    /// Fire at `at` instead, whether it fired already or not.
    pub(crate) fn reset(&mut self, at: Instant) {
        self.at = at;
        self.fired = false;
        match &mut self.timer {
            #[cfg(feature = "tokio")]
            DeadlineTimer::Tokio(timer) => timer.reset(at),
            #[cfg(feature = "smol")]
            DeadlineTimer::Smol(timer) => timer.reset(at),
            // The old sleep is dropped: it waits for the old target.
            DeadlineTimer::Custom { sleep, .. } => *sleep = None,
            DeadlineTimer::Fallback(timer) => timer.set_at(at),
        }
    }

    /// Ready once the clock reached the target, and from then on until the
    /// next reset.
    pub(crate) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if self.fired {
            return Poll::Ready(());
        }
        match &mut self.timer {
            #[cfg(feature = "tokio")]
            DeadlineTimer::Tokio(timer) => ready!(timer.poll(cx)),
            #[cfg(feature = "smol")]
            DeadlineTimer::Smol(timer) => ready!(timer.poll(cx)),
            DeadlineTimer::Custom { source, sleep } => {
                // The clock is read at every poll, not only the sleep's: a
                // runtime's default `sleep_until` sleeps for the time left
                // as it read it, and ends late if its clock moved before
                // the sleep started. A target reached needs no sleep
                // either.
                if source.now() < self.at {
                    let sleep = sleep.get_or_insert_with(|| source.sleep_until(self.at));
                    // The clock may also have reached the target while the
                    // sleep started.
                    if sleep.as_mut().poll(cx).is_pending() && source.now() < self.at {
                        return Poll::Pending;
                    }
                }
            }
            DeadlineTimer::Fallback(timer) => {
                ready!(Pin::new(timer).poll(cx));
            }
        }
        self.fired = true;
        Poll::Ready(())
    }
}

#[cfg(test)]
mod tests;
