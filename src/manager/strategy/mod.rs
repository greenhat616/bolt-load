mod chunk;

pub use chunk::*;

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
    fn step(manager: &TaskManager) -> Vec<StrategyAction>;
}
