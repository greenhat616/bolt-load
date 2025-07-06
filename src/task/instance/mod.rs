use std::{path::PathBuf, sync::Arc};

use smol_cancellation_token::CancellationToken;

use super::{DownloadMode, Progress};
use crate::{
    adapter::{AnyAdapter, StreamError, UnretryableError},
    runner::TaskFailedKind,
    runtime::{LocalRuntimeBuilderImpl, ThreadedRuntimeImpl},
};

mod concurrent_task;
mod id;
mod sampler;
mod singleton_task;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod integration_test;

pub use concurrent_task::*;
use id::Generator;
pub use singleton_task::*;

/// The default capacity of the control channel
///
/// It is used to control the task runner
const DEFAULT_CONTROL_CHANNEL_CAPACITY: usize = 32;

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProgressWithSpeed {
    #[cfg_attr(feature = "serde", serde(flatten))]
    progress: Progress,
    speed: f64,
}

impl ProgressWithSpeed {
    pub fn new(progress: Progress, speed: f64) -> Self {
        Self { progress, speed }
    }

    /// Get the progress
    pub fn progress(&self) -> &Progress {
        &self.progress
    }

    /// Get the speed in bytes per second
    pub fn speed(&self) -> f64 {
        self.speed
    }

    /// Get the total size
    pub fn total(&self) -> Option<u64> {
        self.progress.total
    }

    /// Get the downloaded size
    pub fn downloaded(&self) -> u64 {
        self.progress.downloaded
    }

    /// Get the downloaded chunks
    pub fn downloaded_chunks(&self) -> &[std::ops::Range<u64>] {
        &self.progress.downloaded_chunks
    }
}

/// The event of the task
///
/// It is used to push event to the manager or client
#[derive(Debug, Clone)]
pub enum TaskEvent {
    /// Initializing the task, including preallocating the file and retrieve the meta
    Initializing,
    /// Downloading the file
    Downloading(ProgressWithSpeed),
    /// Failed to download the file
    Failed(TaskInstanceError),
    /// Finished downloading the file
    Finished(Progress),
}

struct TaskControl {
    token: CancellationToken,
    handle: Option<blocking::Task<()>>,
}

impl TaskControl {
    pub fn new(token: CancellationToken, task: blocking::Task<()>) -> Self {
        Self {
            token,
            handle: Some(task),
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

// TODO: maybe we can share the same state machine for both task impl?
#[enum_dispatch::enum_dispatch]
pub enum TaskInstanceImpl {
    Singleton(SingletonTask),
    Concurrent(ConcurrentTask),
}

#[derive(Debug, thiserror::Error, Clone)]
pub enum TaskInstanceError {
    #[error("retrieve meta failed: {0}")]
    RetrieveMetaFailed(UnretryableError),
    #[error("failed to allocate file size: {0}")]
    AllocateFileSpaceFailed(Arc<std::io::Error>),
    #[error("failed to fetch stream failed: {0}")]
    StreamFailed(StreamError),
    #[error("task failed: {0:?}")]
    Failed(TaskFailedKind),
    #[error("failed to write chunk: {0}")]
    WriteChunkFailed(Arc<std::io::Error>),
}

impl TaskInstanceError {
    pub fn new_retrieve_meta_failed(e: UnretryableError) -> Self {
        Self::RetrieveMetaFailed(e)
    }

    pub fn new_stream_failed(e: StreamError) -> Self {
        Self::StreamFailed(e)
    }

    pub fn new_failed(e: TaskFailedKind) -> Self {
        Self::Failed(e)
    }

    pub fn new_write_chunk_failed(e: std::io::Error) -> Self {
        Self::WriteChunkFailed(Arc::new(e))
    }

    pub fn new_allocate_file_size_failed(e: std::io::Error) -> Self {
        Self::AllocateFileSpaceFailed(Arc::new(e))
    }
}

type Result<T, E = TaskInstanceError> = std::result::Result<T, E>;

#[derive(Clone)]
pub struct RunningPayload {
    adapter: Arc<AnyAdapter>,
    cancel_token: CancellationToken,
}

/// The abstract trait of the task
///
/// implemented by singleton task and concurrent task,
///
/// It should be execute at the same task of the `TaskManager`,
/// so we do not have the `Send` and `Sync` for this async fn trait
#[enum_dispatch::enum_dispatch(TaskInstanceImpl)]
pub(in crate::task) trait TaskInstance {
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

impl TaskInstanceImpl {
    pub fn new(
        mode: DownloadMode,
        threaded_rt: ThreadedRuntimeImpl,
        local_runtime_builder: Option<LocalRuntimeBuilderImpl>,
    ) -> Self {
        match mode {
            DownloadMode::Singleton => {
                Self::Singleton(SingletonTask::new(threaded_rt, local_runtime_builder))
            }
            DownloadMode::Concurrent => {
                Self::Concurrent(ConcurrentTask::new(threaded_rt, local_runtime_builder))
            }
        }
    }
}
