//! Runtimes for tests: one on a clock that moves only when told, and one
//! on the tokio runtime the test runs on.

use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

use futures::task::{FutureObj, Spawn, SpawnError};

use super::{
    DowncastLocalRuntime, DynTimer, Sleep, ThreadedRuntime, ThreadedRuntimeImpl, Timer,
    TimerBuilder, TimerImpl,
};

/// A monotonic clock that moves only by [`Self::advance`]; its sleeps wake
/// once it passes their targets.
#[derive(Clone, Debug)]
pub(crate) struct ManualClock(Arc<Mutex<State>>);

#[derive(Debug)]
struct State {
    now: Instant,
    /// The wakers of the sleeps not due yet, woken at every advance.
    waiting: Vec<Waker>,
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl ManualClock {
    /// A clock that starts at the standard clock's now.
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(State {
            now: Instant::now(),
            waiting: Vec::new(),
        })))
    }

    pub(crate) fn now(&self) -> Instant {
        self.0.lock().unwrap().now
    }

    /// Move the clock by `by`, waking every sleep so the due ones end.
    pub(crate) fn advance(&self, by: Duration) {
        let waiting = {
            let mut state = self.0.lock().unwrap();
            state.now += by;
            std::mem::take(&mut state.waiting)
        };
        for waker in waiting {
            waker.wake();
        }
    }

    /// A sleep that ends once the clock reaches `at`.
    pub(crate) fn sleep_until(&self, at: Instant) -> ManualSleep {
        ManualSleep {
            clock: self.clone(),
            at,
        }
    }

    /// A periodic timer on this clock whose first tick is immediate, like
    /// `tokio::time::interval`; each next tick is due a period after the
    /// last one was observed.
    pub(crate) fn delayed_timer(&self, period: Duration) -> TimerImpl {
        TimerImpl::Custom(DynTimer::new_box(ManualInterval {
            clock: self.clone(),
            next: self.now(),
            period,
        }))
    }
}

/// See [`ManualClock::sleep_until`].
pub(crate) struct ManualSleep {
    clock: ManualClock,
    at: Instant,
}

impl Future for ManualSleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = self.clock.0.lock().unwrap();
        if state.now >= self.at {
            return Poll::Ready(());
        }
        if !state.waiting.iter().any(|w| w.will_wake(cx.waker())) {
            state.waiting.push(cx.waker().clone());
        }
        Poll::Pending
    }
}

struct ManualInterval {
    clock: ManualClock,
    next: Instant,
    period: Duration,
}

impl Timer for ManualInterval {
    async fn tick(&mut self) {
        self.clock.sleep_until(self.next).await;
        self.next = self.clock.now() + self.period;
    }
}

/// A threaded runtime whose clock, sleeps and periodic timers are on a
/// [`ManualClock`]; it spawns on the tokio runtime the caller is in.
#[derive(Clone, Debug, Default)]
pub(crate) struct ManualRuntime {
    pub(crate) clock: ManualClock,
}

impl Spawn for ManualRuntime {
    fn spawn_obj(&self, future: FutureObj<'static, ()>) -> Result<(), SpawnError> {
        tokio::runtime::Handle::current().spawn(future);
        Ok(())
    }
}

impl DowncastLocalRuntime for ManualRuntime {}

impl TimerBuilder for ManualRuntime {
    fn create_delayed_timer(&self, duration: Duration) -> TimerImpl {
        self.clock.delayed_timer(duration)
    }

    fn sleep(&self, duration: Duration) -> Sleep {
        Box::pin(self.clock.sleep_until(self.clock.now() + duration))
    }

    fn now(&self) -> Instant {
        self.clock.now()
    }
}

impl ThreadedRuntime for ManualRuntime {}

/// A threaded runtime on the tokio runtime the caller is in, for tests that
/// spawn and time on it: the tokio one with the `tokio` feature, otherwise a
/// custom one with tokio's timers.
///
/// # Panics
///
/// Panics outside a tokio runtime.
pub(crate) fn current_tokio() -> ThreadedRuntimeImpl {
    #[cfg(feature = "tokio")]
    {
        ThreadedRuntimeImpl::new_tokio_rt()
    }
    #[cfg(not(feature = "tokio"))]
    {
        ThreadedRuntimeImpl::new_other_rt(CurrentTokio(tokio::runtime::Handle::current()))
    }
}

#[cfg(not(feature = "tokio"))]
struct CurrentTokio(tokio::runtime::Handle);

#[cfg(not(feature = "tokio"))]
impl Spawn for CurrentTokio {
    fn spawn_obj(&self, future: FutureObj<'static, ()>) -> Result<(), SpawnError> {
        self.0.spawn(future);
        Ok(())
    }
}

#[cfg(not(feature = "tokio"))]
impl DowncastLocalRuntime for CurrentTokio {}

#[cfg(not(feature = "tokio"))]
impl TimerBuilder for CurrentTokio {
    fn create_delayed_timer(&self, duration: Duration) -> TimerImpl {
        struct Interval(tokio::time::Interval);
        impl Timer for Interval {
            async fn tick(&mut self) {
                self.0.tick().await;
            }
        }
        let _guard = self.0.enter();
        let mut interval = tokio::time::interval(duration);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        TimerImpl::Custom(DynTimer::new_box(Interval(interval)))
    }

    fn sleep(&self, duration: Duration) -> Sleep {
        let _guard = self.0.enter();
        Box::pin(tokio::time::sleep(duration))
    }

    fn now(&self) -> Instant {
        let _guard = self.0.enter();
        tokio::time::Instant::now().into_std()
    }
}

#[cfg(not(feature = "tokio"))]
impl ThreadedRuntime for CurrentTokio {}
