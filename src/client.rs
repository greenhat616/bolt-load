use derive_builder::Builder;
use futures::Stream;

use crate::{
    adapter::{self, AnyStream},
    manager::BoltLoadTaskManager, strategy::Strategy,
};

#[derive(Default)]
pub enum BoltLoadPreferDownloadMode {
    /// All tasks should be single thread
    SingleThread,
    /// All tasks should be multi thread if range stream is available
    #[default]
    MultiThread,
}

// TODO: we should implement a life cycle for the client, and provide a client handle to task manager
/// BotLoaderGlobalConfiguration
struct BoltLoadConfiguration {
    prefer_download_mode: BoltLoadPreferDownloadMode,
}

// The main client
#[derive(Builder)]
#[builder(pattern = "owned")]
pub struct BoltLoad {
    #[builder(setter(skip))]
    tasks: Vec<BoltLoadTaskManager>,
    configuration: BoltLoadConfiguration,
}

/// a handle to access client context from sub modules
pub(crate) struct BoltLoadHandle {}

impl BoltLoad {
    // Start the load from url process
    // Should be powered by a state machine
    pub async fn start<A, S, T, Y>(&mut self, adapter: A, save_path: &String)
    where
        A: adapter::BoltLoadAdapter<
                Item = Result<bytes::Bytes, std::io::Error>,
                Stream = AnyStream<std::io::Result<bytes::Bytes>>,
            > + Sync
            + 'static,
        S: Stream<Item = T>,
        T: Send,
        Y: Strategy
    {
        // Split into load tasks
        // According to the mode, etc
        match self.configuration.prefer_download_mode {
            BoltLoadPreferDownloadMode::SingleThread => {
                self.tasks = vec![];
                let task = BoltLoadTaskManager::new_single::<S, A, T>(adapter, save_path);
                self.tasks.push(task);
            }
            BoltLoadPreferDownloadMode::MultiThread => {
                self.tasks = BoltLoadTaskManager::new_multi::<S, A, T, Y>(adapter, save_path).await;
            }
        }
        // TODO: tell the tasks to start
        todo!()
    }
}
