use std::collections::HashMap;

use async_channel::Sender;

use crate::{
    adapter::{self},
    manager::{DownloadMode, TaskManagerCommand},
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

type TaskId = u64;
// The main client
#[non_exhaustive]
pub struct BoltLoad {
    // TODO: use a enum to represent the manager or channel
    // TODO: rethink how to share the state of the task manager, when we impl the persistent
    pub(crate) tasks: HashMap<TaskId, Sender<TaskManagerCommand>>,
    pub(crate) configuration: BoltLoadConfiguration,
}

/// a handle to access client context from sub modules
pub(crate) struct BoltLoadHandle {}

impl BoltLoad {}
