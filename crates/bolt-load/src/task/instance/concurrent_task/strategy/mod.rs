use std::{ops::Range, time::Duration};

use bolt_load_utils::telemetry::*;

mod concurrency_control_strategy;
mod dynamic_strategy;
mod runner_outcome_sampler;
mod write_backpressure_strategy;

use concurrency_control_strategy::{ConcurrencyControlContext, ConcurrencyControlStrategy};
use dynamic_strategy::{DynamicStrategy, DynamicStrategyContext};
pub use runner_outcome_sampler::{FailureKind as RunnerFailureKind, RunnerOutcomeSampler};
use write_backpressure_strategy::{WriteBackpressureContext, WriteBackpressureStrategy};
pub use write_backpressure_strategy::WriteBackpressureConfig;

pub const DEFAULT_STRATEGY_TICK_INTERVAL: Duration = Duration::from_millis(1500); // 1.5 seconds

#[derive(Debug)]
pub enum StrategyAction {
    /// Change the max concurrency
    /// This is used to change the max concurrency of the task
    ChangeMaxConcurrency(usize),
    /// Split all task into two separate tasks, with minimum chunk size
    SplitAllTask(u64),
    /// Split given task (task_id)
    SplitGivenTask(usize),
    /// Create background runners for specific range
    /// This is used to create background runners for specific range
    /// to speed up the download process
    CreateTask(Range<u64>),
}

pub trait Strategy {
    type Context<'a>
    where
        Self: 'a;

    fn name() -> &'static str;
    fn step(&mut self, context: &mut Self::Context<'_>) -> Vec<StrategyAction>;
}

pub struct StrategyControl {
    dynamic_strategy: DynamicStrategy,
    concurrency_control_strategy: ConcurrencyControlStrategy,
    write_backpressure_strategy: WriteBackpressureStrategy,
}

impl StrategyControl {
    pub fn new(initial_max_concurrency: usize) -> Self {
        Self {
            dynamic_strategy: DynamicStrategy::default(),
            concurrency_control_strategy: ConcurrencyControlStrategy::with_default_config(
                initial_max_concurrency,
            ),
            write_backpressure_strategy: WriteBackpressureStrategy::with_default_config(),
        }
    }

    pub fn execute(
        &mut self,
        max_concurrency: usize,
        current_speed: f64,
        per_runner_avg_speed: f64,
        runner_manager: &mut super::RunnerManager,
        write_queue_depth: usize,
        write_speed: f64,
    ) -> Option<(&'static str, Vec<StrategyAction>)> {
        // Priority 1: Check write backpressure first (highest priority)
        let active_runners = runner_manager.get_active_runners_count();
        let mut write_context = WriteBackpressureContext {
            queue_depth: write_queue_depth,
            write_speed,
            download_speed: current_speed,
            current_concurrency: active_runners,
            max_concurrency,
        };
        trace!("[STRATEGY] Write backpressure strategy context: {write_context:?}");
        let actions = self.write_backpressure_strategy.step(&mut write_context);

        if !actions.is_empty() {
            return Some((WriteBackpressureStrategy::name(), actions));
        }

        // Priority 2: Concurrency control based on failure rate
        let available_chunks = runner_manager.get_available_ranges();
        let mut context = ConcurrencyControlContext {
            max_concurrency,
            active_runners,
            sampler: runner_manager.runner_outcome_sampler_mut(),
            available_chunks,
        };
        trace!("[STRATEGY] Concurrency control strategy context: {context:?}");
        let actions = self.concurrency_control_strategy.step(&mut context);

        if !actions.is_empty() {
            return Some((ConcurrencyControlStrategy::name(), actions));
        }

        // TODO: move this to dynamic strategy inner?
        let planned_chunk_size = (current_speed * 2.0) as u64;

        if let Some((runner_id, suggested_range)) =
            runner_manager.find_chunk_to_split(planned_chunk_size)
            && suggested_range.end - suggested_range.start >= planned_chunk_size
        {
            let runner_id =
                runner_id.expect("A empty chunk is not processed by concurrency control");
            // split the chunk
            let mut strategy_context = DynamicStrategyContext {
                speed: current_speed,
                per_runner_speed: per_runner_avg_speed,
                current_concurrency: active_runners,
                max_concurrency,
                remaining_largest_runner_id: runner_id,
                planned_chunk_size,
            };
            trace!("[STRATEGY] Dynamic strategy context: {strategy_context:?}");
            let actions = self.dynamic_strategy.step(&mut strategy_context);
            if !actions.is_empty() {
                return Some((DynamicStrategy::name(), actions));
            }
        }

        None
    }
}
