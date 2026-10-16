#![doc = include_str!("../../docs/runner.md")]
use async_ringbuf::{AsyncHeapCons, AsyncHeapProd};
use bytes::Bytes;
use futures::future::BoxFuture;
use smol_cancellation_token::CancellationToken;

use crate::{
    adapter::{AdapterError, AnyBytesStream},
    task::RunnerId,
};

mod builder;
mod connector;
mod control;
mod error;
mod run_loop;
pub use builder::*;
pub use connector::*;
use control::ControlSource;
pub use control::{ControlSignalReceiver, ControlSignalSender};
pub(crate) use control::{LimitReceiver, LimitSender, limit_mailbox};
pub use error::*;

// TODO: make it configurable or detect the local disk performance?
const BUFFER_SIZE: usize = 32 * 1024; // 32KB

/// The default period without non-empty upstream data after which a runner
/// fails with a retryable [`TaskError::Timeout`]
pub const DEFAULT_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const DATA_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub const LIFECYCLE_CHANNEL_CAPACITY: usize = 2;
pub const DATA_FRAME_CHANNEL_CAPACITY: usize = 32;

#[derive(Debug)]
pub struct DataFrame {
    pub data: Bytes,
}

pub type DataFrameSender = AsyncHeapProd<DataFrame>;
pub type DataFrameReceiver = AsyncHeapCons<DataFrame>;
pub type LifecycleSender = AsyncHeapProd<LifecycleEvent>;
pub type LifecycleReceiver = AsyncHeapCons<LifecycleEvent>;

#[derive(Debug)]
pub enum LifecycleEvent {
    /// The runner is connected; its data frames follow
    Started,
    /// Stopped with message
    Stopped(StoppedReason),
}

impl LifecycleEvent {
    #[inline]
    pub fn failed(e: TaskError) -> Self {
        Self::Stopped(StoppedReason::Failed(e))
    }

    #[inline]
    pub fn finished() -> Self {
        Self::Stopped(StoppedReason::Finished)
    }

    #[inline]
    pub fn started() -> Self {
        Self::Started
    }

    #[inline]
    pub const fn is_finished(&self) -> bool {
        matches!(self, Self::Stopped(StoppedReason::Finished))
    }

    #[inline]
    pub fn is_cancelled(&self) -> bool {
        match self {
            Self::Stopped(StoppedReason::Failed(e)) => e.is_cancelled(),
            _ => false,
        }
    }
}

/// The reason why the task is stopped
#[derive(Debug)]
pub enum StoppedReason {
    /// The task is finished
    Finished,
    /// The task is failed
    Failed(TaskError),
}

#[derive(Debug)]
enum TaskRunError {
    Cancelled,
    Failed(TaskError),
}

impl From<TaskError> for TaskRunError {
    fn from(value: TaskError) -> Self {
        Self::Failed(value)
    }
}

/// Where a runner reads its bytes from.
enum RunnerInput {
    Connected(AnyBytesStream),
    /// Connected by the runner itself before it starts.
    Connecting(BoxFuture<'static, Result<AnyBytesStream, AdapterError>>),
    /// The connection was abandoned before it yielded a stream.
    Abandoned,
}

/// runner for each chunk, or single file, responsible for downloading each chunk
#[derive(derive_more::Debug)]
pub struct TaskRunner {
    /// The id of the runner
    #[cfg_attr(
        not(feature = "tracing"),
        expect(dead_code, reason = "only read by tracing spans")
    )]
    id: RunnerId,
    /// The total size of the this chunk or file
    /// possible None if the total size is unknown
    total: Option<u64>,
    /// the downloaded size
    downloaded: u64,
    /// the stream of the task, or how to connect it
    #[debug(skip)]
    input: RunnerInput,
    /// the control signals from the manager
    #[debug(skip)]
    control: ControlSource,
    /// the sender of the lifecycle events
    #[debug(skip)]
    lifecycle_tx: LifecycleSender,
    /// the sender of the data frames
    #[debug(skip)]
    data_tx: Option<DataFrameSender>,
    /// How long to wait for data frames to be consumed after the stream stops.
    data_drain_timeout: std::time::Duration,
    /// How long the upstream may yield no non-empty data before failing with `Timeout`.
    idle_timeout: std::time::Duration,
    /// the cancel token
    cancel_token: CancellationToken,
}

impl TaskRunner {
    /// Get the builder of the task runner
    pub fn builder() -> TaskRunnerBuilder {
        TaskRunnerBuilder::default()
    }
}

#[cfg(test)]
mod tests;
