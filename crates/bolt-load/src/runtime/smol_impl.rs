use std::{pin::Pin, rc::Rc, sync::Arc, time::Duration};

use bolt_load_utils::telemetry::*;
use futures::task::{LocalFutureObj, LocalSpawn, Spawn, SpawnError};
use smol::future::FutureExt;

use super::{DowncastLocalRuntime, LocalRuntimeImpl, Timer, TimerBuilder, TimerImpl};

/// SmolRuntime is a dead simple runtime for smol.
/// It is used to run tasks via by `bolt-load` client.
#[derive(Clone)]
pub struct SmolThreadedRuntime {
    executor: Arc<smol::Executor<'static>>,
    shutdown: Arc<ShutdownOnDrop>,
}

/// Stops the executor threads once the last runtime clone is dropped.
struct ShutdownOnDrop(async_broadcast::Sender<()>);

impl Drop for ShutdownOnDrop {
    fn drop(&mut self) {
        let _ = self.0.try_broadcast(());
    }
}

impl TimerBuilder for SmolThreadedRuntime {
    fn create_delayed_timer(&self, duration: Duration) -> TimerImpl {
        TimerImpl::Smol(SmolDelayedTimer::new(duration))
    }
}

pub struct SmolJoinHandle<T> {
    task: smol::Task<T>,
}

impl SmolThreadedRuntime {
    pub fn build_with_threads(threads: usize) -> Self {
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
                        smol::Timer::after(Duration::from_millis(100)).await;
                    };

                    smol::future::block_on(executor.run(executor.tick().or(timer)));
                }
            });
        }
        Self {
            executor,
            shutdown: Arc::new(ShutdownOnDrop(shutdown_tx)),
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

    /// Stop the executor threads of this runtime and all of its clones.
    pub fn shutdown(&self) {
        let _ = self.shutdown.0.try_broadcast(());
    }
}

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

impl Spawn for SmolThreadedRuntime {
    #[track_caller]
    fn spawn_obj(&self, future: futures::future::FutureObj<'static, ()>) -> Result<(), SpawnError> {
        self.executor.spawn(future).detach();
        Ok(())
    }
}

impl DowncastLocalRuntime for SmolThreadedRuntime {
    fn downcast_local_runtime(&self) -> Option<LocalRuntimeImpl> {
        Some(LocalRuntimeImpl::Smol(SmolLocalRuntime::new()))
    }
}

#[derive(Clone)]
pub struct SmolLocalRuntime {
    rt: Rc<smol::LocalExecutor<'static>>,
}

impl TimerBuilder for SmolLocalRuntime {
    fn create_delayed_timer(&self, duration: Duration) -> TimerImpl {
        TimerImpl::Smol(SmolDelayedTimer::new(duration))
    }
}

impl Default for SmolLocalRuntime {
    fn default() -> Self {
        Self::new()
    }
}

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

pub struct SmolDelayedTimer {
    timer: smol::Timer,
    duration: Duration,
}

impl SmolDelayedTimer {
    /// The first tick completes immediately, like `tokio::time::interval`.
    pub fn new(duration: Duration) -> Self {
        Self {
            timer: smol::Timer::at(std::time::Instant::now()),
            duration,
        }
    }
}

pin_project_lite::pin_project! {
    pub struct SmolDelayedTick<'a> {
        #[pin]
        timer: Pin<&'a mut smol::Timer>,
        duration: Duration,
    }
}

impl<'a> Future for SmolDelayedTick<'a> {
    type Output = ();
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut this = self.project();

        match this.timer.as_mut().poll(cx) {
            std::task::Poll::Ready(_) => {
                this.timer.set_after(*this.duration);
                std::task::Poll::Ready(())
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl Timer for SmolDelayedTimer {
    async fn tick(&mut self) {
        let fut = SmolDelayedTick {
            timer: Pin::new(&mut self.timer),
            duration: self.duration,
        };
        fut.await
    }
}

impl LocalSpawn for SmolLocalRuntime {
    #[track_caller]
    fn spawn_local_obj(&self, future: LocalFutureObj<'static, ()>) -> Result<(), SpawnError> {
        self.rt.spawn(future).detach();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
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

    #[test]
    fn test_delayed_timer_ticks_immediately_then_periodically() {
        use futures::FutureExt;

        let mut timer = SmolDelayedTimer::new(Duration::from_secs(3600));
        assert!(
            timer.tick().now_or_never().is_some(),
            "first tick must be immediate"
        );
        assert!(
            timer.tick().now_or_never().is_none(),
            "later ticks wait a period"
        );
    }

    #[test]
    fn test_dropping_a_clone_keeps_the_executor_running() {
        let rt = SmolThreadedRuntime::build_with_threads(2);
        drop(rt.clone());
        // Executor threads poll for shutdown every 100 ms.
        std::thread::sleep(Duration::from_millis(300));
        let (tx, rx) = oneshot::channel();
        rt.spawn(async move { tx.send(()).unwrap() }).detach();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(()));
    }
}
