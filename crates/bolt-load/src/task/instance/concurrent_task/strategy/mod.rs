use std::{
    ops::Range,
    time::{Duration, Instant},
};

use bolt_load_utils::telemetry::*;

mod backoff;
mod concurrency_control_strategy;
mod doubling_strategy;
mod failure_policy;
mod runner_outcome_sampler;
mod slow_restart;

pub use backoff::Backoff;
#[cfg(test)]
pub use backoff::MIN_RANGE_BACKOFF;
use concurrency_control_strategy::{ConcurrencyControlContext, ConcurrencyControlStrategy};
use doubling_strategy::{DoublingStrategy, DoublingStrategyContext};
#[cfg(test)]
pub use doubling_strategy::{GROW_INTERVAL, MIN_GROWTH_GAIN};
pub use doubling_strategy::{TickSample, planned_split_size};
pub use failure_policy::FailurePolicy;
pub(super) use failure_policy::{charges_range, is_restart};
pub use runner_outcome_sampler::{FailureKind as RunnerFailureKind, RunnerOutcomeSampler};
pub use slow_restart::{
    JudgedRunner, SILENT_RESTART_AFTER, SLOW_WINDOW, SlowRestartContext, slow_runner,
};

pub const DEFAULT_STRATEGY_TICK_INTERVAL: Duration = Duration::from_millis(1500); // 1.5 seconds

#[derive(Debug)]
pub enum StrategyAction {
    /// Change the max concurrency
    /// This is used to change the max concurrency of the task
    ChangeMaxConcurrency(usize),
    /// Create background runners for specific range
    /// This is used to create background runners for specific range
    /// to speed up the download process
    CreateTask(Range<u64>),
    /// Add up to this many runners, each splitting the running chunk with
    /// the most left to download (design §9.2). The loop reports what it
    /// added with [`StrategyControl::on_grown`].
    Grow(usize),
}

pub trait Strategy {
    type Context<'a>
    where
        Self: 'a;

    fn name() -> &'static str;
    fn step(&mut self, context: &mut Self::Context<'_>) -> Vec<StrategyAction>;
}

pub struct StrategyControl {
    /// Adds the runners (design §9.2).
    doubling_strategy: DoublingStrategy,
    concurrency_control_strategy: ConcurrencyControlStrategy,
}

impl StrategyControl {
    pub fn new(initial_max_concurrency: usize) -> Self {
        Self {
            doubling_strategy: DoublingStrategy::default(),
            concurrency_control_strategy: ConcurrencyControlStrategy::with_default_config(
                initial_max_concurrency,
            ),
        }
    }

    /// Step the strategies at `now`.
    ///
    /// While `congested`, no action opens a connection: no runner is
    /// created and none is split. No runner is created for a range still
    /// backing off (design §8.1).
    ///
    /// The doubling strategy steps on `tick`, the runners over the tick
    /// that ends at `now`.
    pub fn execute(
        &mut self,
        max_concurrency: usize,
        runner_manager: &mut super::RunnerManager,
        now: Instant,
        congested: bool,
        tick: &TickSample,
    ) -> Option<(&'static str, Vec<StrategyAction>)> {
        let mut context = Self::context(max_concurrency, runner_manager, now, congested);
        let active_runners = context.active_runners;
        trace!("[STRATEGY] Concurrency control strategy context: {context:?}");
        let actions = self.concurrency_control_strategy.step(&mut context);

        if !actions.is_empty() {
            trace!("[STRATEGY] Concurrency control strategy not none, proceeding");
            return Some((ConcurrencyControlStrategy::name(), actions));
        }

        let mut context = DoublingStrategyContext {
            now,
            active_runners,
            max_concurrency,
            congested,
            all_delivering: tick.all_delivering,
            silent_runners: tick.silent,
            speed: tick.speed,
            // No speed limit before design §10.
            speed_limit: None,
        };
        trace!("[STRATEGY] Doubling strategy context: {context:?}");
        let actions = self.doubling_strategy.step(&mut context);
        (!actions.is_empty()).then(|| (DoublingStrategy::name(), actions))
    }

    /// The loop stopped a runner to restart its range (design §9.5).
    pub fn on_restarted(&mut self) {
        self.doubling_strategy.on_restarted();
    }

    /// The loop added `added` runners for a [`StrategyAction::Grow`].
    pub fn on_grown(&mut self, added: usize) {
        self.doubling_strategy.on_grown(added);
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
        congested: bool,
    ) -> Vec<StrategyAction> {
        if congested {
            return Vec::new();
        }
        let context = Self::context(max_concurrency, runner_manager, now, congested);
        self.concurrency_control_strategy.create_tasks(&context)
    }

    fn context(
        max_concurrency: usize,
        runner_manager: &mut super::RunnerManager,
        now: Instant,
        congested: bool,
    ) -> ConcurrencyControlContext<'_> {
        let active_runners = runner_manager.get_active_runners_count();
        // No new connection while congested.
        let available_chunks = if congested {
            Vec::new()
        } else {
            runner_manager.get_eligible_ranges(now)
        };
        ConcurrencyControlContext {
            max_concurrency,
            active_runners,
            sampler: runner_manager.runner_outcome_sampler_mut(),
            available_chunks,
            now,
        }
    }
}
