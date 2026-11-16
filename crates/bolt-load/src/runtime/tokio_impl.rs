use std::{
    future::poll_fn,
    rc::Rc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use futures::task::{LocalFutureObj, LocalSpawn, Spawn, SpawnError};

use super::{DowncastLocalRuntime, LocalRuntimeImpl, Sleep, Timer, TimerBuilder, TimerImpl};

#[derive(Clone)]
enum TokioLocalRuntimeImpl {
    // TODO: use `tokio::runtime::LocalRuntime` instead.
    Runtime(Rc<tokio::runtime::Runtime>),
    Handle(tokio::runtime::Handle),
}

#[derive(Clone)]
pub struct LocalTokioRuntime {
    rt: TokioLocalRuntimeImpl,
}

impl TimerBuilder for LocalTokioRuntime {
    fn create_delayed_timer(&self, duration: Duration) -> TimerImpl {
        delayed_timer_on(self.handle(), duration)
    }

    fn sleep(&self, duration: Duration) -> Sleep {
        sleep_on(self.handle(), duration)
    }

    fn now(&self) -> Instant {
        now_on(self.handle())
    }
}

/// A tokio sleep registered with `handle`'s timer, so it can be created and
/// polled outside a tokio context.
fn sleep_on(handle: &tokio::runtime::Handle, duration: Duration) -> Sleep {
    let _guard = handle.enter();
    Box::pin(tokio::time::sleep(duration))
}

/// A periodic timer registered with `handle`'s timer, whatever runtime the
/// caller is in, or none.
fn delayed_timer_on(handle: &tokio::runtime::Handle, duration: Duration) -> TimerImpl {
    TimerImpl::Tokio(TokioDelayedTimer::on(handle.clone(), duration))
}

/// The time on `handle`'s clock: the one its timers use, paused or not.
pub(super) fn now_on(handle: &tokio::runtime::Handle) -> Instant {
    let _guard = handle.enter();
    tokio::time::Instant::now().into_std()
}

tokio::task_local! {
    static IN_TOKIO_LOCAL_CONTEXT: bool;
}

impl Default for LocalTokioRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalTokioRuntime {
    /// Create a local runtime from a current thread.
    pub fn new() -> Self {
        Self {
            rt: TokioLocalRuntimeImpl::Runtime(Rc::new(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap(),
            )),
        }
    }

    /// Create a local runtime from a handle.
    pub fn from_handle(handle: tokio::runtime::Handle) -> Self {
        Self {
            rt: TokioLocalRuntimeImpl::Handle(handle),
        }
    }

    /// The handle of the runtime its tasks and timers run on.
    fn handle(&self) -> &tokio::runtime::Handle {
        match &self.rt {
            TokioLocalRuntimeImpl::Runtime(rt) => rt.handle(),
            TokioLocalRuntimeImpl::Handle(handle) => handle,
        }
    }

    /// Block on a future in the local context.
    #[track_caller]
    pub fn block_on<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        let local = tokio::task::LocalSet::new();
        match &self.rt {
            TokioLocalRuntimeImpl::Runtime(rt) => local.block_on(rt, async {
                IN_TOKIO_LOCAL_CONTEXT.scope(true, future).await
            }),
            TokioLocalRuntimeImpl::Handle(handle) => handle.block_on(
                local.run_until(async { IN_TOKIO_LOCAL_CONTEXT.scope(true, future).await }),
            ),
        }
    }
}

impl LocalSpawn for LocalTokioRuntime {
    #[track_caller]
    fn spawn_local_obj(&self, future: LocalFutureObj<'static, ()>) -> Result<(), SpawnError> {
        // This is should be used only in local-set async context.
        assert!(tokio::runtime::Handle::try_current().is_ok());
        assert!(IN_TOKIO_LOCAL_CONTEXT.get(), "not in local context");
        tokio::task::spawn_local(async { IN_TOKIO_LOCAL_CONTEXT.scope(true, future).await });
        Ok(())
    }
}

pub enum TokioThreadedRuntime {
    Runtime(tokio::runtime::Runtime),
    /// If we can get the current handle, we can use it to spawn tasks.
    Handle(tokio::runtime::Handle),
}

impl TimerBuilder for TokioThreadedRuntime {
    fn create_delayed_timer(&self, duration: Duration) -> TimerImpl {
        delayed_timer_on(self.handle(), duration)
    }

    fn sleep(&self, duration: Duration) -> Sleep {
        sleep_on(self.handle(), duration)
    }

    fn now(&self) -> Instant {
        now_on(self.handle())
    }
}

impl Clone for TokioThreadedRuntime {
    fn clone(&self) -> Self {
        match self {
            TokioThreadedRuntime::Runtime(rt) => TokioThreadedRuntime::Handle(rt.handle().clone()),
            TokioThreadedRuntime::Handle(handle) => TokioThreadedRuntime::Handle(handle.clone()),
        }
    }
}

impl Default for TokioThreadedRuntime {
    fn default() -> Self {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => Self::Handle(handle),
            _ => Self::Runtime(
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .unwrap(),
            ),
        }
    }
}

impl DowncastLocalRuntime for TokioThreadedRuntime {
    fn downcast_local_runtime(&self) -> Option<LocalRuntimeImpl> {
        match self {
            TokioThreadedRuntime::Runtime(rt) => Some(LocalRuntimeImpl::Tokio(
                LocalTokioRuntime::from_handle(rt.handle().clone()),
            )),
            TokioThreadedRuntime::Handle(handle) => Some(LocalRuntimeImpl::Tokio(
                LocalTokioRuntime::from_handle(handle.clone()),
            )),
        }
    }
}

impl TokioThreadedRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// The handle of the runtime its tasks and timers run on.
    pub(crate) fn handle(&self) -> &tokio::runtime::Handle {
        match self {
            TokioThreadedRuntime::Runtime(rt) => rt.handle(),
            TokioThreadedRuntime::Handle(handle) => handle,
        }
    }
}

impl Spawn for TokioThreadedRuntime {
    #[track_caller]
    fn spawn_obj(
        &self,
        future: futures::future::FutureObj<'static, ()>,
    ) -> Result<(), futures::task::SpawnError> {
        match self {
            TokioThreadedRuntime::Runtime(rt) => {
                rt.spawn(future);
            }
            TokioThreadedRuntime::Handle(handle) => {
                handle.spawn(future);
            }
        }
        Ok(())
    }
}

pub struct TokioDelayedTimer {
    timer: tokio::time::Interval,
    /// The runtime of the timer, entered at every poll: tokio schedules the
    /// tick after a late one from the clock of the runtime it is polled in.
    handle: tokio::runtime::Handle,
}

impl TokioDelayedTimer {
    /// A timer on the tokio runtime the caller is in.
    ///
    /// # Panics
    ///
    /// Panics outside a tokio runtime with its timer enabled.
    pub fn new(duration: Duration) -> Self {
        Self::on(tokio::runtime::Handle::current(), duration)
    }

    /// A timer on `handle`'s runtime, whatever runtime the caller is in.
    fn on(handle: tokio::runtime::Handle, duration: Duration) -> Self {
        let timer = {
            let _guard = handle.enter();
            let mut timer = tokio::time::interval(duration);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            timer
        };
        Self { timer, handle }
    }
}

impl TokioDelayedTimer {
    /// Poll for the next tick, on the timer's own clock.
    #[inline]
    pub(crate) fn poll_tick(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let _guard = self.handle.enter();
        self.timer.poll_tick(cx).map(drop)
    }
}

impl Timer for TokioDelayedTimer {
    async fn tick(&mut self) {
        poll_fn(|cx| self.poll_tick(cx)).await
    }
}

#[cfg(test)]
mod tests {
    use tokio::runtime::{Builder, Runtime};

    use super::*;

    const HOUR: Duration = Duration::from_secs(3600);

    fn paused() -> Runtime {
        Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap()
    }

    fn real() -> Runtime {
        Builder::new_current_thread().enable_time().build().unwrap()
    }

    /// `rt`'s clock moves by an hour when `advance` moves its runtime's
    /// paused clock by one, read with no tokio context and with another
    /// runtime entered.
    fn assert_clock_follows(rt: &impl TimerBuilder, advance: impl Fn(Duration)) {
        let before = rt.now();
        advance(HOUR);
        let outside = rt.now();
        let other = real();
        let _guard = other.enter();
        let entered = rt.now();
        assert!(
            outside - before >= HOUR,
            "outside a runtime it read {:?} later",
            outside - before
        );
        assert!(
            entered - before >= HOUR,
            "under another runtime it read {:?} later",
            entered - before
        );
    }

    #[test]
    fn test_now_follows_the_paused_clock_of_its_runtime() {
        let owned = TokioThreadedRuntime::Runtime(paused());
        let TokioThreadedRuntime::Runtime(inner) = &owned else {
            unreachable!()
        };
        assert_clock_follows(&owned, |by| inner.block_on(tokio::time::advance(by)));

        let rt = paused();
        let handle = TokioThreadedRuntime::Handle(rt.handle().clone());
        assert_clock_follows(&handle, |by| rt.block_on(tokio::time::advance(by)));

        let local = LocalTokioRuntime::from_handle(rt.handle().clone());
        assert_clock_follows(&local, |by| rt.block_on(tokio::time::advance(by)));

        let owned = LocalTokioRuntime::new();
        owned.block_on(async { tokio::time::pause() });
        assert_clock_follows(&owned, |by| owned.block_on(tokio::time::advance(by)));
    }

    /// A timer of `rt` built with no tokio context and under another
    /// runtime ticks at once, then a period later on `rt`'s paused clock,
    /// which `drive` runs a future on.
    fn assert_timer_bound(rt: &impl TimerBuilder, drive: impl Fn(&mut TimerImpl) -> [bool; 3]) {
        let outside = rt.create_delayed_timer(HOUR);
        let other = real();
        let entered = {
            let _guard = other.enter();
            rt.create_delayed_timer(HOUR)
        };
        for (name, mut timer) in [("outside", outside), ("entered", entered)] {
            assert_eq!(
                drive(&mut timer),
                [true, false, true],
                "{name}: immediate, not before a period, then a period later"
            );
        }
    }

    /// Ticks within a millisecond, within half a period, then within a period.
    async fn ticks(timer: &mut TimerImpl) -> [bool; 3] {
        let mut ticked = [false; 3];
        for (ticked, within) in ticked
            .iter_mut()
            .zip([Duration::from_millis(1), HOUR / 2, HOUR])
        {
            *ticked = tokio::time::timeout(within, timer.tick()).await.is_ok();
        }
        ticked
    }

    #[test]
    fn test_delayed_timer_binds_to_its_runtime() {
        let owned = TokioThreadedRuntime::Runtime(paused());
        let TokioThreadedRuntime::Runtime(inner) = &owned else {
            unreachable!()
        };
        assert_timer_bound(&owned, |timer| inner.block_on(ticks(timer)));

        let rt = paused();
        let handle = TokioThreadedRuntime::Handle(rt.handle().clone());
        assert_timer_bound(&handle, |timer| rt.block_on(ticks(timer)));

        let local = LocalTokioRuntime::from_handle(rt.handle().clone());
        assert_timer_bound(&local, |timer| rt.block_on(ticks(timer)));

        let owned = LocalTokioRuntime::new();
        owned.block_on(async { tokio::time::pause() });
        assert_timer_bound(&owned, |timer| owned.block_on(ticks(timer)));
    }

    /// A tick taken under another runtime whose paused clock is an hour
    /// ahead schedules the next one on the timer's own clock: a period
    /// later there, not an hour.
    #[test]
    fn test_delayed_timer_ticks_on_its_clock_when_polled_elsewhere() {
        const PERIOD: Duration = Duration::from_millis(250);
        let own = paused();
        let other = paused();
        let rt = TokioThreadedRuntime::Handle(own.handle().clone());
        let TimerImpl::Tokio(mut timer) = rt.create_delayed_timer(PERIOD) else {
            unreachable!("a tokio runtime builds tokio timers")
        };
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(timer.poll_tick(&mut cx).is_ready(), "the first tick");
        own.block_on(tokio::time::advance(PERIOD));
        let taken = other.block_on(async {
            tokio::time::advance(HOUR).await;
            timer.poll_tick(&mut cx).is_ready()
        });
        assert!(taken, "the tick due on its own clock");
        let next = own.block_on(async {
            tokio::time::timeout(PERIOD + Duration::from_millis(1), timer.tick())
                .await
                .is_ok()
        });
        assert!(next, "the next tick comes a period later on its own clock");
    }
}
