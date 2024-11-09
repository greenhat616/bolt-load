use std::{borrow::Borrow, sync::RwLock};

use crate::manager::BoltLoaderTask;

pub enum BoltLoaderState {
    Idle,
    Loading,
    WaitingForMerge,
    Merging,
    Failed(String),
    Finished,
}

pub enum BoltLoaderMode {
    SingleThread,
    MultiThread,
}

// The main client
pub struct BoltLoader {
    inner: RwLock<BoltLoaderInner>,
    client: Box<dyn crate::adaptor::BoltLoadAdaptor + Sync>,
    save_path: String,
    url: String,
}

struct BoltLoaderInner {
    state: BoltLoaderState,
    mode: BoltLoaderMode,
    // The task will be executed according to the mode
    tasks: Vec<BoltLoaderTask>,
}

impl BoltLoader {
    pub fn new<'a, T>(client: T, mode: BoltLoaderMode, save_path: String, url: String) -> Self
    where
        T: crate::adaptor::BoltLoadAdaptor + Sync + 'static,
    {
        Self {
            inner: RwLock::new(BoltLoaderInner {
                state: BoltLoaderState::Idle,
                tasks: Vec::new(),
                mode: mode,
            }),
            client: Box::new(client),
            save_path,
            url,
        }
    }

    // Start the load from url process
    // Should be powered by a state machine
    pub fn start(&self) {
        // Split into load tasks
        // According to the mode, etc        
        unimplemented!();
    }
}
