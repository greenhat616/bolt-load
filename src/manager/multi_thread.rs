
use async_trait::async_trait;

use crate::{
    adapter::{AnyStream, BoltLoadAdapter},
    manager::{BoltLoadTaskState, ChunkPlanner},
    runner::TaskRunner,
    strategy::Strategy,
};

use super::BoltLoadTaskManager;

// TODO: Remove ?Send
#[async_trait(?Send)]
pub trait MultiThreadTaskManager {
    async fn run_multi(&mut self) -> Result<(), String>;
}

#[async_trait(?Send)]
impl MultiThreadTaskManager for BoltLoadTaskManager {
    async fn run_multi(&mut self) -> Result<(), String> {
        assert!(self.meta.is_some());
        let mut planner = ChunkPlanner::new(self.meta.as_ref().unwrap().content_size);
        loop {
            match &self.state {
                BoltLoadTaskState::Idle => {
                    planner.add_chunk(0..self.meta.as_ref().unwrap().content_size, 0);
                    
                    // let (runner, rx) = TaskRunner::new(
                    //     Some(self.meta.as_ref().unwrap().content_size),
                    //     self.adapter,
                    //     task_id,
                    //     receiver,
                    // );
                    self.state = BoltLoadTaskState::Loading;
                }
                BoltLoadTaskState::Loading => {
                    loop {
                        // TODO: loop until all chunks are loaded
                    }
                    self.state = BoltLoadTaskState::WaitingForMerge;
                }
                BoltLoadTaskState::WaitingForMerge => todo!(),
                BoltLoadTaskState::Merging => todo!(),
                BoltLoadTaskState::Failed(e) => {
                    return Err(e.clone());
                }
                BoltLoadTaskState::Finished => break,
            }
        }
        todo!()
    }
}
