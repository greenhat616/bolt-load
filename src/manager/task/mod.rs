use crate::runtime::Runtime;

use super::{DownloadMode, RunnerId};

mod concurrent_task;
mod singleton_task;

pub use concurrent_task::*;
pub use singleton_task::*;

pub enum Task {
    Singleton(SingletonTask),
    Concurrent(ConcurrentTask),
}

impl Task {
    pub fn new(mode: DownloadMode, rt: Runtime) -> Self {
        todo!()
    }

    /// get the position of the runner in the file
    pub fn get_runner_pos(&self, runner_id: RunnerId) -> Option<u64> {
        todo!()
    }
}
