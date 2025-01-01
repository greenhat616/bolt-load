mod multi_thread;
mod planner;
mod single_thread;

use std::ops::Range;

pub use multi_thread::*;
pub use planner::*;
pub use single_thread::*;

use futures::Stream;

use crate::{
    adapter::AnyStream,
    runner::TaskRunner,
    strategy::{Strategy, StrategyAction},
};

// TODO: change this constant
const MININUM_THREAD: usize = 2;

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
}

impl BoltLoadTaskManager {
    pub fn new_single<S, A, T>(adapter: A, save_path: &String) -> Self
    where
        A: super::adapter::BoltLoadAdapter<
                Item = Result<bytes::Bytes, std::io::Error>,
                Stream = AnyStream<std::io::Result<bytes::Bytes>>,
            > + Sync
            + 'static,
        S: Stream<Item = T>,
        T: Send,
    {
        let adapter = Box::new(adapter);
        BoltLoadTaskManager {
            adapter,
            mode: BoltLoadDownloadMode::SingleThread,
            save_path: save_path.clone(),
            state: BoltLoadTaskState::Idle,
        }
    }

    pub async fn new_multi<S, A, T, Y>(adapter: A, save_path: &String) -> Vec<Self>
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
        // Split into load tasks

        // Retrive metadatas
        let Ok(metadata) = adapter.retrieve_meta().await else {
            todo!()
        };
        // Init chunk planners
        let mut planner = ChunkPlanner::new(metadata.content_size);

        // Init manager with minimum thread
        let mut task_managers = Vec::<Self>::new();

        // for _ in 0..MININUM_THREAD {
        //     let task_manager = BoltLoadTaskManager {
        //         adapter,
        //         mode: BoltLoadDownloadMode::MultiThread,
        //         save_path: save_path.clone(),
        //         state: BoltLoadTaskState::Idle,
        //     };
        //     task_managers.push(task_manager);
        // }

        loop {
            // let actions = Y::step();
            // for action in actions {
            //     match action {
            //         StrategyAction::ChangeMaxThread(_) => todo!(),
            //         StrategyAction::SplitAllTask => todo!(),
            //         StrategyAction::SplitGivenTask(_) => todo!(),
            //     }
            // }
        }
        todo!()
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
