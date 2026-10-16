use std::{ops::ControlFlow, time::Instant};

use super::{
    super::{file_writer::WriterFullError, runner_manager::TaskState},
    DownloadLoop, RunnerTick,
};
use crate::{
    runner::TaskError,
    task::instance::{Result, TaskInstanceError},
};

impl DownloadLoop {
    pub(super) fn on_runner(&mut self, tick: RunnerTick) -> Result<ControlFlow<()>> {
        if let Some(chunk) = tick.downloaded {
            self.writer
                .write_range(chunk.range, chunk.bytes)
                .map_err(|e: WriterFullError| {
                    TaskInstanceError::new_failed(TaskError::Other {
                        message: format!(
                            "writer full (invariant violation): {:?}",
                            e.pending_write.range
                        ),
                    })
                })?;
            self.policy
                .on_bytes_accepted(self.manager.get_total_downloaded(), Instant::now());
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
}
