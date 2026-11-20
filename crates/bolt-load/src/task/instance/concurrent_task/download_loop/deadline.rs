use std::{
    task::{Context, Poll},
    time::Instant,
};

use super::{DownloadLoop, LoopEvent};
use crate::{
    runtime::{Deadline, ThreadedRuntimeImpl},
    task::{DownloadError, instance::Result},
};

/// The loop's timer, on the task's runtime: the no-progress deadline, or
/// the end of a backoff when that comes first (design §8.1).
///
/// Re-armed lazily when progress moved the deadline, and at once when a
/// backoff ends earlier than it is armed for.
pub(super) struct DeadlineSource {
    timer: Deadline,
}

impl DeadlineSource {
    pub(super) fn new(rt: &ThreadedRuntimeImpl, at: Instant) -> Self {
        Self {
            timer: rt.deadline(at),
        }
    }

    /// Fires once until re-armed: [`DownloadLoop::on_deadline`] re-arms it,
    /// or ends the loop.
    #[inline]
    pub(super) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<LoopEvent> {
        self.timer.poll(cx).map(|()| LoopEvent::Deadline)
    }

    /// When the timer fires.
    #[cfg(test)]
    pub(super) fn at(&self) -> Instant {
        self.timer.at()
    }

    /// Fire at `at` at the latest.
    pub(super) fn wake_by(&mut self, at: Instant) {
        if at < self.timer.at() {
            self.set(at);
        }
    }

    fn set(&mut self, at: Instant) {
        self.timer.reset(at);
    }
}

impl DownloadLoop {
    /// Fail without progress; otherwise create runners for the ranges whose
    /// backoff ended, and re-arm for the next deadline or backoff end.
    ///
    /// Only instants after `now` are armed, so a range that could not get
    /// a runner, for want of a free slot, never makes the loop spin: the
    /// strategy tick retries it.
    pub(super) fn on_deadline(&mut self, now: Instant) -> Result<()> {
        if let Some(e) = self.policy.check(now) {
            return Err(DownloadError::transfer(e));
        }
        self.retry_backed_off(now);
        let deadline = self.policy.deadline();
        let next = self
            .next_retry(now)
            .map_or(deadline, |retry| retry.min(deadline));
        self.deadline.set(next);
        Ok(())
    }
}
