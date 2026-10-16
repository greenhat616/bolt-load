use std::time::Instant;

use super::DownloadLoop;
use crate::task::instance::{Result, TaskInstanceError};

impl DownloadLoop {
    pub(super) fn on_deadline(&mut self) -> Result<()> {
        if let Some(e) = self.policy.check(Instant::now()) {
            return Err(TaskInstanceError::new_failed(e));
        }
        self.deadline_timer.set_at(self.policy.deadline());
        Ok(())
    }
}
