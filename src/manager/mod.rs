use async_channel::{Receiver, Sender};
use async_fs::File;
use bytes::Bytes;
use futures::{AsyncSeekExt, AsyncWriteExt, FutureExt, StreamExt};
use std::{io::SeekFrom, path::PathBuf};

use crate::{
    adapter::{AnyAdapter, BoltLoadAdapterMeta},
    runner::{RunnerMessage, RunnerMessageKind},
    runtime::Runtime,
};

mod builder;
mod runner_notification;
mod strategy;
mod task;

pub use builder::*;

use runner_notification::*;
use strategy::*;
use task::*;

pub type RunnerId = usize;

/// messages for manager -> runner
pub struct ManagerMessage(pub RunnerId, pub ManagerMessagesVariant);
pub enum ManagerMessagesVariant {
    /// resize the total size of the task
    ResizeTotal(u64),
    Cancel,
}

/// the main progress of the download
pub struct Progress {
    /// the total size of the content
    /// possible None if the total size is unknown
    /// It requires the single-thread task can finished until the stream is None
    pub total: Option<u64>,

    /// the current downloaded size
    pub current: u64,
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
pub enum TaskManagerState {
    #[default]
    /// the task is idle, the initial state
    Idle,
    /// the task is allocating the file size for the temp file
    Allocating,
    /// the task is downloading the content
    Downloading,
    /// the task is finishing, do some cleanup work.
    /// Such as rename the file to the final name.
    Finishing,
    /// the task is failed with the error
    Failed(TaskManagerFailedError),
    /// the task is finished
    Finished,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum TaskManagerFailedError {
    /// the task is cancelled
    #[error("the task is cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum TaskManagerCommandError {}

type CommandResult<T> = Result<T, TaskManagerCommandError>;
type CommandResponse<T> = oneshot::Sender<CommandResult<T>>;

pub enum TaskManagerCommand {
    Cancel(CommandResponse<()>),
}

// #[derive(Clone)]
// #[derive(Clone)]
#[non_exhaustive]
pub struct TaskManager<'a> {
    /// the async runtime passed from the client
    runtime: Runtime,
    /// the inner adapter of this task
    // TODO: support persistent adapter
    adapter: AnyAdapter,
    /// the current mode of this task
    // TODO: maybe we should introduce a `prefer_mode` to indicate the preferred mode of this task.
    // TODO: support resumable download
    mode: DownloadMode,
    /// the save path of this task
    save_path: PathBuf,
    /// the file handle of this task
    file_handle: File,
    /// the current state of this task
    state: TaskManagerState,
    /// the meta of this task
    meta: BoltLoadAdapterMeta,

    /// the task of this manager
    // TODO: dynamic switch the task mode, for the future, possible resume after a long time period
    task: Task,

    /// a control channel between manager and runners
    control_channel: (Sender<ManagerMessage>, Receiver<ManagerMessage>),
    /// the notification channel for the runners
    runners_notification: RunnerNotification<'a, RunnerMessage>,
    /// the command channel
    cmd_rx: Receiver<TaskManagerCommand>,
}

impl TaskManager<'_> {
    fn dispatch_state(&mut self, state: TaskManagerState) {
        self.state = state;
    }

    /// pre-allocate the file size for the temp file
    async fn allocate_file_size(&mut self) -> Result<(), std::io::Error> {
        if self.meta.content_size > 0 {
            self.file_handle.set_len(self.meta.content_size).await?;
        }
        Ok(())
    }

    /// write the chunk to the file by the runner position
    /// and notify the state to the chunk planner
    async fn write_chunk(
        &mut self,
        runner_id: RunnerId,
        chunk: Bytes,
    ) -> Result<(), std::io::Error> {
        match self.task.get_runner_pos(runner_id) {
            Some(pos) => {
                self.file_handle.seek(SeekFrom::Start(pos)).await?;
                self.file_handle.write_all(&chunk).await?;
            }
            None => {
                self.file_handle.write_all(&chunk).await?;
            }
        }
        todo!("notify the state to the chunk planner");
    }

    fn handle_runner_message(&mut self, msg: RunnerMessage) {
        let runner_id = msg.0;
        let kind = msg.1;
        match kind {
            RunnerMessageKind::Started => {
                log::trace!("Runner {} started", runner_id);
            }
            RunnerMessageKind::Stopped(reason) => {
                log::trace!("Runner {} stopped with reason: {:?}", runner_id, reason);
            }
            RunnerMessageKind::Downloaded(data) => {
                log::trace!("Runner {} downloaded {} bytes", runner_id, data.len());
                todo!("write the data to the file")
            }
        }
    }

    fn handle_cmd(&mut self, cmd: TaskManagerCommand) -> bool {
        todo!()
    }
}

impl TaskManager<'_> {
    /// check if the task is finished or failed
    pub fn is_finished(&self) -> bool {
        matches!(
            self.state,
            TaskManagerState::Finished | TaskManagerState::Failed(_)
        )
    }

    /// cancel the task, and do the cleanup work
    pub fn cancel(&mut self) {
        self.dispatch_state(TaskManagerState::Failed(TaskManagerFailedError::Cancelled));
        todo!()
    }

    /// the main loop of the task manager
    /// This function should be called in a async spawn.
    pub async fn run(&mut self) {
        loop {
            let cmd = self.cmd_rx.recv().fuse();
            let notification = self.runners_notification.next().fuse();
            futures::pin_mut!(cmd, notification);
            // It is necessary to use select_biased to ensure the notification is always processed first
            futures::select_biased! {
                notification = notification => {
                    if let Some(msg) = notification {
                        self.handle_runner_message(msg);
                    }
                }
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
            }
        }
    }
}
