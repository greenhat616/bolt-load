use crate::manager::BoltLoadTaskManager;

pub enum StrategyAction {
    ChangeMaxThread(usize),
    // Split all task into two seperate tasks
    SplitAllTask,
    // Split given task (task_id)
    SplitGivenTask(usize),
}

pub trait Strategy {
    fn step(manager: &BoltLoadTaskManager) -> Vec<StrategyAction>;
}