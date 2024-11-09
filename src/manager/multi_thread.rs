use async_trait::async_trait;

use super::BoltLoaderTask;

#[async_trait]
pub trait MultiThreadTask {
    async fn run(&self);
}

#[async_trait]
impl MultiThreadTask for BoltLoaderTask {
    async fn run(&self) {
        // Do fetch according to url, start_pos, end_pos
        // Save to save_path
        unimplemented!();
    }
}
