use bytes::Bytes;
use futures::future::RemoteHandle;
use smol_cancellation_token::CancellationToken;

use std::path::PathBuf;

use crate::{
    adapter::{AnyAdapter, BoltLoadAdapterMeta, StreamError, UnretryableError},
    runner::TaskFailedKind,
    runtime::ThreadedRuntimeImpl,
};

use super::{DownloadMode, Progress, RunnerId, strategy::Chunk};

mod concurrent_task;
mod singleton_task;

pub use concurrent_task::*;
pub use singleton_task::*;

/// The event of the task
///
/// It is used to push event to the manager or client
#[derive(Debug)]
pub enum TaskEvent {
    /// Initializing the task, including preallocating the file and retrieve the meta
    Initializing,
    /// Downloading the file
    Downloading(Progress),
    /// Failed to download the file
    Failed(TaskError),
    /// Finished downloading the file
    Finished(Progress),
}

struct TaskControl {
    token: CancellationToken,
    handle: Option<RemoteHandle<()>>,
}

impl TaskControl {
    pub fn new(token: CancellationToken, handle: RemoteHandle<()>) -> Self {
        Self {
            token,
            handle: Some(handle),
        }
    }

    pub async fn wait(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.await;
        }
    }

    pub async fn stop(&mut self) {
        self.token.cancel();
        self.wait().await;
    }
}

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

type Result<T, E = TaskError> = std::result::Result<T, E>;

/// The abstract trait of the task
///
/// implemented by singleton task and concurrent task,
///
/// It should be execute at the same task of the `TaskManager`,
/// so we do not have the `Send` and `Sync` for this async fn trait
#[enum_dispatch::enum_dispatch(TaskImpl)]
pub(super) trait Task {
    fn run(
        &mut self,
        adapter: Arc<AnyAdapter>,
        path: PathBuf,
        event_tx: async_channel::Sender<TaskEvent>,
        cancel_token: CancellationToken,
    ) -> Result<()>;
    /// Wait for the task to finish
    async fn wait(&mut self) -> Result<()>;
    async fn stop(&mut self) -> Result<()>;
}

impl TaskImpl {
    pub fn new(mode: DownloadMode, rt: ThreadedRuntimeImpl) -> Self {
        todo!()
    }

    /// get the position of the runner in the file
    pub fn get_runner_pos(&self, runner_id: RunnerId) -> Option<u64> {
        todo!()
    }
}
