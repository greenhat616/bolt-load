use std::pin::Pin;

use crate::{
    adapter::{AnyAdapter, BoltLoadAdapterMeta, UnretryableError},
    runtime::Runtime,
};

use super::{DownloadMode, Progress, RunnerId, strategy::Chunk};

use bytes::Bytes;

mod concurrent_task;
mod singleton_task;

pub use concurrent_task::*;
pub use singleton_task::*;

pub enum TaskImpl {
    Singleton(SingletonTask),
    Concurrent(ConcurrentTask),
}

#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error("retrieve meta failed: {0}")]
    RetrieveMetaFailed(UnretryableError),
    #[error("failed to fetch stream failed: {0}")]
    StreamFailed(UnretryableError),
}

type Result<T> = std::result::Result<T, TaskError>;
/// the future of the task, should be polled by the Manager
type TaskFuture = Pin<Box<dyn Future<Output = Result<()>>>>;

/// The abstract trait of the task
///
/// implemented by singleton task and concurrent task,
///
/// It should be execute at the same task of the `TaskManager`,
/// so we do not have the `Send` and `Sync` for this async fn trait
pub(super) trait Task {
    async fn start(&mut self, adapter: &AnyAdapter) -> Result<TaskFuture>;
    async fn stop(&mut self) -> Result<()>;
    fn inspect_progress(&self) -> Progress;
}

impl TaskImpl {
    pub fn new<T>(mode: DownloadMode, rt: Runtime, on_chunk_downloaded: T) -> Self
    where
        T: Fn(Chunk, Bytes),
    {
        todo!()
    }

    /// get the position of the runner in the file
    pub fn get_runner_pos(&self, runner_id: RunnerId) -> Option<u64> {
        todo!()
    }
}
