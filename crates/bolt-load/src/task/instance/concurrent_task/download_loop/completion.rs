use super::{DownloadLoop, WriteCompletion};
use crate::{
    runner::TaskError,
    task::instance::{Result, TaskInstanceError},
};

pub(super) fn check_completions(completions: Vec<WriteCompletion>) -> Result<usize> {
    let mut written = 0usize;
    for c in &completions {
        match &c.result {
            Ok(()) => {
                written += (c.range.end - c.range.start) as usize;
            }
            Err(e) => {
                return Err(TaskInstanceError::new_failed(TaskError::Other {
                    message: format!("write failed at {:?}: {e}", c.range),
                }));
            }
        }
    }
    Ok(written)
}

impl DownloadLoop {
    pub(super) fn record_completions(&mut self, completions: Vec<WriteCompletion>) -> Result<()> {
        let written = check_completions(completions)?;
        self.throughput.written_bytes += written;
        self.policy.on_write_ok(written);
        Ok(())
    }
}
