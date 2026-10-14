use std::{
    pin::Pin,
    task::{Context, Poll},
    time::Instant,
};

use super::{DownloadLoop, LoopEvent};
use crate::task::instance::{Result, TaskInstanceError};

/// The no-progress deadline; re-armed lazily when progress moved it.
pub(super) struct DeadlineSource {
    timer: async_io::Timer,
}

impl DeadlineSource {
    pub(super) fn new(at: Instant) -> Self {
        Self {
            timer: async_io::Timer::at(at),
        }
    }

    #[inline]
    pub(super) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<LoopEvent> {
        Pin::new(&mut self.timer)
            .poll(cx)
            .map(|_| LoopEvent::Deadline)
    }
}

impl DownloadLoop {
    pub(super) fn on_deadline(&mut self) -> Result<()> {
        if let Some(e) = self.policy.check(Instant::now()) {
            return Err(TaskInstanceError::new_failed(e));
        }
        self.deadline.timer.set_at(self.policy.deadline());
        Ok(())
    }
}
