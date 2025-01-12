use futures::Stream;

use crate::{
    adapter::{self, AnyStream},
    manager::{DownloadMode, TaskManager},
    strategy::Strategy,
};

pub enum DownloadSource {
    Adapter(adapter::AnyAdapter),
    // TODO: add abstract trait for self managed task, such as a bittorrent task
    SelfManaged,
}

// TODO: we should implement a life cycle for the client, and provide a client handle to task manager
/// BotLoaderGlobalConfiguration
#[derive(Debug, Default, Clone)]
pub struct BoltLoadConfiguration {
    prefer_download_mode: DownloadMode,
}

// The main client
#[non_exhaustive]
pub struct BoltLoad {
    pub(crate) tasks: Vec<TaskManager>,
    pub(crate) configuration: BoltLoadConfiguration,
}

/// a handle to access client context from sub modules
pub(crate) struct BoltLoadHandle {}

impl BoltLoad {}
