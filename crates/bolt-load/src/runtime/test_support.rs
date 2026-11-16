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
    /// The registrations of the sleeps not due yet, by id: their targets and
    /// wakers. An advance wakes those it makes due, and only those; a sleep
    /// dropped before takes its registration with it.
    waiting: Vec<(u64, Instant, Waker)>,
    /// The id of the next registration.
    next_id: u64,
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl ManualClock {
    /// A clock that starts at the standard clock's now.
    pub(crate) fn new() -> Self {
        Self::starting_at(Instant::now())
    }

    /// A clock that starts at `now`.
    pub(crate) fn starting_at(now: Instant) -> Self {
        Self(Arc::new(Mutex::new(State {
            now,
            waiting: Vec::new(),
            next_id: 0,
        })))
    }

    pub(crate) fn now(&self) -> Instant {
        self.0.lock().unwrap().now
    }

    /// Move the clock by `by`, waking the sleeps that are due then.
    pub(crate) fn advance(&self, by: Duration) {
        let due: Vec<Waker> = {
            let mut state = self.0.lock().unwrap();
            state.now += by;
            let now = state.now;
            let (due, waiting) = std::mem::take(&mut state.waiting)
                .into_iter()
                .partition(|(_, at, _)| *at <= now);
            state.waiting = waiting;
            due.into_iter().map(|(_, _, waker)| waker).collect()
        };
        for waker in due {
            waker.wake();
        }
    }

    /// How many distinct wakers wait for the clock.
    pub(crate) fn waiting(&self) -> usize {
        self.0.lock().unwrap().waiting.len()
    }

    /// Wait, a few real seconds at most, until something waits for the
    /// clock: the code under test has polled its sleeps.
    pub(crate) async fn until_waited_on(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.waiting() == 0 {
            assert!(Instant::now() < deadline, "nothing waits for the clock");
            async_io::Timer::after(Duration::from_millis(1)).await;
        }
    }

    /// A sleep that ends once the clock reaches `at`.
    pub(crate) fn sleep_until(&self, at: Instant) -> ManualSleep {
        ManualSleep {
            clock: self.clone(),
            at,
            id: None,
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
    /// Its registration with the clock, once polled.
    id: Option<u64>,
}

impl Future for ManualSleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = &mut *self;
        let mut state = this.clock.0.lock().unwrap();
        if state.now >= this.at {
            return Poll::Ready(());
        }
        let waker = cx.waker().clone();
        match this
            .id
            .and_then(|id| state.waiting.iter_mut().find(|(other, ..)| *other == id))
        {
            Some((_, _, registered)) => *registered = waker,
            None => {
                let id = state.next_id;
                state.next_id += 1;
                state.waiting.push((id, this.at, waker));
                this.id = Some(id);
            }
        }
        Poll::Pending
    }
}

impl Drop for ManualSleep {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            let mut state = self.clock.0.lock().unwrap();
            state.waiting.retain(|(other, ..)| *other != id);
        }
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

    fn sleep_until(&self, at: Instant) -> Sleep {
        Box::pin(self.clock.sleep_until(at))
    }

    fn now(&self) -> Instant {
        self.clock.now()
    }
}

impl ThreadedRuntime for ManualRuntime {}

/// A runtime on a [`ManualClock`] whose clock moves by `slip` as each sleep
/// starts: as another thread could move it between the read of the clock
/// and the start of the sleep. With `absolute`, its
/// [`TimerBuilder::sleep_until`] registers the target itself; without, it
/// is the default, a sleep for the time left as it read the clock.
#[derive(Clone, Debug)]
pub(crate) struct SlippingRuntime {
    pub(crate) clock: ManualClock,
    pub(crate) slip: Duration,
    pub(crate) absolute: bool,
}

impl Spawn for SlippingRuntime {
    fn spawn_obj(&self, future: FutureObj<'static, ()>) -> Result<(), SpawnError> {
        tokio::runtime::Handle::current().spawn(future);
        Ok(())
    }
}

impl DowncastLocalRuntime for SlippingRuntime {}

impl TimerBuilder for SlippingRuntime {
    fn create_delayed_timer(&self, duration: Duration) -> TimerImpl {
        self.clock.delayed_timer(duration)
    }

    fn sleep(&self, duration: Duration) -> Sleep {
        self.clock.advance(self.slip);
        Box::pin(self.clock.sleep_until(self.clock.now() + duration))
    }

    fn sleep_until(&self, at: Instant) -> Sleep {
        if !self.absolute {
            return super::sleep_for_the_time_left(self, at);
        }
        self.clock.advance(self.slip);
        Box::pin(self.clock.sleep_until(at))
    }

    fn now(&self) -> Instant {
        self.clock.now()
    }
}

impl ThreadedRuntime for SlippingRuntime {}

/// The furthest an [`Instant`] after `from` goes, to the nanosecond.
pub(crate) fn last_instant(from: Instant) -> Instant {
    /// The largest of `0..overflows` for which `fits` holds, `fits(0)` holding.
    fn largest(mut overflows: u64, fits: impl Fn(u64) -> bool) -> u64 {
        let mut largest = 0;
        while overflows - largest > 1 {
            let mid = largest + (overflows - largest) / 2;
            if fits(mid) {
                largest = mid;
            } else {
                overflows = mid;
            }
        }
        largest
    }
    let secs = largest(u64::MAX, |s| {
        from.checked_add(Duration::from_secs(s)).is_some()
    });
    let base = from + Duration::from_secs(secs);
    let nanos = largest(1_000_000_000, |n| {
        base.checked_add(Duration::from_nanos(n)).is_some()
    });
    base + Duration::from_nanos(nanos)
}

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
