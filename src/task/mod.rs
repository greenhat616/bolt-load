use async_channel::{Receiver, Sender};
use async_fs::{File, OpenOptions};
use bytes::Bytes;
use futures::{AsyncSeekExt, AsyncWriteExt, FutureExt, StreamExt};
use smol_cancellation_token::CancellationToken;
use std::{
    collections::HashMap,
    io::SeekFrom,
    ops::Range,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Mutex, atomic::Ordering},
    time::Instant,
};

use crate::{
    adapter::{AnyAdapter, BoltLoadAdapterMeta},
    runner::{RunnerMessage, RunnerMessageKind, TaskFailedKind, TaskRunner},
    runtime::ThreadedRuntimeImpl,
};

mod builder;
mod instance;
mod runner_notification;

pub use builder::*;

use instance::*;
use runner_notification::*;

pub type RunnerId = usize;

/// messages for manager -> runner
pub struct ManagerMessage(pub RunnerId, pub ManagerMessagesVariant);
pub enum ManagerMessagesVariant {
    /// resize the total size of the task
    ResizeTotal(u64),
}

pub type DownloadedChunks = Vec<Range<u64>>;

/// the progress of the download task
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Progress {
    /// the total size of the content
    /// possible None if the total size is unknown
    /// It requires the single-thread task can finished until the stream is None
    pub total: Option<u64>,

    /// the current downloaded size
    pub downloaded: u64,

    /// the downloaded chunks,
    /// for the single-thread task, it is the downloaded chunk
    /// for the concurrent task, it is the downloaded chunks
    pub downloaded_chunks: DownloadedChunks,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum DownloadMode {
    /// the download mode of single-thread download,
    /// Just use one runner task to fetch the content.
    Singleton,
    /// the download mode of concurrent download
    /// Due to we use multiple async tasks to download the content,
    /// and the async tasks is M:N thread model,
    /// so the download mode is concurrent, not called multi-thread.
    /// Because possible some runtime support single-thread async mode.
    #[default]
    Concurrent,
}

#[atomic_enum::atomic_enum]
#[derive(Default, PartialEq)]
pub enum TaskState {
    #[default]
    /// the task is idle, the initial state
    Idle,
    /// the task is initializing the task
    Initializing,
    /// the task is downloading the content
    Downloading,
    /// the task is finished
    Finished,
    /// the task is failed with the error
    Failed,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum TaskFailedError {
    /// the task is cancelled
    #[error("the task is cancelled")]
    Cancelled,
    /// the task is stopped
    #[error("the task is stopped: {0:?}")]
    Stopped(TaskInstanceError),
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum TaskCommandError {}

type CommandResult<T> = Result<T, TaskCommandError>;
type CommandResponse<T> = oneshot::Sender<CommandResult<T>>;

pub enum TaskCommand {
    Cancel(CommandResponse<()>),
    // Pause(CommandResponse<()>),
    // Resume(CommandResponse<()>),
}

#[derive(Default)]
struct TaskStateControl(Option<(TaskState, async_channel::Sender<TaskState>)>);

impl TaskStateControl {
    pub fn new(state: TaskState, sender: Sender<TaskState>) -> Self {
        Self(Some((state, sender)))
    }

    pub async fn dispatch(
        &mut self,
        state: TaskState,
    ) -> Result<(), async_channel::SendError<TaskState>> {
        let Some((manager_state, sender)) = self.0.as_mut() else {
            unreachable!("we should init the state control first");
        };
        *manager_state = state.clone();
        sender.send(state).await?;
        Ok(())
    }
}

// #[derive(Clone)]
// #[derive(Clone)]
#[non_exhaustive]
pub struct Task {
    /// the async runtime passed from the client
    rt: ThreadedRuntimeImpl,
    /// the inner adapter of this task
    // TODO: support persistent adapter
    adapter: AnyAdapter,
    /// the current mode of this task
    // TODO: maybe we should introduce a `prefer_mode` to indicate the preferred mode of this task.
    // TODO: support resumable download
    mode: DownloadMode,
    /// the save path of this task
    save_path: PathBuf,
    /// the tmp path of this task
    tmp_path: PathBuf,

    /// the meta of this task
    meta: BoltLoadAdapterMeta,

    cancel_token: CancellationToken,

    on_task_state_changed: Arc<Vec<Box<dyn Fn(TaskEvent)>>>,
    task: TaskInstanceImpl,
    task_state: Arc<AtomicTaskState>,
    last_error: Arc<Mutex<Option<TaskInstanceError>>>,
}

struct EventDispatcher(Vec<Box<dyn Fn(TaskEvent)>>);

impl EventDispatcher {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn add_callback(&mut self, callback: Box<dyn Fn(TaskEvent)>) {
        self.0.push(callback);
    }

    pub fn dispatch(&self, event: TaskEvent) {
        for callback in self.0.iter() {
            callback(event.clone());
        }
    }
}

pub struct SpawnedTask {
    on_task_state_changed: Arc<EventDispatcher>,
    task_state: Arc<AtomicTaskState>,
    last_error: Arc<Mutex<Option<TaskInstanceError>>>,
    cancel_token: CancellationToken,
    task: TaskInstanceImpl,
}

impl Task {
    pub fn builder() -> TaskBuilder {
        TaskBuilder::default()
    }

    /// check if the task is finished or failed
    pub fn is_finished(&self) -> bool {
        matches!(
            self.task_state.load(Ordering::Acquire),
            TaskState::Finished | TaskState::Failed
        )
    }

    /// check if the task is initialized
    pub fn is_initialized(&self) -> bool {
        self.task_state.load(Ordering::Acquire) != TaskState::Idle
    }

    /// cancel the task, and do the cleanup work
    pub async fn cancel(&mut self) {
        self.cancel_token.cancel();
        todo!()
    }

    /// the main loop of the task manager
    /// This function should be called in a async spawn.
    pub async fn run(&mut self) {}
}
