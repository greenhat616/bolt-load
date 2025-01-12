use async_channel::{Receiver, Sender};
use futures::FutureExt;
use std::path::PathBuf;

use crate::{
    adapter::{AnyAdapter, BoltLoadAdapterMeta},
    runner::TaskRunner,
};

mod builder;
mod multi_thread;
mod planner;
mod runner_notification;
mod single_thread;

pub use builder::*;
pub use planner::*;
pub use single_thread::*;

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
pub struct TaskManager {
    /// the inner adapter of this task
    // TODO: support persistent adapter
    adapter: AnyAdapter,
    /// the current mode of this task
    // TODO: maybe we should introduce a `prefer_mode` to indicate the preferred mode of this task.
    // TODO: support resumable download
    mode: DownloadMode,
    /// the save path of this task
    save_path: PathBuf,
    /// the current state of this task
    state: TaskManagerState,
    /// the meta of this task
    meta: BoltLoadAdapterMeta,
    /// the runners of this task
    runners: Vec<TaskRunner>,

    /// a control channel between manager and runners
    runner_control_channel: (Sender<ManagerMessage>, Receiver<ManagerMessage>),
    /// the command channel
    cmd_rx: Receiver<TaskManagerCommand>,
}

impl TaskManager {
    fn dispatch_state(&mut self, state: TaskManagerState) {
        self.state = state;
    }

    /// check if the task is finished or failed
    pub fn is_finished(&self) -> bool {
        matches!(
            self.state,
            TaskManagerState::Finished | TaskManagerState::Failed(_)
        )
    }

    /// the main loop of the task manager
    /// This function should be called in a async spawn.
    pub async fn run(&mut self) {
        // let handle_runner_msg = async {
        //     while let Ok(msg) = self.runner_control_channel.1.recv().await {
        //         match msg {
        //             ManagerMessage(_, ManagerMessagesVariant::Cancel) => {
        //                 self.dispatch_state(TaskManagerState::Failed(
        //                     TaskManagerFailedError::Cancelled,
        //                 ));
        //             }
        //         }
        //     }
        // }
        // .fuse();

        // let handle_cmd = async {
        //     while let Ok(cmd) = self.cmd_rx.recv().await {
        //         match cmd {
        //             TaskManagerCommand::Cancel(res) => {
        //                 self.dispatch_state(TaskManagerState::Failed(
        //                     TaskManagerFailedError::Cancelled,
        //                 ));
        //                 res.send(Ok(()));
        //                 break;
        //             }
        //         }
        //     }
        // }
        // .fuse();
    }
}
