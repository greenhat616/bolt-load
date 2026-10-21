use std::{
    ops::Range,
    task::{Context, Poll},
};

use bolt_load_utils::telemetry::*;

use super::{super::strategy::StrategyAction, DownloadLoop, LoopEvent};
use crate::{runtime::EventTimer, task::RunnerId};

/// The strategy's periodic tick.
pub(super) struct StrategyTick {
    timer: EventTimer,
}

impl StrategyTick {
    pub(super) fn new(timer: EventTimer) -> Self {
        Self { timer }
    }

    #[inline]
    pub(super) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<LoopEvent> {
        self.timer.poll_tick(cx).map(|()| LoopEvent::StrategyTimer)
    }
}

impl DownloadLoop {
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    pub(super) fn on_strategy_tick(&mut self) {
        let Some((strategy_name, actions)) = self.strategy.execute(
            self.max_concurrency,
            self.throughput.speed,
            self.throughput.per_runner_speed,
            &mut self.manager,
        ) else {
            return;
        };
        for action in actions {
            trace!("[TASK] applying strategy: {strategy_name}, action: {action:?}");
            self.apply(action);
        }
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    pub(super) fn apply(&mut self, action: StrategyAction) {
        match action {
            // Split current all task into two separate tasks
            StrategyAction::SplitAllTask(planned_chunk_size) => {
                trace!("[TASK] Strategy: split all task");
                let states = self.manager.get_incomplete_states(Some(planned_chunk_size));
                for (runner_id, incomplete_range) in states {
                    trace!("split task: {runner_id}, {incomplete_range:?}");
                    self.split(runner_id, incomplete_range);
                }
            }
            // Split the given task into two separate tasks
            StrategyAction::SplitGivenTask(task_id) => {
                trace!("[TASK] Strategy: split given task: {task_id}");
                let incomplete_range = self
                    .manager
                    .get_runner_state(task_id)
                    .map(|s| s.incomplete_range())
                    .expect("task id not found");
                self.split(task_id, incomplete_range);
            }
            // Change the max concurrency
            StrategyAction::ChangeMaxConcurrency(new_max_concurrency) => {
                trace!("[TASK] Strategy: change max concurrency: {new_max_concurrency}");
                self.max_concurrency = new_max_concurrency;
            }
            // Create background runners for specific range
            StrategyAction::CreateTask(range) => {
                trace!("[TASK] Strategy: create task: {range:?}");
                self.spawn_runner(range);
            }
        }
    }

    /// Split the given runner's incomplete range in two, spawning a runner for the second half.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, fields(runner_id, range))
    )]
    pub(super) fn split(&mut self, runner_id: RunnerId, incomplete_range: Range<u64>) {
        let half_size = (incomplete_range.end - incomplete_range.start) / 2;
        if let Err(e) = self.manager.resize_runner(runner_id, half_size) {
            trace!("[TASK] skip splitting runner {runner_id}: {e}");
            return;
        }
        self.spawn_runner(incomplete_range.start + half_size..incomplete_range.end);
    }

    pub(super) fn spawn_runner(&mut self, range: Range<u64>) {
        let spawner = &self.spawner;
        self.manager
            .spawn_runner_with_chunk(range.clone(), |runner_id, limits| {
                spawner.spawn(range, runner_id, limits)
            })
            .expect("failed to allocate runner with chunk");
    }
}
