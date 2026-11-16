#![doc = include_str!("../../docs/runtime.md")]
use std::{rc::Rc, sync::Arc};

use futures::task::{LocalSpawn, Spawn, SpawnError};

mod deadline;
mod event_timer;
#[cfg(feature = "smol")]
mod smol_impl;
#[cfg(test)]
pub(crate) mod test_support;
mod timer_source;
#[cfg(feature = "tokio")]
mod tokio_impl;

pub(crate) use deadline::{Deadline, capped_wait, instant_after};
pub(crate) use event_timer::EventTimer;
#[cfg(feature = "smol")]
pub use smol_impl::*;
pub(crate) use timer_source::TimerSource;
#[cfg(feature = "tokio")]
pub use tokio_impl::*;

/// A periodic timer.
///
/// Implement it with `async fn tick(&mut self)`; the returned future must be
/// `Send`. Wrap a custom timer with [`DynTimer::new_box`] to pass it as
/// [`TimerImpl::Custom`].
#[dynosaur::dynosaur(pub DynTimer = dyn(box) Timer, bridge(none))]
pub trait Timer: Send + Sync {
    fn tick(&mut self) -> impl std::future::Future<Output = ()> + Send;
}

pub enum TimerImpl {
    #[cfg(feature = "tokio")]
    Tokio(tokio_impl::TokioDelayedTimer),
    #[cfg(feature = "smol")]
    Smol(smol_impl::SmolDelayedTimer),
    Custom(Box<DynTimer<'static>>),
}

impl Timer for TimerImpl {
    #[expect(
        clippy::manual_async_fn,
        reason = "spelled out to declare the `Send` bound"
    )]
    fn tick(&mut self) -> impl std::future::Future<Output = ()> + Send {
        async move {
            match self {
                #[cfg(feature = "tokio")]
                TimerImpl::Tokio(timer) => {
                    timer.tick().await;
                }
                #[cfg(feature = "smol")]
                TimerImpl::Smol(timer) => {
                    timer.tick().await;
                }
                TimerImpl::Custom(timer) => {
                    timer.tick().await;
                }
            }
        }
    }
}

/// A one-shot sleep created by [`TimerBuilder::sleep`].
pub type Sleep = futures::future::BoxFuture<'static, ()>;

pub trait TimerBuilder {
    /// A `Timer` that ticks every `duration` and is delayed if it missed a tick.
    ///
    /// Like `tokio::time::interval`, the first tick completes immediately.
    fn create_delayed_timer(&self, duration: std::time::Duration) -> TimerImpl;

    /// Sleep for `duration` on this runtime's timer.
    ///
    /// The default waits for the second tick of [`Self::create_delayed_timer`],
    /// so it only sleeps for `duration` when the first tick is immediate.
    /// Custom runtimes should override it with their own one-shot sleep.
    fn sleep(&self, duration: std::time::Duration) -> Sleep {
        let mut timer = self.create_delayed_timer(duration);
        Box::pin(async move {
            timer.tick().await;
            timer.tick().await;
        })
    }

    /// Sleep until [`Self::now`] reaches `at`; a target reached ends at
    /// once.
    ///
    /// The default sleeps for the time from `now()` to `at`, so it ends late
    /// by however far the clock moved between that read and the start of
    /// the sleep. The engine's deadlines still fire once polled at or after
    /// their target, but on a runtime that wakes only the sleeps due, the
    /// wake-up for the target comes no earlier than that late end. A
    /// runtime whose clock can move meanwhile, such as a virtual clock
    /// advanced from another thread, should override it to register `at`
    /// itself, for exact wake-ups; the tokio and smol runtimes do.
    fn sleep_until(&self, at: std::time::Instant) -> Sleep {
        sleep_for_the_time_left(self, at)
    }

    /// The current time on this runtime's clock.
    ///
    /// The engine reads every instant it compares with a timer of this
    /// runtime from here, so the clock and the timers must share one time
    /// domain: a sleep of `d` started at `now()` ends when `now()` has
    /// moved by `d`. The default is [`std::time::Instant::now`]; a runtime
    /// whose timers run on another clock, such as a paused or virtual one,
    /// overrides it.
    ///
    /// The engine waits at most 30 years at once: every wait it arms, a
    /// timeout, a backoff or a metadata retry, generated with jitter or
    /// not, is capped there. For the whole download, the clock must stay at
    /// least 30 years plus the longest configured duration (timeouts,
    /// metadata retry delays) before the end of what an
    /// [`Instant`](std::time::Instant) holds, a duration over 30 years, such
    /// as [`Duration::MAX`], counting as 30 years: so never more than 60
    /// years. A tokio runtime's clock must likewise stay that far before
    /// the end of its timer's range, the milliseconds a `u64` counts from
    /// the runtime's start, so that every target fits in it too, as an
    /// unadvanced or normally advanced runtime does. The standard clock and
    /// smol's and tokio's, paused or not, meet this by a wide margin.
    /// Within it, no wait the engine arms for a duration configured or sent
    /// by a server overflows an instant or fires before its capped
    /// deadline: a configured 31-year timeout fires at 30 years. Outside it
    /// there is no guarantee, and the engine's arithmetic and tokio's own
    /// timers may panic.
    ///
    /// [`Duration::MAX`]: std::time::Duration::MAX
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }
}

/// The default [`TimerBuilder::sleep_until`]: `rt`'s sleep for the time
/// from its `now()` to `at`.
pub(crate) fn sleep_for_the_time_left<T: TimerBuilder + ?Sized>(
    rt: &T,
    at: std::time::Instant,
) -> Sleep {
    let now = rt.now();
    if now >= at {
        // A sleep of nothing may not work: the default `sleep` is built on a
        // periodic timer.
        return Box::pin(std::future::ready(()));
    }
    rt.sleep(at - now)
}

/// ThreadedRuntime is a wrapper around the different multi-threaded runtime libraries.
/// It is used to hold by bolt-load client, and run different manager and its tasks.
#[derive(Clone)]
pub enum ThreadedRuntimeImpl {
    #[cfg(feature = "tokio")]
    Tokio(TokioThreadedRuntime),
    #[cfg(feature = "smol")]
    Smol(SmolThreadedRuntime),
    Other(Arc<dyn ThreadedRuntime + Send + Sync + 'static>),
}

impl TimerBuilder for ThreadedRuntimeImpl {
    fn create_delayed_timer(&self, duration: std::time::Duration) -> TimerImpl {
        match self {
            #[cfg(feature = "tokio")]
            ThreadedRuntimeImpl::Tokio(rt) => rt.create_delayed_timer(duration),
            #[cfg(feature = "smol")]
            ThreadedRuntimeImpl::Smol(rt) => rt.create_delayed_timer(duration),
            ThreadedRuntimeImpl::Other(rt) => rt.create_delayed_timer(duration),
        }
    }

    fn sleep(&self, duration: std::time::Duration) -> Sleep {
        match self {
            #[cfg(feature = "tokio")]
            ThreadedRuntimeImpl::Tokio(rt) => rt.sleep(duration),
            #[cfg(feature = "smol")]
            ThreadedRuntimeImpl::Smol(rt) => rt.sleep(duration),
            ThreadedRuntimeImpl::Other(rt) => rt.sleep(duration),
        }
    }

    fn sleep_until(&self, at: std::time::Instant) -> Sleep {
        match self {
            #[cfg(feature = "tokio")]
            ThreadedRuntimeImpl::Tokio(rt) => rt.sleep_until(at),
            #[cfg(feature = "smol")]
            ThreadedRuntimeImpl::Smol(rt) => rt.sleep_until(at),
            ThreadedRuntimeImpl::Other(rt) => rt.sleep_until(at),
        }
    }

    fn now(&self) -> std::time::Instant {
        match self {
            #[cfg(feature = "tokio")]
            ThreadedRuntimeImpl::Tokio(rt) => rt.now(),
            #[cfg(feature = "smol")]
            ThreadedRuntimeImpl::Smol(rt) => rt.now(),
            ThreadedRuntimeImpl::Other(rt) => rt.now(),
        }
    }
}

/// ThreadedRuntime is a trait to execute a future in the threaded context.
///
/// Provide the `spawn_obj` method to spawn tasks in the threaded context.
pub trait ThreadedRuntime: Spawn + DowncastLocalRuntime + TimerBuilder {}

pub trait ThreadedRuntimeExt: ThreadedRuntime {
    /// Downcast the runtime to a local runtime.
    ///
    /// If the runtime is a custom runtime and downcast is not supported, it will use the builder to create a local runtime.
    ///
    /// # Arguments
    ///
    /// * `builder` - The builder to create a local runtime.
    fn downcast_local(&self, builder: Option<LocalRuntimeBuilderImpl>) -> Option<LocalRuntimeImpl> {
        self.downcast_local_runtime()
            .or_else(|| builder.map(|builder| builder.build()))
    }
}

impl ThreadedRuntimeExt for ThreadedRuntimeImpl {}

impl ThreadedRuntimeImpl {
    #[cfg(feature = "tokio")]
    /// Create a new tokio runtime.
    /// If the current thread already has a tokio runtime, it will be used.
    pub fn new_tokio_rt() -> Self {
        Self::Tokio(match tokio::runtime::Handle::try_current() {
            Ok(handle) => TokioThreadedRuntime::Handle(handle),
            _ => TokioThreadedRuntime::Runtime(tokio::runtime::Runtime::new().unwrap()),
        })
    }

    #[cfg(feature = "smol")]
    /// Create a new smol threaded runtime.
    ///
    pub fn new_smol_rt() -> Self {
        let available_threads = std::thread::available_parallelism().unwrap();
        Self::Smol(SmolThreadedRuntime::build_with_threads(
            available_threads.get(),
        ))
    }

    /// Create a new other runtime.
    ///
    /// It is used to create a runtime from a custom runtime.
    ///
    /// # Arguments
    ///
    /// * `rt` - The custom runtime.
    pub fn new_other_rt<T: ThreadedRuntime + Send + Sync + 'static>(rt: T) -> Self {
        Self::Other(Arc::new(rt))
    }
}

impl Spawn for ThreadedRuntimeImpl {
    #[track_caller]
    fn spawn_obj(&self, future: futures::future::FutureObj<'static, ()>) -> Result<(), SpawnError> {
        match self {
            #[cfg(feature = "tokio")]
            ThreadedRuntimeImpl::Tokio(rt) => rt.spawn_obj(future),
            #[cfg(feature = "smol")]
            ThreadedRuntimeImpl::Smol(rt) => rt.spawn_obj(future),
            ThreadedRuntimeImpl::Other(rt) => rt.spawn_obj(future),
        }
    }
}

impl DowncastLocalRuntime for ThreadedRuntimeImpl {
    fn downcast_local_runtime(&self) -> Option<LocalRuntimeImpl> {
        match self {
            #[cfg(feature = "tokio")]
            ThreadedRuntimeImpl::Tokio(rt) => rt.downcast_local_runtime(),
            #[cfg(feature = "smol")]
            ThreadedRuntimeImpl::Smol(rt) => rt.downcast_local_runtime(),
            ThreadedRuntimeImpl::Other(rt) => rt.downcast_local_runtime(),
        }
    }
}

impl ThreadedRuntime for ThreadedRuntimeImpl {}

/// LocalRuntime is a wrapper around the different single-threaded runtime libraries.
/// It is used to hold by bolt-load client, and run different manager and its tasks.
#[derive(Clone)]
pub enum LocalRuntimeImpl {
    #[cfg(feature = "tokio")]
    Tokio(LocalTokioRuntime),
    #[cfg(feature = "smol")]
    Smol(SmolLocalRuntime),
    Other(Rc<dyn LocalRuntime>),
}

/// LocalRuntimeBuilder is a trait to build a local runtime.
///
/// It is used to create a local runtime from a new thread.
///
pub trait LocalRuntimeBuilder {
    fn build(&self) -> LocalRuntimeImpl;
}

/// LocalRuntimeExecutor is a trait to execute a future in the local context.
///
/// Provide the `block_on` method to execute a future in the local context.
pub trait LocalRuntimeExecutor {
    fn block_on(&self, future: Box<dyn std::future::Future<Output = ()>>);
}

/// LocalRuntime is a trait to execute a future in the local context.
///
/// Provide the `spawn_local_obj` method to spawn tasks in the local context.
pub trait LocalRuntime: LocalRuntimeExecutor + LocalSpawn + TimerBuilder {}

/// `DowncastLocalRuntime` is a trait to downcast a runtime to a local runtime.
///
/// It is used to downcast a threaded runtime to a local runtime.
///
pub trait DowncastLocalRuntime {
    fn downcast_local_runtime(&self) -> Option<LocalRuntimeImpl> {
        None
    }
}

impl LocalRuntimeImpl {
    #[inline]
    #[track_caller]
    pub fn block_on(&self, future: impl std::future::Future<Output = ()> + 'static) {
        match self {
            #[cfg(feature = "tokio")]
            LocalRuntimeImpl::Tokio(rt) => rt.block_on(future),
            #[cfg(feature = "smol")]
            LocalRuntimeImpl::Smol(rt) => rt.block_on(future),
            LocalRuntimeImpl::Other(rt) => rt.block_on(Box::new(future)),
        }
    }
}

#[derive(Clone)]
pub enum LocalRuntimeBuilderImpl {
    #[cfg(feature = "tokio")]
    Tokio,
    #[cfg(feature = "smol")]
    Smol,
    /// User can use this to impl their own local runtime builder.
    Other(Arc<dyn LocalRuntimeBuilder + Send + Sync + 'static>),
}

impl TimerBuilder for LocalRuntimeImpl {
    fn create_delayed_timer(&self, duration: std::time::Duration) -> TimerImpl {
        match self {
            #[cfg(feature = "tokio")]
            LocalRuntimeImpl::Tokio(rt) => rt.create_delayed_timer(duration),
            #[cfg(feature = "smol")]
            LocalRuntimeImpl::Smol(rt) => rt.create_delayed_timer(duration),
            LocalRuntimeImpl::Other(rt) => rt.create_delayed_timer(duration),
        }
    }

    fn sleep(&self, duration: std::time::Duration) -> Sleep {
        match self {
            #[cfg(feature = "tokio")]
            LocalRuntimeImpl::Tokio(rt) => rt.sleep(duration),
            #[cfg(feature = "smol")]
            LocalRuntimeImpl::Smol(rt) => rt.sleep(duration),
            LocalRuntimeImpl::Other(rt) => rt.sleep(duration),
        }
    }

    fn sleep_until(&self, at: std::time::Instant) -> Sleep {
        match self {
            #[cfg(feature = "tokio")]
            LocalRuntimeImpl::Tokio(rt) => rt.sleep_until(at),
            #[cfg(feature = "smol")]
            LocalRuntimeImpl::Smol(rt) => rt.sleep_until(at),
            LocalRuntimeImpl::Other(rt) => rt.sleep_until(at),
        }
    }

    fn now(&self) -> std::time::Instant {
        match self {
            #[cfg(feature = "tokio")]
            LocalRuntimeImpl::Tokio(rt) => rt.now(),
            #[cfg(feature = "smol")]
            LocalRuntimeImpl::Smol(rt) => rt.now(),
            LocalRuntimeImpl::Other(rt) => rt.now(),
        }
    }
}

impl LocalRuntimeBuilder for LocalRuntimeBuilderImpl {
    fn build(&self) -> LocalRuntimeImpl {
        match self {
            #[cfg(feature = "tokio")]
            LocalRuntimeBuilderImpl::Tokio => LocalRuntimeImpl::Tokio(LocalTokioRuntime::new()),
            #[cfg(feature = "smol")]
            LocalRuntimeBuilderImpl::Smol => LocalRuntimeImpl::Smol(SmolLocalRuntime::new()),
            LocalRuntimeBuilderImpl::Other(builder) => builder.build(),
        }
    }
}

// Ref: https://github.com/smol-rs/futures-lite/blob/329be16e987f947552d0c77785c662e3166e706a/src/future.rs#L216
/// Wakes the current task and returns [`Poll::Pending`] once.
///
/// This function is useful when we want to cooperatively give time to the task scheduler. It is
/// generally a good idea to yield inside loops because that way we make sure long-running tasks
/// don't prevent other tasks from running.
///
/// # Examples
///
/// ```
/// use futures_lite::future;
///
/// # spin_on::spin_on(async {
/// future::yield_now().await;
/// # })
/// ```
pub fn yield_now() -> YieldNow {
    YieldNow(false)
}

#[derive(Debug, snafu::Snafu)]
#[snafu(display("timeout deadline exceeded"))]
pub struct TimeoutError;

pub async fn timeout<T>(
    duration: std::time::Duration,
    future: impl std::future::Future<Output = T>,
) -> Result<T, TimeoutError> {
    use futures::FutureExt;
    use futures_concurrency::prelude::*;
    let timer = async {
        async_io::Timer::after(duration).await;
        Err(TimeoutError)
    };
    (future.map(Ok), timer).race().await
}

/// Future for the [`yield_now()`] function.
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct YieldNow(bool);

impl std::future::Future for YieldNow {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        if !self.0 {
            self.0 = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: ?Sized + Send + Sync>() {}
    fn assert_send<T: Send>(_: &T) {}

    #[test]
    fn test_timers_and_their_ticks_are_send() {
        assert_send_sync::<TimerImpl>();
        assert_send_sync::<DynTimer<'static>>();

        struct NoopTimer;
        impl Timer for NoopTimer {
            async fn tick(&mut self) {}
        }
        let mut timer = TimerImpl::Custom(DynTimer::new_box(NoopTimer));
        assert_send(&timer.tick());
        let mut boxed = DynTimer::new_box(NoopTimer);
        assert_send(&boxed.tick());
    }

    #[tokio::test]
    async fn test_timeout() {
        let result = timeout(std::time::Duration::from_secs(1), async {
            async_io::Timer::after(std::time::Duration::from_secs(2)).await;
        })
        .await;
        assert!(result.is_err());

        let result = timeout(std::time::Duration::from_secs(2), async {
            async_io::Timer::after(std::time::Duration::from_secs(1)).await;
        })
        .await;
        assert!(result.is_ok());
    }

    /// The clock of a custom runtime is the one both wrappers read.
    #[test]
    fn test_custom_clock_is_forwarded() {
        use test_support::{ManualClock, ManualRuntime};

        struct LocalOnClock(ManualClock);
        impl LocalRuntimeExecutor for LocalOnClock {
            fn block_on(&self, _: Box<dyn std::future::Future<Output = ()>>) {
                unreachable!("only the clock is read")
            }
        }
        impl LocalSpawn for LocalOnClock {
            fn spawn_local_obj(
                &self,
                _: futures::future::LocalFutureObj<'static, ()>,
            ) -> Result<(), SpawnError> {
                unreachable!("only the clock is read")
            }
        }
        impl TimerBuilder for LocalOnClock {
            fn create_delayed_timer(&self, duration: std::time::Duration) -> TimerImpl {
                self.0.delayed_timer(duration)
            }
            fn now(&self) -> std::time::Instant {
                self.0.now()
            }
        }
        impl LocalRuntime for LocalOnClock {}

        let clock = ManualClock::new();
        let threaded = ThreadedRuntimeImpl::new_other_rt(ManualRuntime {
            clock: clock.clone(),
        });
        let local = LocalRuntimeImpl::Other(Rc::new(LocalOnClock(clock.clone())));
        let start = clock.now();
        assert_eq!(threaded.now(), start);
        assert_eq!(local.now(), start);
        clock.advance(std::time::Duration::from_secs(3600));
        let later = start + std::time::Duration::from_secs(3600);
        assert_eq!(
            threaded.now(),
            later,
            "the threaded wrapper reads the custom clock"
        );
        assert_eq!(
            local.now(),
            later,
            "the local wrapper reads the custom clock"
        );
    }

    /// A runtime that does not override the clock reads the standard one.
    #[test]
    fn test_default_clock_is_the_standard_clock() {
        struct TimerOnly;
        impl TimerBuilder for TimerOnly {
            fn create_delayed_timer(&self, _: std::time::Duration) -> TimerImpl {
                unreachable!("only the clock is read")
            }
        }

        let before = std::time::Instant::now();
        let now = TimerOnly.now();
        assert!(before <= now && now <= std::time::Instant::now());
    }

    #[cfg(feature = "tokio")]
    #[tokio::test]
    async fn test_default_sleep_waits_for_the_second_tick() {
        struct TimerOnly;
        impl TimerBuilder for TimerOnly {
            fn create_delayed_timer(&self, duration: std::time::Duration) -> TimerImpl {
                TimerImpl::Tokio(TokioDelayedTimer::new(duration))
            }
        }

        let delay = std::time::Duration::from_millis(50);
        let start = std::time::Instant::now();
        TimerOnly.sleep(delay).await;
        assert!(start.elapsed() >= delay);
    }
}
