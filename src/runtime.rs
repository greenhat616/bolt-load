#![allow(dead_code)]
#[cfg(feature = "smol")]
use smol::future::FutureExt;

/// Runtime is a wrapper around the different runtime libraries.
/// It is used to hold by bolt-load client, and run different manager and its tasks.
// TODO: support?
pub enum Runtime {
    #[cfg(feature = "tokio")]
    Tokio(TokioRuntime),
    #[cfg(feature = "smol")]
    Smol(SmolRuntime),
}

impl Runtime {
    #[cfg(feature = "tokio")]
    /// Create a new tokio runtime.
    /// If the current thread already has a tokio runtime, it will be used.
    pub fn new_tokio_rt() -> Self {
        Self::Tokio(TokioRuntime::Runtime(
            tokio::runtime::Runtime::new().unwrap(),
        ))
    }

    #[cfg(feature = "smol")]
    pub fn new_smol_rt() -> Self {
        let available_threads = std::thread::available_parallelism().unwrap();
        Self::Smol(SmolRuntime::build_with_threads(available_threads.get()))
    }
}

#[cfg(feature = "tokio")]
pub enum TokioRuntime {
    Runtime(tokio::runtime::Runtime),
    Handle(tokio::runtime::Handle),
}

#[cfg(feature = "tokio")]
impl TokioRuntime {
    pub fn new() -> Self {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            Self::Handle(handle)
        } else {
            Self::Runtime(
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .unwrap(),
            )
        }
    }
}

#[cfg(feature = "smol")]
/// SmolRuntime is a dead simple runtime for smol.
/// It is used to run tasks via by `bolt-load` client.
pub struct SmolRuntime {
    executor: std::sync::Arc<smol::Executor<'static>>,
    shutdown_signal: async_broadcast::Sender<()>,
}

#[cfg(feature = "smol")]
pub struct SmolJoinHandle<T> {
    task: smol::Task<T>,
}

#[cfg(feature = "smol")]
impl SmolRuntime {
    pub fn build_with_threads(threads: usize) -> Self {
        use std::sync::Arc;
        let executor = Arc::new(smol::Executor::new());
        let (shutdown_tx, shutdown_rx) = async_broadcast::broadcast(1);
        for i in 0..threads {
            log::debug!("spawn thread {} for smol runtime executor", i);
            let executor = executor.clone();
            let mut shutdown_signal = shutdown_rx.clone();
            std::thread::spawn(move || loop {
                if shutdown_signal.try_recv().is_ok() {
                    break;
                }

                let timer = async move {
                    smol::Timer::after(std::time::Duration::from_millis(100)).await;
                };

                smol::future::block_on(executor.run(executor.tick().or(timer)));
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
impl Drop for SmolRuntime {
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

pub enum JoinHandle<T: Send + 'static> {
    #[cfg(feature = "tokio")]
    Tokio(tokio::task::JoinHandle<T>),
    #[cfg(feature = "smol")]
    Smol(SmolJoinHandle<T>),
}

#[derive(Debug, thiserror::Error)]
pub enum JoinError {
    #[cfg(feature = "tokio")]
    #[error(transparent)]
    Tokio(#[from] tokio::task::JoinError),
}

impl<T: Send + 'static> JoinHandle<T> {
    pub async fn join(self) -> Result<T, JoinError> {
        match self {
            #[cfg(feature = "tokio")]
            JoinHandle::Tokio(handle) => Ok(handle.await?),
            #[cfg(feature = "smol")]
            JoinHandle::Smol(handle) => Ok(handle.join().await),
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "smol")]
    use super::SmolRuntime;
    #[cfg(feature = "smol")]
    use std::time::Duration;

    #[test]
    #[cfg(feature = "smol")]
    fn test_smol_runtime() {
        let rt = SmolRuntime::build_with_threads(5);
        let (tx, rx) = oneshot::channel();
        // test detach
        let handle = rt.spawn(async move {
            log::info!("start async");
            smol::Timer::after(Duration::from_millis(10)).await;
            log::info!("finished async");
            tx.send(200).unwrap();
        });
        assert!(!handle.is_finished());
        handle.detach();
        assert_eq!(rx.recv(), Ok(200));

        // test join
        let handle = rt.spawn(async move {
            log::info!("start async");
            smol::Timer::after(Duration::from_millis(10)).await;
            log::info!("finished async");
            200
        });
        assert!(!handle.is_finished());
        rt.block_on(async move {
            assert_eq!(handle.join().await, 200);
        });

        // test abort
        let handle = rt.spawn(async move {
            log::info!("start async");
            smol::Timer::after(Duration::from_millis(100)).await;
            log::info!("finished async");
            200
        });
        assert!(!handle.is_finished());
        rt.block_on(async move {
            assert_eq!(handle.abort().await, None);
        });
    }
}
