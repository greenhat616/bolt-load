use std::{
    pin::Pin,
    task::{Context, Poll, ready},
};

use futures::Stream;

#[cfg(feature = "smol")]
use super::SmolDelayedTimer;
#[cfg(feature = "tokio")]
use super::TokioDelayedTimer;
use super::{Timer, TimerImpl};

/// A periodic timer that is polled in place instead of awaited per tick.
pub(crate) enum EventTimer {
    #[cfg(feature = "tokio")]
    Tokio(TokioDelayedTimer),
    #[cfg(feature = "smol")]
    Smol(SmolDelayedTimer),
    /// A custom [`Timer`] only offers `tick`, so its ticks are a stream.
    Custom(Pin<Box<dyn Stream<Item = ()> + Send>>),
}

impl From<TimerImpl> for EventTimer {
    fn from(timer: TimerImpl) -> Self {
        match timer {
            #[cfg(feature = "tokio")]
            TimerImpl::Tokio(timer) => Self::Tokio(timer),
            #[cfg(feature = "smol")]
            TimerImpl::Smol(timer) => Self::Smol(timer),
            TimerImpl::Custom(timer) => Self::Custom(Box::pin(futures::stream::unfold(
                timer,
                |mut timer| async move {
                    timer.tick().await;
                    Some(((), timer))
                },
            ))),
        }
    }
}

impl EventTimer {
    /// Poll for the next tick.
    #[inline]
    pub(crate) fn poll_tick(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        match self {
            #[cfg(feature = "tokio")]
            Self::Tokio(timer) => timer.poll_tick(cx),
            #[cfg(feature = "smol")]
            Self::Smol(timer) => timer.poll_tick(cx),
            Self::Custom(ticks) => match ready!(ticks.as_mut().poll_next(cx)) {
                Some(()) => Poll::Ready(()),
                // A busy loop on a finished timer would starve every other source.
                None => unreachable!("unfold over a periodic timer never ends"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use super::*;
    use crate::runtime::DynTimer;

    /// Ticks once per permit.
    struct PermitTimer(async_channel::Receiver<()>);

    impl Timer for PermitTimer {
        async fn tick(&mut self) {
            self.0.recv().await.unwrap();
        }
    }

    /// The first tick is due at once, the next one a period later.
    async fn assert_ticks_once_then_waits(mut timer: EventTimer) {
        let tick = std::pin::pin!(std::future::poll_fn(|cx| timer.poll_tick(cx)));
        let watchdog = async_io::Timer::after(Duration::from_secs(5));
        let ticked = matches!(
            futures::future::select(tick, watchdog).await,
            futures::future::Either::Left(_)
        );
        assert!(ticked, "the first tick is due at once");
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(
            timer.poll_tick(&mut cx).is_pending(),
            "the next tick waits a period"
        );
    }

    #[test]
    fn test_native_timers_tick_immediately_then_periodically() {
        #[cfg(feature = "tokio")]
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let timer = TokioDelayedTimer::new(Duration::from_secs(3600));
                assert_ticks_once_then_waits(TimerImpl::Tokio(timer).into()).await;
            });
        #[cfg(feature = "smol")]
        futures::executor::block_on(async {
            let timer = SmolDelayedTimer::new(Duration::from_secs(3600));
            assert_ticks_once_then_waits(TimerImpl::Smol(timer).into()).await;
        });
    }

    #[test]
    fn test_custom_timer_wakes_and_ticks_per_permit() {
        #[derive(Default)]
        struct WakeCount(AtomicUsize);
        impl futures::task::ArcWake for WakeCount {
            fn wake_by_ref(this: &Arc<Self>) {
                this.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let wakes = Arc::new(WakeCount::default());
        let waker = futures::task::waker(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        let (permits, rx) = async_channel::unbounded();
        let mut timer = EventTimer::from(TimerImpl::Custom(DynTimer::new_box(PermitTimer(rx))));

        assert!(timer.poll_tick(&mut cx).is_pending());
        for _ in 0..3 {
            let seen = wakes.0.load(Ordering::SeqCst);
            permits.try_send(()).unwrap();
            assert!(
                wakes.0.load(Ordering::SeqCst) > seen,
                "a permit wakes the poller"
            );
            assert!(timer.poll_tick(&mut cx).is_ready());
            assert!(timer.poll_tick(&mut cx).is_pending());
        }
    }
}
