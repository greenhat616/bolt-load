use async_trait::async_trait;

#[async_trait]
pub trait SingleThreadTask {
    async fn load(&self, url: &String);
    async fn save(&self, save_path: &String);
    async fn run(&self);
}
