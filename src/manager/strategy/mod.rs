mod chunk;
mod dynamic_strategy;

pub use chunk::*;
pub use dynamic_strategy::*;

use crate::manager::TaskManager;

pub(crate) struct StrategyManager {
    pub chunk_planner: ChunkPlanner,
    // TODO: add concurrency control
}

pub enum StrategyAction {
    ChangeMaxThread(usize),
    // Split all task into two seperate tasks
    SplitAllTask,
    // Split given task (task_id)
    SplitGivenTask(usize),
}

pub trait Strategy {
    fn step(&mut self, manager: &TaskManager) -> Vec<StrategyAction>;
}
