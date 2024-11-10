use crate::manager::BoltLoaderTaskManager;

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
    tasks: Vec<BoltLoaderTaskManager>,
    save_path: String,
    url: String,
    state: BoltLoaderState,
    mode: BoltLoaderMode,
}

impl BoltLoader {
    pub fn new<'a, T>(adaptor: T, mode: BoltLoaderMode, save_path: String, url: String) -> Self
    where
        T: crate::adaptor::BoltLoadAdaptor + Sync + 'static,
    {
        let tasks = match mode {
            BoltLoaderMode::SingleThread => {
                let mut tasks = Vec::new();
                tasks.push(BoltLoaderTaskManager::new_single(adaptor, &save_path, &url));
                tasks
            }
            BoltLoaderMode::MultiThread => {
                BoltLoaderTaskManager::new_multi(adaptor, &save_path, &url)
            }
        };

        BoltLoader {
            tasks,
            save_path,
            url,
            state: BoltLoaderState::Idle,
            mode,
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
