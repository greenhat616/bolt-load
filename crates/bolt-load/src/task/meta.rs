//! The metadata request shared by the task builder and both task modes.

use std::time::{Duration, Instant};

use backon::{ExponentialBuilder, Retryable};
use bolt_load_utils::telemetry::*;
use futures::FutureExt;

use super::MetaRetry;
use crate::{
    adapter::{AdapterError, AnyAdapter, BoltLoadAdapter, BoltLoadAdapterMeta},
    runtime::{Sleep, ThreadedRuntimeImpl, TimerBuilder, capped_wait, instant_after},
};

/// Retrieve the metadata, retrying [`AdapterError::Retryable`] errors as
/// `retry` allows and returning the last error once the retries run out.
///
/// The backoff sleeps on `rt`'s timer, or on `async-io` when no runtime is set,
/// until an absolute deadline, as a runner's timeouts do. Each delay, the
/// jitter included, is capped at 30 years, the longest the engine waits at
/// once, before it is added to the clock.
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
            Some(rt) => {
                let mut deadline = rt.deadline(instant_after(rt.now(), delay));
                Box::pin(std::future::poll_fn(move |cx| deadline.poll(cx)))
            }
            None => async_io::Timer::at(instant_after(Instant::now(), delay))
                .map(drop)
                .boxed(),
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
        // The jitter is added after `max_delay` caps the base delay. The
        // engine's 30-year cap on a wait applies too, so the delay notified
        // and logged is the one waited for.
        .adjust(move |_, delay| delay.map(|d| capped_wait(d.min(cap))))
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
        BoltLoadAdapter, DynBoltLoadAdapter, FullResponse, RetryableError, UnretryableError,
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
            Ok(BoltLoadAdapterMeta::new(42))
        }

        async fn full_stream(&self) -> Result<FullResponse, AdapterError> {
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

    /// A delay over 30 years, the jitter's included, is notified, and so
    /// logged, as the 30 years it waits.
    #[test]
    fn a_delay_over_30_years_is_notified_as_30_years() {
        use crate::runtime::test_support::{ManualClock, ManualRuntime};

        const YEAR: Duration = Duration::from_secs(86400 * 365);
        let rt = ThreadedRuntimeImpl::new_other_rt(ManualRuntime {
            clock: ManualClock::new(),
        });
        let retry = MetaRetry::default()
            .max_retries(1)
            .min_delay(100 * YEAR)
            .max_delay(Duration::MAX);
        let (adapter, _) = FlakyAdapter::new(1, true);
        let adapter = DynBoltLoadAdapter::new_box(adapter);
        let mut delays = Vec::new();
        {
            let waker = futures::task::noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            let mut meta =
                std::pin::pin!(retrieve_meta_with(&adapter, &retry, Some(&rt), |_, d| {
                    delays.push(d)
                }));
            assert!(meta.as_mut().poll(&mut cx).is_pending());
        }
        assert_eq!(delays, [30 * YEAR]);
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
        /// Its clock stands still, so the wall clock cannot pass a retry's
        /// deadline before its sleep ticks.
        struct TimerOnlyRuntime(Arc<AtomicUsize>, std::time::Instant);
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

            fn now(&self) -> std::time::Instant {
                self.1
            }
        }
        impl ThreadedRuntime for TimerOnlyRuntime {}

        let ticks = Arc::new(AtomicUsize::new(0));
        let rt = ThreadedRuntimeImpl::new_other_rt(TimerOnlyRuntime(
            ticks.clone(),
            std::time::Instant::now(),
        ));
        let (adapter, calls) = FlakyAdapter::new(2, true);
        let adapter = DynBoltLoadAdapter::new_box(adapter);
        let meta = futures::executor::block_on(retrieve_meta(&adapter, &fast_retry(2), Some(&rt)));
        assert_eq!(meta.unwrap().content_size, 42);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(ticks.load(Ordering::SeqCst), 4, "two ticks per retry");
    }

    /// A retry delay too long for an `Instant`, `Duration::MAX`, waits 30
    /// years on the runtime's clock, the engine's cap on any wait, instead
    /// of handing the runtime a sleep it cannot add to its clock.
    #[test]
    fn a_retry_delay_past_what_an_instant_holds_waits_30_years() {
        use crate::runtime::test_support::{ManualClock, ManualRuntime};

        const THIRTY_YEARS: Duration = Duration::from_secs(86400 * 365 * 30);
        let clock = ManualClock::new();
        let rt = ThreadedRuntimeImpl::new_other_rt(ManualRuntime {
            clock: clock.clone(),
        });
        let retry = MetaRetry::default()
            .max_retries(1)
            .min_delay(Duration::MAX)
            .max_delay(Duration::MAX);
        let (adapter, calls) = FlakyAdapter::new(1, true);
        let adapter = DynBoltLoadAdapter::new_box(adapter);
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut meta = std::pin::pin!(retrieve_meta(&adapter, &retry, Some(&rt)));
        assert!(meta.as_mut().poll(&mut cx).is_pending());
        clock.advance(THIRTY_YEARS - Duration::from_nanos(1));
        assert!(
            meta.as_mut().poll(&mut cx).is_pending(),
            "a nanosecond early"
        );
        clock.advance(Duration::from_nanos(1));
        let std::task::Poll::Ready(meta) = meta.as_mut().poll(&mut cx) else {
            panic!("not retried 30 years on");
        };
        assert_eq!(meta.unwrap().content_size, 42);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// A retry delay longer than 30 years, one an `Instant` holds, waits 30
    /// years on tokio: the engine's documented cap on any wait, applied
    /// before tokio sees the deadline, not tokio's own cap on relative
    /// sleeps.
    #[cfg(feature = "tokio")]
    #[test]
    fn a_retry_delay_past_30_years_waits_30_years() {
        use crate::runtime::TokioThreadedRuntime;

        const DELAY: Duration = Duration::from_secs(86400 * 365 * 31);
        const CAP: Duration = Duration::from_secs(86400 * 365 * 30);
        let paused = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        let rt = ThreadedRuntimeImpl::Tokio(TokioThreadedRuntime::Handle(paused.handle().clone()));
        let retry = MetaRetry::default()
            .max_retries(1)
            .min_delay(DELAY)
            .max_delay(DELAY);
        let (adapter, calls) = FlakyAdapter::new(1, true);
        let adapter = DynBoltLoadAdapter::new_box(adapter);
        let start = rt.now();
        let meta = paused.block_on(retrieve_meta(&adapter, &retry, Some(&rt)));
        let waited = rt.now() - start;
        assert_eq!(meta.unwrap().content_size, 42);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(
            waited >= CAP && waited < CAP + Duration::from_millis(10),
            "{waited:?}"
        );
    }

    /// A generated delay over 30 years, the jitter's included, is capped
    /// before a custom runtime's default `sleep_until` turns the rest into a
    /// relative sleep: on a clock that moves 20 years as that sleep starts,
    /// with 160 years left, a 150-year delay neither overflows the clock
    /// nor outlives its 30-year cap.
    #[test]
    fn a_generated_delay_is_capped_before_the_default_sleep_until() {
        use crate::runtime::test_support::{ManualClock, SlippingRuntime, last_instant};

        const YEAR: Duration = Duration::from_secs(86400 * 365);
        let clock = ManualClock::starting_at(last_instant(std::time::Instant::now()) - 160 * YEAR);
        let t0 = clock.now();
        let rt = ThreadedRuntimeImpl::new_other_rt(SlippingRuntime {
            clock: clock.clone(),
            slip: 20 * YEAR,
            absolute: false,
        });
        let retry = MetaRetry::default()
            .max_retries(1)
            .min_delay(150 * YEAR)
            .max_delay(150 * YEAR);
        let (adapter, calls) = FlakyAdapter::new(1, true);
        let adapter = DynBoltLoadAdapter::new_box(adapter);
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut meta = std::pin::pin!(retrieve_meta(&adapter, &retry, Some(&rt)));
        assert!(meta.as_mut().poll(&mut cx).is_pending());
        assert_eq!(clock.now(), t0 + 20 * YEAR, "the sleep started late");
        clock.advance(10 * YEAR - Duration::from_nanos(1));
        assert!(
            meta.as_mut().poll(&mut cx).is_pending(),
            "a nanosecond before 30 years"
        );
        clock.advance(Duration::from_nanos(1));
        let std::task::Poll::Ready(meta) = meta.as_mut().poll(&mut cx) else {
            panic!("not retried 30 years on");
        };
        assert_eq!(meta.unwrap().content_size, 42);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
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
