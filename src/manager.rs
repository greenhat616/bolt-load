use futures::Stream;

pub mod multi_thread;
pub mod single_thread;

pub enum BoltLoadDownloadMode {
    SingleThread,
    MultiThread,
}

pub enum BoltLoadTaskState {
    Idle,
    Loading,
    WaitingForMerge,
    Merging,
    Failed(String),
    Finished,
}

pub struct BoltLoadTaskManager {
    adapter: Box<
        dyn super::adapter::BoltLoadAdapter<Box<dyn Stream<Item = Vec<u8>>>, Vec<u8>>
            + Sync
            + 'static,
    >,
    mode: BoltLoadDownloadMode,
    save_path: String,
    url: String,
    state: BoltLoadTaskState,
}

impl BoltLoadTaskManager {
    pub fn new_single<S, A, T>(adaptor: A, save_path: &String, url: &String) -> Self
    where
        A: super::adapter::BoltLoadAdapter<S, T> + Sync + 'static,
        S: Stream<Item = T>,
        T: Send,
    {
        todo!();
    }

    pub fn new_multi<S, A, T>(adaptor: A, save_path: &String, url: &String) -> Vec<Self>
    where
        A: super::adapter::BoltLoadAdapter<S, T> + Sync + 'static,
        S: Stream<Item = T>,
        T: Send,
    {
        todo!();
    }
}
