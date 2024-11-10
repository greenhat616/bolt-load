use async_trait::async_trait;

use super::BoltLoaderTaskManager;

#[async_trait]
pub trait SingleThreadTask {
    async fn load(&self, url: &String);
    async fn save(&self, save_path: &String);
    async fn run(&self);
}

#[async_trait]
impl SingleThreadTask for BoltLoaderTaskManager {
    async fn load(&self, url: &String) {
        unimplemented!()
    }

    async fn save(&self, save_path: &String) {
        unimplemented!()
    }

    async fn run(&self) {
        // Simply do fetch and save, ignore the start_pos and end_pos
        unimplemented!()
    }
}
