use std::{collections::BTreeMap, ops::Range};

use crate::{manager::strategy::StrategyManager, runner::TaskRunner};

use super::RunnerId;

pub struct RunnerChunks {
    map: BTreeMap<RunnerId, Vec<Range<u64>>>,
}

pub struct ConcurrentTask {
    /// the runners of this task
    runners: Vec<TaskRunner>,
    /// the strategy manager of this task
    strategy_manager: StrategyManager,
    /// The downloaded chunks of this task
    /// It is None if the task is in single-thread/singleton mode
    downloaded_chunks: Vec<Range<u64>>,
}
