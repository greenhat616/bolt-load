mod dynamic_strategy;

pub use dynamic_strategy::*;

#[derive(Debug)]
pub enum StrategyAction {
    ChangeMaxThread(usize),
    // Split all task into two separate tasks
    SplitAllTask,
    // Split given task (task_id)
    SplitGivenTask(usize),
}

pub trait Strategy {
    type Context;
    fn step(&mut self, context: &Self::Context) -> Vec<StrategyAction>;
}
