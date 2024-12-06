use futures::Stream;

use crate::adapter::AnyStream;

pub mod multi_thread;
pub mod single_thread;

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
    url: String,
    state: BoltLoadTaskState,
}

impl BoltLoadTaskManager {
    pub fn new_single<S, A, T>(adapter: A, save_path: &String, url: &String) -> Self
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
            url: url.clone(),
            state: BoltLoadTaskState::Idle,
        }
    }

    pub fn new_multi<S, A, T>(adapter: A, save_path: &String, url: &String) -> Vec<Self>
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
        // Split into load tasks
        todo!()
    }
}
