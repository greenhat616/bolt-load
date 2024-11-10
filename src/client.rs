use crate::manager::BoltLoadTaskManager;

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
// TODO: use `derive_builder` to build the client
pub struct BoltLoad {
    tasks: Vec<BoltLoadTaskManager>,
    configuration: BoltLoadConfiguration,
}

/// a handle to access client context from sub modules
pub(crate) struct BotLoadHandle {}


impl BoltLoad {
    // Start the load from url process
    // Should be powered by a state machine
    pub fn start(&self) {
        // Split into load tasks
        // According to the mode, etc
        unimplemented!();
    }
}
