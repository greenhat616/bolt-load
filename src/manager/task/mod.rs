use bytes::Bytes;
use smol_cancellation_token::CancellationToken;

use std::{pin::Pin, rc::Rc};

use crate::{
    adapter::{AnyAdapter, BoltLoadAdapterMeta, StreamError, UnretryableError},
    runner::TaskFailedKind,
    runtime::Runtime,
};

use super::{DownloadMode, Progress, RunnerId, strategy::Chunk};

mod concurrent_task;
mod singleton_task;

pub use concurrent_task::*;
pub use singleton_task::*;

#[enum_dispatch::enum_dispatch]
pub enum TaskImpl {
    Singleton(SingletonTask),
    Concurrent(ConcurrentTask),
}

#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error("retrieve meta failed: {0}")]
    RetrieveMetaFailed(UnretryableError),
    #[error("failed to fetch stream failed: {0}")]
    StreamFailed(StreamError),
    #[error("task failed: {0:?}")]
    Failed(TaskFailedKind),
    #[error("failed to write chunk: {0}")]
    WriteChunkFailed(std::io::Error),
}

type Result<T> = std::result::Result<T, TaskError>;

/// The abstract trait of the task
///
/// implemented by singleton task and concurrent task,
///
/// It should be execute at the same task of the `TaskManager`,
/// so we do not have the `Send` and `Sync` for this async fn trait
#[enum_dispatch::enum_dispatch(TaskImpl)]
pub(super) trait Task {
    async fn start(
        &mut self,
        adapter: &AnyAdapter,
        cancel_token: CancellationToken,
    ) -> Result<()>;
    async fn stop(&mut self) -> Result<()>;
    fn inspect_progress(&self) -> Progress;
}

impl TaskImpl {
    pub fn new(mode: DownloadMode, rt: Runtime) -> Self {
        todo!()
    }

    /// get the position of the runner in the file
    pub fn get_runner_pos(&self, runner_id: RunnerId) -> Option<u64> {
        todo!()
    }
}
