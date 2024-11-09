use async_trait::async_trait;

use super::BoltLoaderTask;

#[async_trait]
pub trait SingleThreadTask {
    async fn run(&self);
}

#[async_trait]
impl SingleThreadTask for BoltLoaderTask {
    async fn run(&self) {
        // Simply do fetch and save, ignore the start_pos and end_pos
        unimplemented!()
    }
}
