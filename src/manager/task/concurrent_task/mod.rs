use std::{collections::BTreeMap, ops::Range};

use bytes::Bytes;
use smol_cancellation_token::CancellationToken;

use crate::{
    adapter::AnyAdapter,
    manager::{Progress, RunnerId, strategy::StrategyManager},
    runner::TaskRunner,
};

use super::{Result, Task};

mod file;

/// start with 4 runners, and then try to use 8 threads if needed
const INITIAL_TASK_RUNNER_COUNT: usize = 4;

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

impl Task for ConcurrentTask {
    async fn start(&mut self, adapter: &AnyAdapter, cancel_token: CancellationToken) -> Result<()> {
        todo!()
    }
    async fn stop(&mut self) -> Result<()> {
        todo!()
    }
    fn inspect_progress(&self) -> Progress {
        todo!()
    }
}
