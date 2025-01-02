use async_trait::async_trait;

use crate::manager::BoltLoadDownloadMode;

use super::{BoltLoadTaskManager, ChunkPlanner};

#[async_trait]
pub trait MultiThreadTask {
    async fn load_part(&self, start_pos: u64, end_pos: u64);
    async fn merge(&self, save_path: &String);
    async fn run(&self);
}

#[async_trait]
impl MultiThreadTask for BoltLoadTaskManager {
    async fn load_part(&self, start_pos: u64, end_pos: u64) {
        let Ok(range_stream) = self.adapter.range_stream(start_pos, end_pos).await else {
            todo!()
        };
        // Send stream to manager
        unimplemented!()
    }

    async fn merge(&self, save_path: &String) {
        unimplemented!()
    }

    async fn run(&self) {
        // Do fetch according to url, start_pos, end_pos
        // Save to save_path
        assert!(self.mode == BoltLoadDownloadMode::MultiThread);

        let Ok(meta) = self.adapter.retrieve_meta().await else {
            todo!()
        };
        let mut chunk_planner = ChunkPlanner::new(meta.content_size);

        // chunk_planner
        
        unimplemented!()
    }
}
