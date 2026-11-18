use std::{
    ops::Range,
    time::{Duration, Instant},
};

use bolt_load_utils::telemetry::*;

mod admission_evidence;
mod backoff;
mod concurrency_control_strategy;
mod doubling_strategy;
mod failure_policy;
mod slow_restart;

#[cfg(test)]
pub use admission_evidence::UNSEEN;
pub use admission_evidence::{
    AdmissionClass, AdmissionEvidence, AttemptPhase, classify, streak_gap,
};
pub use backoff::Backoff;
#[cfg(test)]
pub use backoff::MIN_RANGE_BACKOFF;
pub use concurrency_control_strategy::ConcurrencyControlConfig;
use concurrency_control_strategy::{ConcurrencyControlContext, ConcurrencyControlStrategy};
#[cfg(test)]
pub use doubling_strategy::{BASE_REPROBE_WAIT, growth_deadline};
use doubling_strategy::{DoublingStrategy, DoublingStrategyContext};
#[cfg(test)]
pub use doubling_strategy::{GROW_INTERVAL, MIN_GROWTH_GAIN};
pub use doubling_strategy::{TickSample, planned_split_size};
pub use failure_policy::FailurePolicy;
pub(super) use failure_policy::{charges_range, is_restart};
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

/// What the loop knows at a strategy step besides the runners.
#[derive(Debug, Clone, Copy)]
pub struct LoopSignals {
    /// Whether a congestion holds back new connections.
    pub congested: bool,
    /// Until when a congestion holds back new connections.
    pub congestion_until: Option<Instant>,
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
    /// Adds the runners (design §9.2).
    doubling_strategy: DoublingStrategy,
    concurrency_control_strategy: ConcurrencyControlStrategy,
    max_connections: usize,
}

impl StrategyControl {
    /// Strategies for at most `max_connections` runners, which may take
    /// `connect_timeout` to connect.
    pub fn new(max_connections: usize, connect_timeout: Duration) -> Self {
        Self {
            doubling_strategy: DoublingStrategy::new(connect_timeout),
            concurrency_control_strategy: ConcurrencyControlStrategy::new(
                ConcurrencyControlConfig::with_connect_timeout(connect_timeout),
                max_connections,
            ),
            max_connections,
        }
    }

    /// [`Self::new`], with the limit controller configured by `config`.
    #[cfg(test)]
    pub fn with_concurrency_control(
        max_connections: usize,
        connect_timeout: Duration,
        config: ConcurrencyControlConfig,
    ) -> Self {
        Self {
            concurrency_control_strategy: ConcurrencyControlStrategy::new(config, max_connections),
            ..Self::new(max_connections, connect_timeout)
        }
    }

    /// The connections the growth side wants open, see
    /// [`DoublingStrategy::want`].
    pub fn want(&self) -> usize {
        self.doubling_strategy.want(self.max_connections)
    }

    /// Hold `h` runners as a gainless doubling would, probed again at
    /// `reprobe_at`.
    #[cfg(test)]
    pub fn hold(&mut self, h: usize, reprobe_at: Instant) {
        self.doubling_strategy.set_hold(h, reprobe_at);
    }

    /// When the growth's hold is probed next; `None` while ramping.
    #[cfg(test)]
    pub fn reprobe_at(&self) -> Option<Instant> {
        self.doubling_strategy.reprobe_at()
    }

    /// The connections the limit controller lets new runners take now,
    /// whatever the growth wants: see
    /// [`ConcurrencyControlStrategy::refill_cap`]. The endgame hedge is
    /// gated by it, so it may open one connection above a hold (design
    /// §9.6, §14.33).
    pub fn limit_cap(&self, admission: &AdmissionEvidence) -> usize {
        self.concurrency_control_strategy
            .refill_cap(self.max_connections, admission)
    }

    /// The connections the runners may take now, see
    /// [`ConcurrencyControlStrategy::cap`].
    #[cfg(test)]
    pub fn cap(&self) -> usize {
        self.concurrency_control_strategy.cap(self.want())
    }

    /// The limit controller's state, see
    /// [`ConcurrencyControlStrategy::state`].
    #[cfg(test)]
    pub fn limit_state(&self) -> (usize, Option<Instant>, Option<Instant>, Duration) {
        self.concurrency_control_strategy.state()
    }

    /// Step the strategies at `now`.
    ///
    /// While `signals.congested`, no action opens a connection: no runner
    /// is created and none is split. No runner is created for a range still
    /// backing off (design §8.1). Nothing is added while the runners
    /// holding a slot, active or not released yet, reach
    /// `min(max_concurrency, cap)`, see
    /// [`ConcurrencyControlStrategy::refill_cap`], with what the growth
    /// wants: the growth asks the limit controller for connections through
    /// `want`, and adds them within its cap (design §8.3, §9.2).
    ///
    /// The doubling strategy steps on `tick`, the runners over the tick
    /// that ends at `now`.
    pub fn execute(
        &mut self,
        max_concurrency: usize,
        runner_manager: &mut super::RunnerManager,
        now: Instant,
        signals: LoopSignals,
        tick: &TickSample,
    ) -> Option<(&'static str, Vec<StrategyAction>)> {
        let congested = signals.congested;
        runner_manager.observe_connecting();
        // A probe past its deadline wants nothing more from this step on.
        self.doubling_strategy.expire(now);
        let want = self.want();
        let mut context = Self::context(want, runner_manager, now, signals);
        let active_runners = context.active_runners;
        // The slots of runners that are no longer active but not released
        // yet: no growth takes them before they stop.
        let held = context
            .allocated_runners
            .saturating_sub(context.active_runners);
        let eligible_ranges = !context.available_chunks.is_empty();
        trace!("[STRATEGY] Concurrency control strategy context: {context:?}");
        let actions = self.concurrency_control_strategy.step(&mut context);

        if !actions.is_empty() {
            trace!("[STRATEGY] Concurrency control strategy not none, proceeding");
            return Some((ConcurrencyControlStrategy::name(), actions));
        }

        // Growth adds runners up to the limit controller's cap, never above
        // it, short of a running step's trial once a refusal marked it
        // (design §8.3); within it, the growth keeps to what it wants.
        let limit_cap = self
            .limit_cap(runner_manager.admission_mut())
            .min(max_concurrency);
        let mut context = DoublingStrategyContext {
            now,
            active_runners,
            max_concurrency: limit_cap,
            held,
            eligible_ranges,
            congested,
            all_delivering: tick.all_delivering,
            silent_runners: tick.silent,
            stable: tick.stable,
            speed: tick.speed,
            // No speed limit before design §10.
            speed_limit: None,
        };
        trace!("[STRATEGY] Doubling strategy context: {context:?}");
        let actions = self.doubling_strategy.step(&mut context);
        (!actions.is_empty()).then(|| (DoublingStrategy::name(), actions))
    }

    /// The loop stopped a runner at `now` to restart its range (design
    /// §9.5).
    pub fn on_restarted(&mut self, now: Instant) {
        self.doubling_strategy.on_restarted(now);
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
        runner_manager: &mut super::RunnerManager,
        now: Instant,
        signals: LoopSignals,
    ) -> Vec<StrategyAction> {
        if signals.congested {
            return Vec::new();
        }
        let context = Self::context(self.want(), runner_manager, now, signals);
        self.concurrency_control_strategy.create_tasks(&context)
    }

    fn context(
        want: usize,
        runner_manager: &mut super::RunnerManager,
        now: Instant,
        signals: LoopSignals,
    ) -> ConcurrencyControlContext<'_> {
        let active_runners = runner_manager.get_active_runners_count();
        let allocated_runners = runner_manager.allocated_runner_count();
        let proven_runners = runner_manager.get_proven_runners_count();
        // No new connection while congested.
        let available_chunks = if signals.congested {
            Vec::new()
        } else {
            runner_manager.get_eligible_ranges(now)
        };
        ConcurrencyControlContext {
            want,
            active_runners,
            allocated_runners,
            proven_runners,
            admission: runner_manager.admission_mut(),
            available_chunks,
            congestion_until: signals.congestion_until,
            last_progress_at: signals.last_progress_at,
            writer_ready: signals.writer_ready,
            now,
        }
    }
}
