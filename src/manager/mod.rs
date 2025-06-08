use async_channel::{Receiver, Sender};
use async_fs::{File, OpenOptions};
use bytes::Bytes;
use futures::{AsyncSeekExt, AsyncWriteExt, FutureExt, StreamExt};
use smol_cancellation_token::CancellationToken;
use std::{
    collections::HashMap, io::SeekFrom, ops::Range, path::PathBuf, rc::Rc, sync::Arc, time::Instant,
};

use crate::{
    adapter::{AnyAdapter, BoltLoadAdapterMeta},
    runner::{RunnerMessage, RunnerMessageKind, TaskFailedKind, TaskRunner},
    runtime::ThreadedRuntimeImpl,
};

mod builder;
mod runner_notification;
mod task;

pub use builder::*;

use runner_notification::*;
use task::*;

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

#[derive(Clone, Default)]
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
    Failed(TaskManagerFailedError),
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum TaskManagerFailedError {
    /// the task is cancelled
    #[error("the task is cancelled")]
    Cancelled,
    /// the task is failed to allocate the file size
    #[error("failed to allocate the file size: {0}")]
    AllocateError(String),
    /// the task is stopped
    #[error("the task is stopped: {0:?}")]
    Stopped(TaskError),
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum TaskManagerCommandError {}

type CommandResult<T> = Result<T, TaskManagerCommandError>;
type CommandResponse<T> = oneshot::Sender<CommandResult<T>>;

pub enum TaskManagerCommand {
    Cancel(CommandResponse<()>),
    // Pause(CommandResponse<()>),
    // Resume(CommandResponse<()>),
}

#[derive(Default)]
struct TaskManagerStateControl(Option<(TaskState, async_channel::Sender<TaskState>)>);

impl TaskManagerStateControl {
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
pub struct TaskManager<'a> {
    /// the async runtime passed from the client
    runtime: ThreadedRuntimeImpl,
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
    task_state: TaskState,
    /// the meta of this task
    meta: BoltLoadAdapterMeta,

    cancel_token: CancellationToken,
    task: TaskImpl,

}

/// fast fail the call, and dispatch the state to the state control


impl TaskManager<'_> {


}

impl TaskManager<'_> {
    /// check if the task is finished or failed
    pub fn is_finished(&self) -> bool {
        matches!(
            self.state_control,
            TaskManagerStateControl(Some((
                TaskState::Finished | TaskState::Failed(_),
                _
            )))
        )
    }

    /// check if the task is initialized
    pub fn is_initialized(&self) -> bool {
        self.state_control.0.is_some()
    }

    /// cancel the task, and do the cleanup work
    pub async fn cancel(&mut self) {
        self.state_control
            .dispatch(TaskState::Failed(TaskManagerFailedError::Cancelled))
            .await;
        todo!()
    }

    /// the main loop of the task manager
    /// This function should be called in a async spawn.
    pub async fn run(&mut self) {
        let (state_sender, state_receiver) = async_channel::bounded(2);
        state_sender.send(TaskState::Idle).await.unwrap();
        self.state_control = TaskManagerStateControl::new(TaskState::Idle, state_sender);

        let state_dispatcher = self.handle_state_change(state_receiver);
        let mut downloading_dispatcher = None;

        loop {
            let cmd = self.cmd_rx.recv().fuse();
            let state = state_receiver.recv().fuse();
            futures::pin_mut!(cmd, state);
            futures::select! {
                cmd = cmd => {
                    let flag = match cmd {
                        Ok(cmd) => {
                            self.handle_cmd(cmd)
                        }
                        // Only the cmd is closed, so let's cancel the task immediately
                        Err(_) => {
                            self.cancel();
                            true
                        }
                    };
                    if flag {
                        break;
                    }
                }
                state = state => {
                    match state {
                        Ok(state) => {
                            state_future = self.handle_state_change(state).boxed().fuse();
                        }
                        Err(_) => {
                            log::error!("failed to receive the state from the channel");
                            break;
                        }
                    }
                }
                // poll the state future
                _ = &mut state_future => (),
            }
        }
    }
}
