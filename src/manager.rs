pub mod multi_thread;
pub mod single_thread;

pub struct BoltLoaderTask {
    pub url: String,
    pub save_path: String,
    pub adaptor: Box<dyn super::adaptor::BoltLoadAdaptor + Sync>,
    pub start_pos: u64,
    pub end_pos: u64,
}
