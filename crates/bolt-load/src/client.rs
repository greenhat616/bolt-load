use std::collections::HashMap;

use async_channel::Sender;

use crate::{
    adapter::{self},
    task::{DownloadMode, TaskCommand},
};

pub enum DownloadSource {
    Adapter(adapter::AnyAdapter),
    // TODO: add abstract trait for self managed task, such as a bittorrent task
    SelfManaged,
}

// TODO: we should implement a life cycle for the client, and provide a client handle to task manager
/// BotLoaderGlobalConfiguration
#[derive(Debug, Default, Clone)]
#[allow(dead_code)]
pub struct BoltLoadConfiguration {
    prefer_download_mode: DownloadMode,
}

type TaskId = u64;
// The main client
#[non_exhaustive]
#[allow(dead_code)]
pub struct BoltLoad {
    // TODO: use a enum to represent the manager or channel
    // TODO: rethink how to share the state of the task manager, when we impl the persistent
    pub(crate) tasks: HashMap<TaskId, Sender<TaskCommand>>,
    pub(crate) configuration: BoltLoadConfiguration,
}

/// a handle to access client context from sub modules
#[allow(dead_code)]
pub(crate) struct BoltLoadHandle {}

impl BoltLoad {}
