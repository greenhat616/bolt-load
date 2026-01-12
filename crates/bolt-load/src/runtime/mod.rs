#![allow(dead_code)]
use std::{rc::Rc, sync::Arc};

use futures::task::{LocalSpawn, Spawn, SpawnError};

#[cfg(feature = "smol")]
mod smol_impl;
#[cfg(feature = "tokio")]
mod tokio_impl;

#[cfg(feature = "smol")]
pub use smol_impl::*;
#[cfg(feature = "tokio")]
pub use tokio_impl::*;

pub trait Timer {
    fn tick(&mut self) -> impl std::future::Future<Output = ()>;
}

#[async_trait::async_trait]
pub trait ObjectSafeTimer: Send + Sync + 'static {
    async fn tick(&mut self);
}

impl<T: ObjectSafeTimer> Timer for T {
    fn tick(&mut self) -> impl std::future::Future<Output = ()> {
        self.tick()
    }
}

pub enum TimerImpl {
    #[cfg(feature = "tokio")]
    Tokio(tokio_impl::TokioDelayedTimer),
    #[cfg(feature = "smol")]
    Smol(smol_impl::SmolDelayedTimer),
    Custom(Box<dyn ObjectSafeTimer + Send + Sync + 'static>),
}

impl Timer for TimerImpl {
    #[allow(clippy::manual_async_fn)]
    fn tick(&mut self) -> impl std::future::Future<Output = ()> {
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

pub trait TimerBuilder {
    /// A `Timer` that will be delayed if missed a tick in the interval.
    fn create_delayed_timer(&self, duration: std::time::Duration) -> TimerImpl;
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

#[derive(Debug, thiserror::Error)]
#[error("timeout deadline exceeded")]
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
}
