#![allow(dead_code)]
use std::{rc::Rc, sync::Arc};

use bolt_load_utils::telemetry::*;
use futures::task::{LocalFutureObj, LocalSpawn, Spawn, SpawnError};
#[cfg(feature = "smol")]
use smol::future::FutureExt;

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

/// ThreadedRuntime is a trait to execute a future in the threaded context.
///
/// Provide the `spawn_obj` method to spawn tasks in the threaded context.
pub trait ThreadedRuntime: Spawn + DowncastLocalRuntime {}

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

#[cfg(feature = "smol")]
/// SmolRuntime is a dead simple runtime for smol.
/// It is used to run tasks via by `bolt-load` client.
#[derive(Clone)]
pub struct SmolThreadedRuntime {
    executor: std::sync::Arc<smol::Executor<'static>>,
    shutdown_signal: async_broadcast::Sender<()>,
}

#[cfg(feature = "smol")]
pub struct SmolJoinHandle<T> {
    task: smol::Task<T>,
}

#[cfg(feature = "smol")]
impl SmolThreadedRuntime {
    pub fn build_with_threads(threads: usize) -> Self {
        use std::sync::Arc;
        let executor = Arc::new(smol::Executor::new());
        let (shutdown_tx, shutdown_rx) = async_broadcast::broadcast(1);
        for i in 0..threads {
            debug!("spawn thread {} for smol runtime executor", i);
            let executor = executor.clone();
            let mut shutdown_signal = shutdown_rx.clone();
            std::thread::spawn(move || {
                loop {
                    if shutdown_signal.try_recv().is_ok() {
                        break;
                    }

                    let timer = async move {
                        smol::Timer::after(std::time::Duration::from_millis(100)).await;
                    };

                    smol::future::block_on(executor.run(executor.tick().or(timer)));
                }
            });
        }
        Self {
            executor,
            shutdown_signal: shutdown_tx,
        }
    }

    #[track_caller]
    pub fn spawn<T>(
        &self,
        future: impl std::future::Future<Output = T> + Send + 'static,
    ) -> SmolJoinHandle<T>
    where
        T: Send + 'static,
    {
        SmolJoinHandle {
            task: self.executor.spawn(future),
        }
    }

    #[track_caller]
    pub fn block_on<T>(&self, future: impl std::future::Future<Output = T> + Send + 'static) -> T
    where
        T: Send + 'static,
    {
        smol::future::block_on(async move { self.executor.run(future).await })
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown_signal.try_broadcast(());
    }
}

#[cfg(feature = "smol")]
impl Drop for SmolThreadedRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(feature = "smol")]
impl<T> SmolJoinHandle<T>
where
    T: Send + 'static,
{
    pub async fn join(self) -> T {
        self.task.await
    }

    pub async fn abort(self) -> Option<T> {
        self.task.cancel().await
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    pub fn detach(self) {
        self.task.detach();
    }
}

#[cfg(feature = "smol")]
impl Spawn for SmolThreadedRuntime {
    #[track_caller]
    fn spawn_obj(&self, future: futures::future::FutureObj<'static, ()>) -> Result<(), SpawnError> {
        self.executor.spawn(future).detach();
        Ok(())
    }
}

#[cfg(feature = "smol")]
impl DowncastLocalRuntime for SmolThreadedRuntime {
    fn downcast_local_runtime(&self) -> Option<LocalRuntimeImpl> {
        Some(LocalRuntimeImpl::Smol(SmolLocalRuntime::new()))
    }
}

#[cfg(feature = "tokio")]
pub enum TokioThreadedRuntime {
    Runtime(tokio::runtime::Runtime),
    /// If we can get the current handle, we can use it to spawn tasks.
    Handle(tokio::runtime::Handle),
}

#[cfg(feature = "tokio")]
impl Clone for TokioThreadedRuntime {
    fn clone(&self) -> Self {
        match self {
            TokioThreadedRuntime::Runtime(rt) => TokioThreadedRuntime::Handle(rt.handle().clone()),
            TokioThreadedRuntime::Handle(handle) => TokioThreadedRuntime::Handle(handle.clone()),
        }
    }
}

#[cfg(feature = "tokio")]
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

#[cfg(feature = "tokio")]
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

#[cfg(feature = "tokio")]
impl TokioThreadedRuntime {
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(feature = "tokio")]
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
pub trait LocalRuntime: LocalRuntimeExecutor + LocalSpawn {}

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

#[cfg(feature = "tokio")]
#[derive(Clone)]
enum TokioLocalRuntimeImpl {
    // TODO: use `tokio::runtime::LocalRuntime` instead.
    Runtime(Rc<tokio::runtime::Runtime>),
    Handle(tokio::runtime::Handle),
}

#[cfg(feature = "tokio")]
#[derive(Clone)]
pub struct LocalTokioRuntime {
    rt: TokioLocalRuntimeImpl,
}

#[cfg(feature = "tokio")]
tokio::task_local! {
    static IN_TOKIO_LOCAL_CONTEXT: bool;
}

#[cfg(feature = "tokio")]
impl LocalTokioRuntime {
    /// Create a local runtime from a current thread.
    pub fn new() -> Self {
        Self {
            rt: TokioLocalRuntimeImpl::Runtime(Rc::new(
                tokio::runtime::Builder::new_current_thread()
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

#[cfg(feature = "tokio")]
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

#[cfg(feature = "smol")]
#[derive(Clone)]
pub struct SmolLocalRuntime {
    rt: Rc<smol::LocalExecutor<'static>>,
}

#[cfg(feature = "smol")]
impl SmolLocalRuntime {
    pub fn new() -> Self {
        Self {
            rt: Rc::new(smol::LocalExecutor::new()),
        }
    }

    #[track_caller]
    pub fn block_on<F>(&self, future: F) -> F::Output
    where
        F: std::future::Future,
    {
        smol::future::block_on(self.rt.run(future))
    }
}

#[cfg(feature = "smol")]
impl LocalSpawn for SmolLocalRuntime {
    #[track_caller]
    fn spawn_local_obj(&self, future: LocalFutureObj<'static, ()>) -> Result<(), SpawnError> {
        self.rt.spawn(future).detach();
        Ok(())
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

impl Future for YieldNow {
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
    #[cfg(feature = "smol")]
    use std::time::Duration;

    use bolt_load_utils::telemetry::*;

    #[cfg(feature = "smol")]
    use super::SmolThreadedRuntime;
    use super::*;

    #[test]
    #[cfg(feature = "smol")]
    #[n0_tracing_test::traced_test]
    fn test_smol_runtime() {
        let rt = SmolThreadedRuntime::build_with_threads(5);
        let (tx, rx) = oneshot::channel();
        // test detach
        let handle = rt.spawn(async move {
            info!("start async");
            smol::Timer::after(Duration::from_millis(10)).await;
            info!("finished async");
            tx.send(200).unwrap();
        });
        assert!(!handle.is_finished());
        handle.detach();
        assert_eq!(rx.recv(), Ok(200));

        // test join
        let handle = rt.spawn(async move {
            info!("start async");
            smol::Timer::after(Duration::from_millis(10)).await;
            info!("finished async");
            200
        });
        assert!(!handle.is_finished());
        rt.block_on(async move {
            assert_eq!(handle.join().await, 200);
        });

        // test abort
        let handle = rt.spawn(async move {
            info!("start async");
            smol::Timer::after(Duration::from_millis(100)).await;
            info!("finished async");
            200
        });
        assert!(!handle.is_finished());
        rt.block_on(async move {
            assert_eq!(handle.abort().await, None);
        });
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
}
