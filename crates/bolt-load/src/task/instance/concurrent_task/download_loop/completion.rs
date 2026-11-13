use std::task::{Context, Poll};

use super::{DownloadLoop, LoopEvent, WriteCompletion};
use crate::{
    runner::TaskError,
    task::{DownloadError, instance::Result},
};

pub(super) fn check_completions(completions: Vec<WriteCompletion>) -> Result<usize> {
    let mut written = 0usize;
    for c in &completions {
        match &c.result {
            Ok(()) => {
                written += (c.range.end - c.range.start) as usize;
            }
            Err(e) => {
                return Err(DownloadError::transfer(TaskError::Other {
                    message: format!("write failed at {:?}: {e}", c.range),
                }));
            }
        }
    }
    Ok(written)
}

impl DownloadLoop {
    /// Pending while no write is in flight: only the loop itself dispatches
    /// writes, and it polls again after handling the event that did.
    #[inline]
    pub(super) fn poll_writer(&mut self, cx: &mut Context<'_>) -> Poll<LoopEvent> {
        self.writer
            .poll_completions(cx)
            .map(LoopEvent::WriterCompleted)
    }

    pub(super) fn record_completions(&mut self, completions: Vec<WriteCompletion>) -> Result<()> {
        let written = check_completions(completions)?;
        self.throughput.record_written(written);
        self.policy.on_write_ok(written);
        Ok(())
    }
}
