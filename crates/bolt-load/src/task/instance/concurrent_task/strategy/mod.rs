use std::{
    ops::Range,
    time::{Duration, Instant},
};

use bolt_load_utils::telemetry::*;

mod admission_evidence;
mod backoff;
mod concurrency_control_strategy;
mod dynamic_strategy;
mod failure_policy;
mod runner_outcome_sampler;

pub use admission_evidence::{AdmissionEvidence, AttemptPhase, classify, streak_gap};
pub use backoff::Backoff;
#[cfg(test)]
pub use backoff::MIN_RANGE_BACKOFF;
use concurrency_control_strategy::{
    ConcurrencyControlConfig, ConcurrencyControlContext, ConcurrencyControlStrategy,
};
use dynamic_strategy::{DynamicStrategy, DynamicStrategyContext};
pub use failure_policy::FailurePolicy;
pub use runner_outcome_sampler::{FailureKind as RunnerFailureKind, RunnerOutcomeSampler};

use super::chunk_planner::SplitCandidate;

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

/// What the loop knows at a strategy step besides the runners.
#[derive(Debug, Clone, Copy)]
pub struct LoopSignals {
    /// Whether a congestion holds back new connections.
    pub congested: bool,
    /// When a non-empty frame was last accepted.
    pub last_progress_at: Instant,
    /// Whether the writer can take data.
    pub writer_ready: bool,
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
}

impl StrategyControl {
    pub fn new(initial_max_concurrency: usize) -> Self {
        Self {
            dynamic_strategy: DynamicStrategy::default(),
            concurrency_control_strategy: ConcurrencyControlStrategy::new(
                ConcurrencyControlConfig::default(),
                initial_max_concurrency,
            ),
        }
    }

    /// Step the strategies at `now`.
    ///
    /// While `signals.congested`, no action opens a connection: no runner
    /// is created and none is split. No runner is created for a range still
    /// backing off (design §8.1).
    pub fn execute(
        &mut self,
        max_concurrency: usize,
        current_speed: f64,
        per_runner_avg_speed: f64,
        runner_manager: &mut super::RunnerManager,
        now: Instant,
        signals: LoopSignals,
    ) -> Option<(&'static str, Vec<StrategyAction>)> {
        let congested = signals.congested;
        let mut context = Self::context(max_concurrency, runner_manager, now, signals);
        let active_runners = context.active_runners;
        trace!("[STRATEGY] Concurrency control strategy context: {context:?}");
        let actions = self.concurrency_control_strategy.step(&mut context);

        if !actions.is_empty() {
            trace!("[STRATEGY] Concurrency control strategy not none, proceeding");
            return Some((ConcurrencyControlStrategy::name(), actions));
        }

        if congested {
            trace!("[STRATEGY] Congested, not splitting");
            return None;
        }

        // TODO: move this to dynamic strategy inner?
        let planned_chunk_size = (current_speed * 2.0) as u64;

        // An unassigned range is left to the concurrency control, which
        // creates runners for it.
        if active_runners < max_concurrency
            && let Some(SplitCandidate::Running {
                runner: runner_id,
                range: suggested_range,
            }) = runner_manager.find_chunk_to_split(planned_chunk_size, now)
            && suggested_range.end - suggested_range.start >= planned_chunk_size
        {
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

    /// The runners to create between ticks, for ranges whose backoff, or
    /// the task's congestion, ended at `now` (design §8.1).
    ///
    /// No strategy steps: the runners are only those the concurrency
    /// control would create next, see
    /// [`ConcurrencyControlStrategy::create_tasks`].
    pub fn create_tasks(
        &self,
        max_concurrency: usize,
        runner_manager: &mut super::RunnerManager,
        now: Instant,
        signals: LoopSignals,
    ) -> Vec<StrategyAction> {
        if signals.congested {
            return Vec::new();
        }
        let context = Self::context(max_concurrency, runner_manager, now, signals);
        self.concurrency_control_strategy.create_tasks(&context)
    }

    fn context(
        max_concurrency: usize,
        runner_manager: &mut super::RunnerManager,
        now: Instant,
        signals: LoopSignals,
    ) -> ConcurrencyControlContext<'_> {
        let active_runners = runner_manager.get_active_runners_count();
        let proven_runners = runner_manager.get_proven_runners_count();
        // No new connection while congested.
        let available_chunks = if signals.congested {
            Vec::new()
        } else {
            runner_manager.get_eligible_ranges(now)
        };
        let (sampler, admission) = runner_manager.outcome_sources_mut();
        ConcurrencyControlContext {
            max_concurrency,
            active_runners,
            proven_runners,
            sampler,
            admission,
            available_chunks,
            last_progress_at: signals.last_progress_at,
            writer_ready: signals.writer_ready,
            now,
        }
    }
}
