use async_channel::{Receiver, Sender};
use futures::Stream;
use std::path::PathBuf;

use crate::{
    adapter::{AnyAdapter, AnyStream, BoltLoadAdapterMeta},
    runner::TaskRunner,
    strategy::{Strategy, StrategyAction},
};

mod builder;
mod multi_thread;
mod planner;
mod single_thread;

pub use builder::*;
pub use multi_thread::*;
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
    Failed(String),
    /// the task is finished
    Finished,
}

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
    /// a channel between manager and runners
    channel: (Sender<ManagerMessage>, Receiver<ManagerMessage>),
}

impl TaskManager {
    pub async fn run(&mut self) -> Result<(), String> {
        match self.mode {
            DownloadMode::Singleton => todo!(),
            DownloadMode::Concurrent => todo!(),
        }
    }
}
