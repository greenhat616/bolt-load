use std::{
    ops::ControlFlow,
    task::{Context, Poll},
    time::Instant,
};

use super::{
    super::{
        file_writer::WriterFullError,
        runner_manager::{DownloadedChunk, TaskState},
    },
    DownloadLoop, LoopEvent, RunnerTick,
};
use crate::{
    runner::TaskError,
    task::instance::{Result, TaskInstanceError},
};

impl DownloadLoop {
    /// Runner data is only polled while the writer can take it; their
    /// lifecycle always is, so failures are noticed under backpressure.
    #[inline]
    pub(super) fn poll_runner(&mut self, cx: &mut Context<'_>) -> Poll<LoopEvent> {
        let data_enabled = self.writer.can_write();
        self.manager
            .poll_tick(cx, data_enabled)
            .map(LoopEvent::Runner)
    }

    pub(super) fn on_runner(&mut self, tick: RunnerTick) -> Result<ControlFlow<()>> {
        if let Some(chunk) = tick.downloaded {
            self.accept_chunk(chunk)?;
        }
        if let Some(failure) = tick.failure
            && let Some(e) = self
                .policy
                .on_failure(&failure, self.manager.allocated_runner_count())
        {
            return Err(TaskInstanceError::new_failed(e));
        }
        Ok(if tick.state == TaskState::Finished {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        })
    }

    /// Write a chunk the planner accepted; a non-empty one is progress.
    fn accept_chunk(&mut self, chunk: DownloadedChunk) -> Result<()> {
        let progressed = !chunk.range.is_empty();
        self.writer
            .write_range(chunk.range, chunk.bytes)
            .map_err(writer_full)?;
        if progressed {
            self.policy.on_progress(Instant::now());
        }
        Ok(())
    }
}

#[cold]
fn writer_full(e: WriterFullError) -> TaskInstanceError {
    TaskInstanceError::new_failed(TaskError::Other {
        message: format!(
            "writer full (invariant violation): {:?}",
            e.pending_write.range
        ),
    })
}
