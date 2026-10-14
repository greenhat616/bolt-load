use std::{
    future::poll_fn,
    rc::Rc,
    task::{Context, Poll},
    time::Duration,
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
        TimerImpl::Tokio(TokioDelayedTimer::new(duration))
    }

    fn sleep(&self, duration: Duration) -> Sleep {
        match &self.rt {
            TokioLocalRuntimeImpl::Runtime(rt) => sleep_on(rt.handle(), duration),
            TokioLocalRuntimeImpl::Handle(handle) => sleep_on(handle, duration),
        }
    }
}

/// A tokio sleep registered with `handle`'s timer, so it can be created and
/// polled outside a tokio context.
fn sleep_on(handle: &tokio::runtime::Handle, duration: Duration) -> Sleep {
    let _guard = handle.enter();
    Box::pin(tokio::time::sleep(duration))
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
        TimerImpl::Tokio(TokioDelayedTimer::new(duration))
    }

    fn sleep(&self, duration: Duration) -> Sleep {
        match self {
            TokioThreadedRuntime::Runtime(rt) => sleep_on(rt.handle(), duration),
            TokioThreadedRuntime::Handle(handle) => sleep_on(handle, duration),
        }
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
}

impl TokioDelayedTimer {
    pub fn new(duration: Duration) -> Self {
        let mut timer = tokio::time::interval(duration);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Self { timer }
    }
}

impl TokioDelayedTimer {
    /// Poll for the next tick.
    #[inline]
    pub(crate) fn poll_tick(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        self.timer.poll_tick(cx).map(drop)
    }
}

impl Timer for TokioDelayedTimer {
    async fn tick(&mut self) {
        poll_fn(|cx| self.poll_tick(cx)).await
    }
}
