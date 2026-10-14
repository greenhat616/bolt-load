//! The metadata request shared by the task builder and both task modes.

use std::time::Duration;

use backon::{ExponentialBuilder, Retryable};
use bolt_load_utils::telemetry::*;
use futures::FutureExt;

use super::MetaRetry;
use crate::{
    adapter::{AdapterError, AnyAdapter, BoltLoadAdapter, BoltLoadAdapterMeta},
    runtime::{Sleep, ThreadedRuntimeImpl, TimerBuilder},
};

/// Retrieve the metadata, retrying [`AdapterError::Retryable`] errors as
/// `retry` allows and returning the last error once the retries run out.
///
/// The backoff sleeps on `rt`'s timer, or on `async-io` when no runtime is set.
pub(crate) async fn retrieve_meta(
    adapter: &AnyAdapter,
    retry: &MetaRetry,
    rt: Option<&ThreadedRuntimeImpl>,
) -> Result<BoltLoadAdapterMeta, AdapterError> {
    retrieve_meta_with(adapter, retry, rt, |_, _| {}).await
}

async fn retrieve_meta_with(
    adapter: &AnyAdapter,
    retry: &MetaRetry,
    rt: Option<&ThreadedRuntimeImpl>,
    mut notify: impl FnMut(&AdapterError, Duration),
) -> Result<BoltLoadAdapterMeta, AdapterError> {
    let rt = rt.cloned();
    let sleep = move |delay: Duration| -> Sleep {
        match &rt {
            Some(rt) => rt.sleep(delay),
            None => async_io::Timer::after(delay).map(drop).boxed(),
        }
    };
    let cap = retry.max_delay;
    (|| adapter.retrieve_meta())
        .retry(
            ExponentialBuilder::default()
                .with_min_delay(retry.min_delay)
                .with_max_delay(retry.max_delay)
                .with_max_times(retry.max_retries)
                .with_jitter(),
        )
        .sleep(sleep)
        .when(|e| matches!(e, AdapterError::Retryable { .. }))
        // The jitter is added after `max_delay` caps the base delay.
        .adjust(move |_, delay| delay.map(|d| d.min(cap)))
        .notify(|e, delay| {
            // The error text may carry the request URL and its credentials.
            trace!("[TASK] retrieve_meta failed with a retryable error, retrying in {delay:?}");
            notify(e, delay);
        })
        .await
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use crate::adapter::{
        AnyBytesStream, BoltLoadAdapter, DynBoltLoadAdapter, RetryableError, UnretryableError,
    };

    /// Fails `failures` times with `error`, then succeeds.
    struct FlakyAdapter {
        calls: Arc<AtomicUsize>,
        failures: usize,
        retryable: bool,
    }

    impl FlakyAdapter {
        fn new(failures: usize, retryable: bool) -> (Self, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            let adapter = Self {
                calls: calls.clone(),
                failures,
                retryable,
            };
            (adapter, calls)
        }
    }

    impl BoltLoadAdapter for FlakyAdapter {
        async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call < self.failures {
                let message = format!("failure {call}");
                return Err(if self.retryable {
                    RetryableError::Io {
                        source: Arc::new(std::io::Error::other(message)),
                    }
                    .into()
                } else {
                    AdapterError::Unretryable {
                        source: UnretryableError::Internal { message },
                    }
                });
            }
            Ok(BoltLoadAdapterMeta {
                content_size: 42,
                filename: None,
            })
        }

        async fn full_stream(&self) -> Result<AnyBytesStream, AdapterError> {
            unreachable!("only the metadata is requested")
        }
    }

    fn fast_retry(max_retries: usize) -> MetaRetry {
        MetaRetry::default()
            .max_retries(max_retries)
            .min_delay(Duration::from_millis(1))
            .max_delay(Duration::from_millis(10))
    }

    async fn run(adapter: FlakyAdapter, retry: &MetaRetry) -> Result<u64, AdapterError> {
        let adapter = DynBoltLoadAdapter::new_box(adapter);
        retrieve_meta(&adapter, retry, None)
            .await
            .map(|meta| meta.content_size)
    }

    #[tokio::test]
    async fn retryable_then_success() {
        let (adapter, calls) = FlakyAdapter::new(2, true);
        assert_eq!(run(adapter, &fast_retry(3)).await.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn unretryable_fails_fast() {
        let (adapter, calls) = FlakyAdapter::new(1, false);
        let err = run(adapter, &fast_retry(3)).await.unwrap_err();
        assert!(matches!(err, AdapterError::Unretryable { .. }));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn exhausted_reports_last_error() {
        let (adapter, calls) = FlakyAdapter::new(usize::MAX, true);
        let err = run(adapter, &fast_retry(2)).await.unwrap_err();
        assert!(err.to_string().contains("failure 2"), "{err}");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn zero_retries_calls_once() {
        let (adapter, calls) = FlakyAdapter::new(usize::MAX, true);
        assert!(run(adapter, &fast_retry(0)).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn delay_is_capped_after_jitter() {
        let (adapter, _) = FlakyAdapter::new(usize::MAX, true);
        let adapter = DynBoltLoadAdapter::new_box(adapter);
        // A min delay equal to the max lets the jitter push every wait past the cap.
        let retry = MetaRetry::default()
            .max_retries(8)
            .min_delay(Duration::from_millis(2))
            .max_delay(Duration::from_millis(2));
        let mut delays = Vec::new();
        let _ = retrieve_meta_with(&adapter, &retry, None, |_, d| delays.push(d)).await;
        assert_eq!(delays.len(), 8);
        assert!(delays.iter().all(|d| *d <= retry.max_delay), "{delays:?}");
    }

    /// Polled outside any tokio context, so the backoff must be registered
    /// with the runtime's own timer.
    #[cfg(feature = "tokio")]
    #[test]
    fn sleeps_on_the_tokio_timer_outside_its_context() {
        use crate::runtime::TokioThreadedRuntime;

        let (adapter, calls) = FlakyAdapter::new(1, true);
        let adapter = DynBoltLoadAdapter::new_box(adapter);
        let rt = ThreadedRuntimeImpl::Tokio(TokioThreadedRuntime::new());
        assert!(tokio::runtime::Handle::try_current().is_err());
        let meta = futures::executor::block_on(retrieve_meta(&adapter, &fast_retry(1), Some(&rt)));
        assert_eq!(meta.unwrap().content_size, 42);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// A custom runtime's default sleep ticks its delayed timer twice.
    #[test]
    fn sleeps_on_a_custom_runtime_timer() {
        use crate::runtime::{DowncastLocalRuntime, DynTimer, ThreadedRuntime, Timer, TimerImpl};

        struct CountingTimer(Arc<AtomicUsize>);
        impl Timer for CountingTimer {
            async fn tick(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        struct TimerOnlyRuntime(Arc<AtomicUsize>);
        impl futures::task::Spawn for TimerOnlyRuntime {
            fn spawn_obj(
                &self,
                _: futures::future::FutureObj<'static, ()>,
            ) -> Result<(), futures::task::SpawnError> {
                Err(futures::task::SpawnError::shutdown())
            }
        }
        impl DowncastLocalRuntime for TimerOnlyRuntime {}
        impl TimerBuilder for TimerOnlyRuntime {
            fn create_delayed_timer(&self, _: Duration) -> TimerImpl {
                TimerImpl::Custom(DynTimer::new_box(CountingTimer(self.0.clone())))
            }
        }
        impl ThreadedRuntime for TimerOnlyRuntime {}

        let ticks = Arc::new(AtomicUsize::new(0));
        let rt = ThreadedRuntimeImpl::new_other_rt(TimerOnlyRuntime(ticks.clone()));
        let (adapter, calls) = FlakyAdapter::new(2, true);
        let adapter = DynBoltLoadAdapter::new_box(adapter);
        let meta = futures::executor::block_on(retrieve_meta(&adapter, &fast_retry(2), Some(&rt)));
        assert_eq!(meta.unwrap().content_size, 42);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(ticks.load(Ordering::SeqCst), 4, "two ticks per retry");
    }

    #[cfg(feature = "smol")]
    #[test]
    fn sleeps_on_the_smol_timer() {
        let (adapter, calls) = FlakyAdapter::new(1, true);
        let adapter = DynBoltLoadAdapter::new_box(adapter);
        let rt = ThreadedRuntimeImpl::new_smol_rt();
        let meta = futures::executor::block_on(retrieve_meta(&adapter, &fast_retry(1), Some(&rt)));
        assert_eq!(meta.unwrap().content_size, 42);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
