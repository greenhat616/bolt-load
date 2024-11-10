use async_trait::async_trait;

use super::BoltLoaderTaskManager;

#[async_trait]
pub trait MultiThreadTask {
    async fn load_part(&self, url: &String, start_pos: u64, end_pos: u64);
    async fn save_part(&self, save_path: &String);
    async fn merge(&self, save_path: &String);
    async fn run(&self);
}

#[async_trait]
impl MultiThreadTask for BoltLoaderTaskManager {
    async fn load_part(&self, url: &String, start_pos: u64, end_pos: u64) {
        unimplemented!()
    }

    async fn save_part(&self, save_path: &String) {
        unimplemented!()
    }

    async fn merge(&self, save_path: &String) {
        unimplemented!()
    }

    async fn run(&self) {
        // Do fetch according to url, start_pos, end_pos
        // Save to save_path
        unimplemented!()
    }
}
