//! Where a runner reads its time from.

use std::{fmt, sync::Arc, time::Instant};

use super::{ThreadedRuntime, ThreadedRuntimeImpl};

/// The clock a runner reads, and its timeouts run on: its task's runtime's,
/// or, for a runner built without one, the standard clock.
///
/// It does not own a tokio or smol runtime: it holds a tokio runtime by its
/// handle and a smol runtime by nothing, its timers needing none, so
/// dropping it shuts neither down. A custom runtime it holds by a clone of
/// the `Arc` the runtime is shared in, which its drop may release last.
#[derive(Clone, Default)]
pub(crate) struct TimerSource(Clock);

#[derive(Clone, Default)]
enum Clock {
    #[default]
    Standard,
    #[cfg(feature = "tokio")]
    Tokio(tokio::runtime::Handle),
    #[cfg(feature = "smol")]
    Smol,
    Other(Arc<dyn ThreadedRuntime + Send + Sync + 'static>),
}

impl TimerSource {
    /// The clock and timers of `rt`.
    pub(crate) fn new(rt: &ThreadedRuntimeImpl) -> Self {
        Self(match rt {
            #[cfg(feature = "tokio")]
            ThreadedRuntimeImpl::Tokio(rt) => Clock::Tokio(rt.handle().clone()),
            #[cfg(feature = "smol")]
            ThreadedRuntimeImpl::Smol(_) => Clock::Smol,
            ThreadedRuntimeImpl::Other(rt) => Clock::Other(rt.clone()),
        })
    }

    /// The current time on the source's clock.
    pub(crate) fn now(&self) -> Instant {
        match &self.0 {
            Clock::Standard => Instant::now(),
            #[cfg(feature = "tokio")]
            Clock::Tokio(handle) => super::tokio_impl::now_on(handle),
            #[cfg(feature = "smol")]
            Clock::Smol => Instant::now(),
            Clock::Other(rt) => rt.now(),
        }
    }
}

impl fmt::Debug for TimerSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            Clock::Standard => "TimerSource(standard clock)",
            _ => "TimerSource(runtime)",
        })
    }
}
