#![allow(dead_code)]
use std::{rc::Rc, sync::Arc};

use futures::task::{LocalFutureObj, LocalSpawn, Spawn, SpawnError};
#[cfg(feature = "smol")]
use smol::future::FutureExt;

use crate::utils::logging::*;

/// ThreadedRuntime is a wrapper around the different multi-threaded runtime libraries.
/// It is used to hold by bolt-load client, and run different manager and its tasks.
// TODO: support?
#[derive(Clone)]
pub enum ThreadedRuntimeImpl {
    #[cfg(feature = "tokio")]
    Tokio(TokioThreadedRuntime),
    #[cfg(feature = "smol")]
    Smol(SmolThreadedRuntime),
    Other(Arc<dyn Spawn + Send + Sync + 'static>),
}

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
    pub fn new_smol_rt() -> Self {
        let available_threads = std::thread::available_parallelism().unwrap();
        Self::Smol(SmolThreadedRuntime::build_with_threads(
            available_threads.get(),
        ))
    }
}

impl Spawn for ThreadedRuntimeImpl {
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
    fn spawn_obj(&self, future: futures::future::FutureObj<'static, ()>) -> Result<(), SpawnError> {
        self.executor.spawn(future);
        Ok(())
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
impl TokioThreadedRuntime {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Spawn for TokioThreadedRuntime {
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

pub trait LocalRuntimeBuilder {
    fn build(&self) -> LocalRuntimeImpl;
}

pub trait LocalRuntimeExecutor {
    fn block_on(&self, future: Box<dyn std::future::Future<Output = ()>>);
}

pub trait LocalRuntime: LocalRuntimeExecutor + LocalSpawn {}

impl LocalRuntimeImpl {
    #[inline]
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
pub struct LocalTokioRuntime {
    // TODO: use `tokio::runtime::LocalRuntime` instead.
    rt: Rc<tokio::runtime::Runtime>,
}

#[cfg(feature = "tokio")]
tokio::task_local! {
    static IN_TOKIO_LOCAL_CONTEXT: bool;
}

#[cfg(feature = "tokio")]
impl LocalTokioRuntime {
    pub fn new() -> Self {
        Self {
            rt: Rc::new(
                tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap(),
            ),
        }
    }

    /// Block on a future in the local context.
    pub fn block_on<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        let local = tokio::task::LocalSet::new();
        local.block_on(&self.rt, async {
            IN_TOKIO_LOCAL_CONTEXT.scope(true, future).await
        })
    }
}

#[cfg(feature = "tokio")]
impl LocalSpawn for LocalTokioRuntime {
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

    pub fn block_on<F>(&self, future: F) -> F::Output
    where
        F: std::future::Future,
    {
        smol::future::block_on(self.rt.run(future))
    }
}

#[cfg(feature = "smol")]
impl LocalSpawn for SmolLocalRuntime {
    fn spawn_local_obj(&self, future: LocalFutureObj<'static, ()>) -> Result<(), SpawnError> {
        self.rt.spawn(future);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "smol")]
    use std::time::Duration;

    #[cfg(feature = "smol")]
    use super::SmolThreadedRuntime;

    #[test]
    #[cfg(feature = "smol")]
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
}
