mod multi_thread;
mod planner;
mod single_thread;

use async_channel::{Receiver, Sender};
pub use multi_thread::*;
pub use planner::*;
pub use single_thread::*;

use futures::Stream;

use crate::{
    adapter::{AnyStream, BoltLoadAdapterMeta},
    client::BoltLoadPreferDownloadMode,
    runner::TaskRunner,
    strategy::{Strategy, StrategyAction},
};

pub type TaskId = usize;

/// messages for manager -> runner
pub struct ManagerMessage(pub TaskId, pub ManagerMessagesVariant);
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

#[derive(Clone)]
pub enum BoltLoadDownloadMode {
    SingleThread,
    MultiThread,
}

#[derive(Clone)]
pub enum BoltLoadTaskState {
    Idle,
    Loading,
    WaitingForMerge,
    Merging,
    Failed(String),
    Finished,
}

// #[derive(Clone)]
pub struct BoltLoadTaskManager {
    adapter: Box<
        dyn super::adapter::BoltLoadAdapter<
                Item = std::io::Result<bytes::Bytes>,
                Stream = AnyStream<std::io::Result<bytes::Bytes>>,
            > + Sync
            + 'static,
    >,
    mode: BoltLoadDownloadMode,
    save_path: String,
    state: BoltLoadTaskState,
    meta: Option<BoltLoadAdapterMeta>,
    runners: Vec<TaskRunner>,
    channel: Option<(Sender<ManagerMessage>, Receiver<ManagerMessage>)>,
}

impl BoltLoadTaskManager {
    pub async fn new<S, A, T, Y>(
        adapter: A,
        save_path: &String,
        preffered_mode: BoltLoadPreferDownloadMode,
    ) -> Result<Self, String>
    where
        A: super::adapter::BoltLoadAdapter<
                Item = Result<bytes::Bytes, std::io::Error>,
                Stream = AnyStream<std::io::Result<bytes::Bytes>>,
            > + Sync
            + 'static,
        S: Stream<Item = T>,
        T: Send,
        Y: Strategy,
    {
        let adapter = Box::new(adapter);
        let Ok(metadata) = adapter.retrieve_meta().await else {
            // TODO: Change to custom error types
            return Err("Failed to retrieve metadata".to_string());
        };

        let channel = async_channel::unbounded::<ManagerMessage>();

        let final_mode = match preffered_mode {
            BoltLoadPreferDownloadMode::SingleThread => BoltLoadDownloadMode::SingleThread,
            BoltLoadPreferDownloadMode::MultiThread => {
                if adapter.is_range_stream_available().await && metadata.content_size > 0 {
                    BoltLoadDownloadMode::MultiThread
                } else {
                    BoltLoadDownloadMode::SingleThread
                }
            }
        };
        match final_mode {
            BoltLoadDownloadMode::SingleThread => {
                let manager = Self {
                    adapter,
                    mode: BoltLoadDownloadMode::SingleThread,
                    save_path: save_path.clone(),
                    state: BoltLoadTaskState::Idle,
                    meta: Some(metadata),
                    runners: vec![],
                    channel: Some(channel),
                };
                Ok(manager)
            }
            BoltLoadDownloadMode::MultiThread => {
                let manager = Self {
                    adapter,
                    mode: BoltLoadDownloadMode::MultiThread,
                    save_path: save_path.clone(),
                    state: BoltLoadTaskState::Idle,
                    meta: Some(metadata),
                    runners: vec![],
                    channel: Some(channel),
                };
                Ok(manager)
            }
        }
    }

    pub async fn run(&mut self) -> Result<(), String> {
        match self.mode {
            BoltLoadDownloadMode::SingleThread => todo!(),
            BoltLoadDownloadMode::MultiThread => self.run_multi().await,
        }
    }
}

impl PartialEq for BoltLoadDownloadMode {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (BoltLoadDownloadMode::SingleThread, BoltLoadDownloadMode::SingleThread) => true,
            (BoltLoadDownloadMode::MultiThread, BoltLoadDownloadMode::MultiThread) => true,
            _ => false,
        }
    }
}
