use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::Waker,
};

use futures::task::ArcWake;

use super::*;
use crate::runtime::{
    DowncastLocalRuntime, ThreadedRuntime, TimerBuilder, TimerImpl,
    test_support::{ManualClock, ManualRuntime, SlippingRuntime, last_instant},
};

const SECOND: Duration = Duration::from_secs(1);

/// Counts its wake-ups.
#[derive(Default)]
struct WakeCount(AtomicUsize);

impl ArcWake for WakeCount {
    fn wake_by_ref(this: &Arc<Self>) {
        this.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Polls a deadline by hand, counting the wake-ups it gets.
struct Poller {
    wakes: Arc<WakeCount>,
    waker: Waker,
}

impl Poller {
    fn new() -> Self {
        let wakes = Arc::new(WakeCount::default());
        let waker = futures::task::waker(wakes.clone());
        Self { wakes, waker }
    }

    fn poll(&self, deadline: &mut Deadline) -> bool {
        deadline
            .poll(&mut Context::from_waker(&self.waker))
            .is_ready()
    }

    fn woken(&self) -> usize {
        self.wakes.0.load(Ordering::SeqCst)
    }
}

/// A custom runtime on a manual clock; the deadline takes the `Other` path.
fn manual() -> (ManualClock, ThreadedRuntimeImpl) {
    let clock = ManualClock::new();
    let rt = ThreadedRuntimeImpl::new_other_rt(ManualRuntime {
        clock: clock.clone(),
    });
    (clock, rt)
}

#[test]
fn test_custom_fires_at_its_target() {
    let (clock, rt) = manual();
    let poller = Poller::new();
    let mut deadline = rt.deadline(clock.now() + 10 * SECOND);
    assert!(!poller.poll(&mut deadline));
    clock.advance(10 * SECOND - Duration::from_nanos(1));
    assert_eq!(poller.woken(), 0, "no wake before the target");
    assert!(!poller.poll(&mut deadline), "a nanosecond early");
    let seen = poller.woken();
    clock.advance(Duration::from_nanos(1));
    assert!(
        poller.woken() > seen,
        "reaching the target wakes the poller"
    );
    assert!(poller.poll(&mut deadline));
}

/// A deadline first polled late keeps its target: the time left is taken
/// at the first poll.
#[test]
fn test_custom_keeps_its_target_when_first_polled_late() {
    let (clock, rt) = manual();
    let poller = Poller::new();
    let mut deadline = rt.deadline(clock.now() + 10 * SECOND);
    clock.advance(6 * SECOND);
    assert!(!poller.poll(&mut deadline));
    clock.advance(4 * SECOND);
    assert!(
        poller.poll(&mut deadline),
        "fired at the target, not 6 s after"
    );
}

#[test]
fn test_custom_stays_fired_until_reset() {
    let (clock, rt) = manual();
    let poller = Poller::new();
    let mut deadline = rt.deadline(clock.now() + SECOND);
    clock.advance(SECOND);
    for _ in 0..3 {
        assert!(poller.poll(&mut deadline), "ready until reset");
    }
    deadline.reset(clock.now() + SECOND);
    assert!(
        !poller.poll(&mut deadline),
        "a reset clears the fired state"
    );
    clock.advance(SECOND);
    assert!(poller.poll(&mut deadline));
}

#[test]
fn test_custom_reset_moves_the_target() {
    let (clock, rt) = manual();
    let t0 = clock.now();
    let poller = Poller::new();

    // Before the first poll.
    let mut deadline = rt.deadline(t0 + 10 * SECOND);
    deadline.reset(t0 + 2 * SECOND);
    assert_eq!(deadline.at(), t0 + 2 * SECOND);
    assert!(!poller.poll(&mut deadline));
    clock.advance(2 * SECOND);
    assert!(
        poller.poll(&mut deadline),
        "reset earlier before the first poll"
    );

    // Later while pending: the old sleep no longer fires it.
    let mut deadline = rt.deadline(t0 + 4 * SECOND);
    assert!(!poller.poll(&mut deadline));
    deadline.reset(t0 + 6 * SECOND);
    assert!(!poller.poll(&mut deadline));
    let seen = poller.woken();
    clock.advance(2 * SECOND);
    assert_eq!(poller.woken(), seen, "the dropped sleep does not wake it");
    assert!(!poller.poll(&mut deadline), "the old target passed");
    let seen = poller.woken();
    clock.advance(2 * SECOND);
    assert!(poller.woken() > seen, "the reset target wakes the poller");
    assert!(poller.poll(&mut deadline));

    // Earlier while pending.
    let mut deadline = rt.deadline(clock.now() + 10 * SECOND);
    assert!(!poller.poll(&mut deadline));
    deadline.reset(clock.now() + SECOND);
    assert!(!poller.poll(&mut deadline));
    clock.advance(SECOND);
    assert!(poller.poll(&mut deadline), "reset earlier while pending");

    // Later after it fired.
    deadline.reset(clock.now() + SECOND);
    assert!(!poller.poll(&mut deadline));
    clock.advance(SECOND);
    assert!(poller.poll(&mut deadline), "reset after it fired");
}

/// A custom runtime with the default sleep, whose periodic timer must never
/// be asked for nothing.
#[derive(Clone)]
struct DefaultSleep {
    clock: ManualClock,
    timers: Arc<std::sync::Mutex<Vec<Duration>>>,
}

impl futures::task::Spawn for DefaultSleep {
    fn spawn_obj(
        &self,
        _: futures::future::FutureObj<'static, ()>,
    ) -> Result<(), futures::task::SpawnError> {
        unreachable!("nothing is spawned")
    }
}

impl DowncastLocalRuntime for DefaultSleep {}

impl TimerBuilder for DefaultSleep {
    fn create_delayed_timer(&self, duration: Duration) -> TimerImpl {
        assert!(!duration.is_zero(), "a periodic timer of nothing");
        self.timers.lock().unwrap().push(duration);
        self.clock.delayed_timer(duration)
    }

    fn now(&self) -> Instant {
        self.clock.now()
    }
}

impl ThreadedRuntime for DefaultSleep {}

/// A target already reached fires without a sleep; a later one sleeps on
/// the default sleep, for the time left.
#[test]
fn test_custom_default_sleep() {
    let rt = DefaultSleep {
        clock: ManualClock::new(),
        timers: Arc::default(),
    };
    let clock = rt.clock.clone();
    let timers = rt.timers.clone();
    let rt = ThreadedRuntimeImpl::new_other_rt(rt);
    let poller = Poller::new();
    for at in [clock.now(), clock.now() - SECOND] {
        let mut deadline = rt.deadline(at);
        assert!(poller.poll(&mut deadline), "a target reached fires at once");
    }
    let mut deadline = rt.deadline(clock.now() + SECOND);
    deadline.reset(clock.now());
    assert!(poller.poll(&mut deadline), "reset to now fires at once");
    assert!(timers.lock().unwrap().is_empty(), "no sleep for nothing");

    let mut deadline = rt.deadline(clock.now() + 3 * SECOND);
    clock.advance(SECOND);
    assert!(!poller.poll(&mut deadline));
    assert_eq!(*timers.lock().unwrap(), [2 * SECOND], "the time left");
    clock.advance(2 * SECOND - Duration::from_nanos(1));
    assert!(!poller.poll(&mut deadline));
    clock.advance(Duration::from_nanos(1));
    assert!(poller.poll(&mut deadline));
}

/// A custom runtime on a manual clock that counts its sleeps alive.
#[derive(Clone)]
struct CountedSleeps {
    clock: ManualClock,
    alive: Arc<AtomicUsize>,
}

/// Alive until dropped.
struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl futures::task::Spawn for CountedSleeps {
    fn spawn_obj(
        &self,
        _: futures::future::FutureObj<'static, ()>,
    ) -> Result<(), futures::task::SpawnError> {
        unreachable!("nothing is spawned")
    }
}

impl DowncastLocalRuntime for CountedSleeps {}

impl TimerBuilder for CountedSleeps {
    fn create_delayed_timer(&self, duration: Duration) -> TimerImpl {
        self.clock.delayed_timer(duration)
    }

    fn sleep(&self, duration: Duration) -> Sleep {
        self.alive.fetch_add(1, Ordering::SeqCst);
        let counted = Counted(self.alive.clone());
        let sleep = self.clock.sleep_until(self.clock.now() + duration);
        Box::pin(async move {
            sleep.await;
            drop(counted);
        })
    }

    fn now(&self) -> Instant {
        self.clock.now()
    }
}

impl ThreadedRuntime for CountedSleeps {}

/// A reset drops the sleep for the old target, and so does dropping the
/// deadline.
#[test]
fn test_custom_drops_its_sleep() {
    let rt = CountedSleeps {
        clock: ManualClock::new(),
        alive: Arc::default(),
    };
    let (clock, alive) = (rt.clock.clone(), rt.alive.clone());
    let rt = ThreadedRuntimeImpl::new_other_rt(rt);
    let alive = move || alive.load(Ordering::SeqCst);
    let poller = Poller::new();
    let mut deadline = rt.deadline(clock.now() + SECOND);
    assert_eq!(alive(), 0, "no sleep before the first poll");
    assert!(!poller.poll(&mut deadline));
    assert_eq!(alive(), 1);
    deadline.reset(clock.now() + 2 * SECOND);
    assert_eq!(alive(), 0, "a reset drops the old sleep");
    assert!(!poller.poll(&mut deadline));
    assert_eq!(alive(), 1);
    drop(deadline);
    assert_eq!(alive(), 0, "dropping the deadline drops its sleep");
}

#[test]
fn test_far_targets_saturate() {
    let now = Instant::now();
    assert_eq!(instant_after(now, SECOND), now + SECOND);
    assert_eq!(instant_after(now, Duration::MAX), now + FAR_FUTURE);
    assert_eq!(instant_after(now, FAR_FUTURE), now + FAR_FUTURE);
    assert_eq!(
        instant_after(now, FAR_FUTURE + SECOND),
        now + FAR_FUTURE,
        "a duration an `Instant` holds is capped too"
    );
    assert_eq!(capped_wait(FAR_FUTURE + SECOND), FAR_FUTURE);
    assert_eq!(capped_wait(Duration::MAX), FAR_FUTURE);
    assert_eq!(capped_wait(SECOND), SECOND);
}

/// On a clock with less than 30 years left before the end of what an
/// `Instant` holds, a far target is the furthest instant the halving of 30
/// years finds: at least half of what is left, and no panic.
#[test]
fn test_far_targets_saturate_near_the_end_of_instants() {
    let end = last_instant(Instant::now());
    let left = 86400 * SECOND;
    let now = end - left;
    assert!(
        now.checked_add(FAR_FUTURE).is_none(),
        "less than 30 years left"
    );
    for after in [Duration::MAX, FAR_FUTURE, 2 * left] {
        let at = instant_after(now, after);
        assert!(
            at > now + left / 2 && at <= end,
            "{after:?}: {:?} after now",
            at - now
        );
    }
    assert_eq!(instant_after(now, SECOND), now + SECOND);
}

/// A sleep that starts after the clock reached the target, which the
/// default `sleep_until` then arms a full timeout too late, does not hold
/// the deadline back: it fires with no further move of the clock.
#[test]
fn test_custom_fires_when_the_clock_reaches_the_target_as_the_sleep_starts() {
    let clock = ManualClock::new();
    let rt = ThreadedRuntimeImpl::new_other_rt(SlippingRuntime {
        clock: clock.clone(),
        slip: 10 * SECOND,
        absolute: false,
    });
    let poller = Poller::new();
    let mut deadline = rt.deadline(clock.now() + 10 * SECOND);
    assert!(poller.poll(&mut deadline), "fired at the target");
}

/// A runtime that registers its targets wakes the deadline at its target,
/// before any poll, however far its clock moved as the sleep started; its
/// clock wakes only the sleeps due.
#[test]
fn test_custom_wakes_at_its_target_when_the_clock_moves_as_the_sleep_starts() {
    let clock = ManualClock::new();
    let t0 = clock.now();
    let rt = ThreadedRuntimeImpl::new_other_rt(SlippingRuntime {
        clock: clock.clone(),
        slip: 4 * SECOND,
        absolute: true,
    });
    let poller = Poller::new();
    let mut deadline = rt.deadline(t0 + 10 * SECOND);
    assert!(!poller.poll(&mut deadline));
    assert_eq!(clock.now(), t0 + 4 * SECOND, "the sleep started late");
    clock.advance(6 * SECOND - Duration::from_nanos(1));
    assert_eq!(poller.woken(), 0, "no wake a nanosecond early");
    clock.advance(Duration::from_nanos(1));
    assert_eq!(poller.woken(), 1, "the target wakes the poller");
    assert!(poller.poll(&mut deadline), "fired at the target");
}

/// The default `sleep_until`, armed late because the clock moved as it
/// started, does not move the target: a poll once the clock reached it
/// fires. Its wake-up comes at the late end of its sleep, 4 s after.
#[test]
fn test_custom_default_sleep_keeps_its_target_when_it_starts_late() {
    let slipping = || {
        let clock = ManualClock::new();
        let rt = ThreadedRuntimeImpl::new_other_rt(SlippingRuntime {
            clock: clock.clone(),
            slip: 4 * SECOND,
            absolute: false,
        });
        let deadline = rt.deadline(clock.now() + 10 * SECOND);
        (clock, deadline)
    };

    // The first poll reads the clock at 0 and starts the sleep at 4 s,
    // for 10 s.
    let (clock, mut deadline) = slipping();
    let poller = Poller::new();
    assert!(!poller.poll(&mut deadline));
    clock.advance(6 * SECOND - Duration::from_nanos(1));
    assert!(!poller.poll(&mut deadline), "a nanosecond early");
    clock.advance(Duration::from_nanos(1));
    assert!(
        poller.poll(&mut deadline),
        "fired at the target, not 4 s after"
    );

    let (clock, mut deadline) = slipping();
    let poller = Poller::new();
    assert!(!poller.poll(&mut deadline));
    clock.advance(6 * SECOND);
    assert_eq!(poller.woken(), 0, "the target does not wake it");
    clock.advance(4 * SECOND);
    assert!(poller.woken() > 0, "the late end of its sleep does");
    assert!(poller.poll(&mut deadline));
}

/// A custom runtime that registers absolute targets only: a relative sleep
/// would panic.
#[derive(Clone)]
struct AbsoluteOnly(ManualClock);

impl futures::task::Spawn for AbsoluteOnly {
    fn spawn_obj(
        &self,
        _: futures::future::FutureObj<'static, ()>,
    ) -> Result<(), futures::task::SpawnError> {
        unreachable!("nothing is spawned")
    }
}

impl DowncastLocalRuntime for AbsoluteOnly {}

impl TimerBuilder for AbsoluteOnly {
    fn create_delayed_timer(&self, _: Duration) -> TimerImpl {
        unreachable!("no periodic timer")
    }

    fn sleep(&self, _: Duration) -> Sleep {
        unreachable!("the deadline registers its target")
    }

    fn sleep_until(&self, at: Instant) -> Sleep {
        Box::pin(self.0.sleep_until(at))
    }

    fn now(&self) -> Instant {
        self.0.now()
    }
}

impl ThreadedRuntime for AbsoluteOnly {}

/// A custom runtime's deadline sleeps until its target with the runtime's
/// own `sleep_until`, which both wrappers forward.
#[test]
fn test_custom_sleeps_until_its_target() {
    let clock = ManualClock::new();
    let rt = ThreadedRuntimeImpl::new_other_rt(AbsoluteOnly(clock.clone()));
    let poller = Poller::new();
    let mut deadline = rt.deadline(clock.now() + 10 * SECOND);
    assert!(!poller.poll(&mut deadline));
    let seen = poller.woken();
    clock.advance(10 * SECOND);
    assert!(poller.woken() > seen, "its sleep wakes the poller");
    assert!(poller.poll(&mut deadline));

    let local = crate::runtime::LocalRuntimeImpl::Other(std::rc::Rc::new(LocalAbsolute(
        AbsoluteOnly(clock.clone()),
    )));
    let sleeps: [&dyn Fn(Instant) -> Sleep; 2] =
        [&|at| rt.sleep_until(at), &|at| local.sleep_until(at)];
    for sleep_until in sleeps {
        let mut sleep = sleep_until(clock.now() + SECOND);
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(sleep.as_mut().poll(&mut cx).is_pending());
        clock.advance(SECOND);
        assert!(sleep.as_mut().poll(&mut cx).is_ready(), "forwarded");
    }
}

/// [`AbsoluteOnly`] as a local runtime.
struct LocalAbsolute(AbsoluteOnly);

impl crate::runtime::LocalRuntimeExecutor for LocalAbsolute {
    fn block_on(&self, _: Box<dyn Future<Output = ()>>) {
        unreachable!("only its sleeps are used")
    }
}

impl futures::task::LocalSpawn for LocalAbsolute {
    fn spawn_local_obj(
        &self,
        _: futures::future::LocalFutureObj<'static, ()>,
    ) -> Result<(), futures::task::SpawnError> {
        unreachable!("only its sleeps are used")
    }
}

impl TimerBuilder for LocalAbsolute {
    fn create_delayed_timer(&self, duration: Duration) -> TimerImpl {
        self.0.create_delayed_timer(duration)
    }

    fn sleep(&self, duration: Duration) -> Sleep {
        self.0.sleep(duration)
    }

    fn sleep_until(&self, at: Instant) -> Sleep {
        self.0.sleep_until(at)
    }

    fn now(&self) -> Instant {
        self.0.now()
    }
}

impl crate::runtime::LocalRuntime for LocalAbsolute {}

/// The default `sleep_until` ends at once at a target reached, without a
/// periodic timer of nothing, and otherwise sleeps for the time left.
#[test]
fn test_default_sleep_until() {
    let rt = DefaultSleep {
        clock: ManualClock::new(),
        timers: Arc::default(),
    };
    let clock = rt.clock.clone();
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    for at in [clock.now(), clock.now() - SECOND] {
        assert!(rt.sleep_until(at).as_mut().poll(&mut cx).is_ready());
    }
    assert!(rt.timers.lock().unwrap().is_empty(), "no sleep for nothing");
    let mut sleep = rt.sleep_until(clock.now() + 3 * SECOND);
    assert!(sleep.as_mut().poll(&mut cx).is_pending());
    assert_eq!(*rt.timers.lock().unwrap(), [3 * SECOND], "the time left");
}

/// Poll `deadline` until it fires, within `within` of the standard clock.
async fn fires_within(deadline: &mut Deadline, within: Duration) -> bool {
    let fired = std::future::poll_fn(|cx| deadline.poll(cx));
    let watchdog = async_io::Timer::after(within);
    matches!(
        futures::future::select(std::pin::pin!(fired), watchdog).await,
        futures::future::Either::Left(_)
    )
}

/// The deadlines on the standard clock: smol's and the fallback.
async fn assert_on_the_standard_clock(deadline: impl Fn(Instant) -> Deadline) {
    const STEP: Duration = Duration::from_millis(50);
    let poller = Poller::new();

    let mut past = deadline(Instant::now() - SECOND);
    assert!(fires_within(&mut past, SECOND).await, "a past target fires");
    assert!(poller.poll(&mut past), "ready until reset");
    past.reset(Instant::now() + 3600 * SECOND);
    assert!(!poller.poll(&mut past), "a reset clears the fired state");

    let start = Instant::now();
    let mut later = deadline(start + 3600 * SECOND);
    assert!(!poller.poll(&mut later));
    later.reset(start + STEP);
    assert!(fires_within(&mut later, 10 * SECOND).await);
    assert!(Instant::now() >= start + STEP, "not before the target");

    let mut delayed = deadline(Instant::now() + STEP);
    async_io::Timer::after(2 * STEP).await;
    assert!(
        poller.poll(&mut delayed),
        "fires at the target when polled late"
    );
}

#[test]
fn test_fallback() {
    futures::executor::block_on(assert_on_the_standard_clock(Deadline::fallback));
}

#[cfg(feature = "smol")]
#[test]
fn test_smol() {
    let rt = ThreadedRuntimeImpl::Smol(crate::runtime::SmolThreadedRuntime::build_with_threads(1));
    smol::block_on(assert_on_the_standard_clock(|at| rt.deadline(at)));
}

#[cfg(feature = "tokio")]
mod tokio {
    use ::tokio::{
        runtime::{Builder, Runtime},
        time::timeout,
    };

    use super::*;
    use crate::runtime::TokioThreadedRuntime;

    const HOUR: Duration = Duration::from_secs(3600);
    const MS: Duration = Duration::from_millis(1);

    fn paused() -> Runtime {
        Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap()
    }

    /// Whether `deadline` fires within `within` of the paused clock.
    async fn fires_within(deadline: &mut Deadline, within: Duration) -> bool {
        timeout(within, std::future::poll_fn(|cx| deadline.poll(cx)))
            .await
            .is_ok()
    }

    /// On the paused clock of `paused`, built with no tokio context and
    /// under another runtime: it fires at its target and not before, stays
    /// fired until reset, and resets earlier, later and after firing.
    /// A target within a millisecond of the end of what an `Instant` holds
    /// does not overflow tokio's rounding to its tick: a deadline there,
    /// reset there, and a sleep until there, stay pending.
    #[test]
    fn test_tokio_near_the_end_of_instants() {
        use crate::runtime::test_support::last_instant;

        let paused = paused();
        let rt = ThreadedRuntimeImpl::Tokio(TokioThreadedRuntime::Handle(paused.handle().clone()));
        let at = last_instant(rt.now()) - Duration::from_nanos(500);
        let poller = Poller::new();
        paused.block_on(async {
            let mut deadline = rt.deadline(at);
            assert!(!poller.poll(&mut deadline));
            deadline.reset(at);
            assert!(!poller.poll(&mut deadline));
            let mut sleep = rt.sleep_until(at);
            let mut cx = Context::from_waker(&poller.waker);
            assert!(sleep.as_mut().poll(&mut cx).is_pending());
        });
    }

    #[test]
    fn test_tokio_on_the_runtime_clock() {
        let paused = paused();
        let rt = ThreadedRuntimeImpl::Tokio(TokioThreadedRuntime::Handle(paused.handle().clone()));
        let other = Builder::new_current_thread().enable_time().build().unwrap();
        let outside = rt.deadline(rt.now() + HOUR);
        let entered = {
            let _guard = other.enter();
            rt.deadline(rt.now() + HOUR)
        };
        for (name, mut deadline) in [("outside", outside), ("entered", entered)] {
            let start = rt.now();
            deadline.reset(start + HOUR);
            paused.block_on(async {
                assert!(
                    !fires_within(&mut deadline, HOUR - MS).await,
                    "{name}: early"
                );
                assert!(
                    fires_within(&mut deadline, 2 * MS).await,
                    "{name}: at the target"
                );
                assert!(
                    fires_within(&mut deadline, Duration::ZERO).await,
                    "{name}: stays fired"
                );
            });
            assert!(rt.now() >= start + HOUR);
        }

        let mut deadline = rt.deadline(rt.now() + HOUR);
        paused.block_on(async {
            assert!(!fires_within(&mut deadline, HOUR / 2).await);
            // Earlier while pending.
            deadline.reset(rt.now() + HOUR / 4);
            assert!(!fires_within(&mut deadline, HOUR / 4 - MS).await);
            assert!(fires_within(&mut deadline, 2 * MS).await);
            // Later after it fired.
            deadline.reset(rt.now() + HOUR);
            assert!(
                !fires_within(&mut deadline, HOUR / 2).await,
                "reset after it fired"
            );
            // Later while pending.
            deadline.reset(rt.now() + HOUR);
            assert!(!fires_within(&mut deadline, HOUR - MS).await);
            assert!(fires_within(&mut deadline, 2 * MS).await);
            // A past target.
            deadline.reset(rt.now() - SECOND);
            assert!(fires_within(&mut deadline, MS).await, "a past target fires");
        });
    }
}
