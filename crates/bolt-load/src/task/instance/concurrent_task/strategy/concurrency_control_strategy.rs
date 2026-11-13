//! The Strategy that controls the max concurrency of the task, from what
//! failed connects tell about the server's connection limit (design §8.3)

use std::{
    ops::Range,
    time::{Duration, Instant},
};

use super::{
    DEFAULT_STRATEGY_TICK_INTERVAL, Strategy, StrategyAction,
    admission_evidence::AdmissionEvidence,
    runner_outcome_sampler::{OutcomeStats, RunnerOutcomeSampler},
};

/// Corroborated refusals in a row before the silent rule lowers the limit.
const REFUSALS_IN_A_ROW: usize = 3;

#[derive(Debug, Clone)]
/// The configuration for the concurrency control strategy
pub struct ConcurrencyControlConfig {
    /// The threshold for triggering degradation
    pub failure_rate_threshold: f64,
    /// The time to wait for recovery after degradation
    pub recovery_delay: Duration,
    /// The observation period for recovery
    pub recovery_observation_period: Duration,
    /// The number of concurrency to decrease each time of degradation
    pub degradation_step: usize,
    /// The number of concurrency to increase each time of recovery
    pub recovery_step: usize,
    /// The minimum concurrency
    pub min_concurrency: usize,
    /// The interval of the strategy tick: progress one interval after the
    /// third refusal confirms a streak, and a tick more than two intervals
    /// after the previous one is late.
    pub tick_interval: Duration,
}

impl Default for ConcurrencyControlConfig {
    fn default() -> Self {
        Self {
            failure_rate_threshold: 0.3,
            recovery_delay: Duration::from_secs(30),
            recovery_observation_period: Duration::from_secs(15),
            degradation_step: 1,
            recovery_step: 1,
            min_concurrency: 1,
            tick_interval: DEFAULT_STRATEGY_TICK_INTERVAL,
        }
    }
}

/// The context for the concurrency control strategy
#[derive(derive_more::Debug)]
pub struct ConcurrencyControlContext<'a> {
    /// The current maximum concurrency
    pub max_concurrency: usize,
    /// The current active runner count
    pub active_runners: usize,
    /// The runners that are running and had bytes accepted
    pub proven_runners: usize,
    /// The sampler
    #[debug(skip)]
    pub sampler: &'a mut RunnerOutcomeSampler,
    /// What ended attempts tell about the connections the server admits
    #[debug(skip)]
    pub admission: &'a mut AdmissionEvidence,
    /// The ranges a runner may be created for now: none while a congestion
    /// holds back new connections, and no range still backing off.
    pub available_chunks: Vec<Range<u64>>,
    /// When a non-empty frame was last accepted
    pub last_progress_at: Instant,
    /// Whether the writer can take data
    pub writer_ready: bool,
    /// The time of this step
    pub now: Instant,
}

/// The state of the concurrency control strategy
#[derive(Debug, Clone, Copy, PartialEq)]
enum StrategyState {
    /// Normal operation
    Normal,
    /// Degraded
    Degraded {
        target_concurrency: usize,
        degraded_at: Instant,
    },
    /// Recovering
    Recovering {
        trial_concurrency: usize,
        started_at: Instant,
        /// The number of failures at the start of recovery, for comparison
        initial_failures: usize,
    },
}

#[derive(Debug)]
pub struct ConcurrencyControlStrategy {
    config: ConcurrencyControlConfig,
    state: StrategyState,
    initial_max_concurrency: usize,
    first_execute: bool,
    /// The previous step's time, to tell a late tick.
    last_tick: Option<Instant>,
}

impl ConcurrencyControlStrategy {
    pub fn new(config: ConcurrencyControlConfig, initial_max_concurrency: usize) -> Self {
        Self {
            config,
            state: StrategyState::Normal,
            initial_max_concurrency,
            first_execute: true,
            last_tick: None,
        }
    }

    #[cfg(test)]
    pub fn with_default_config(initial_max_concurrency: usize) -> Self {
        Self::new(ConcurrencyControlConfig::default(), initial_max_concurrency)
    }

    /// The congestion rule: a 429/503 lowers the limit to the active
    /// runners, and never raises it.
    #[inline]
    fn handle_congestion(
        &mut self,
        context: &mut ConcurrencyControlContext,
        now: Instant,
    ) -> Vec<StrategyAction> {
        // Never degrade to zero: with no active runner nothing would be scheduled again.
        let new_concurrency = context.max_concurrency.min(
            context
                .active_runners
                .max(self.config.min_concurrency)
                .max(1),
        );
        context.admission.on_lowered(new_concurrency);
        self.state = StrategyState::Degraded {
            target_concurrency: new_concurrency,
            degraded_at: now,
        };

        vec![StrategyAction::ChangeMaxConcurrency(new_concurrency)]
    }

    /// The silent rule: the limit the evidence lowers the current one to,
    /// if any.
    ///
    /// Three corroborated refusals in a row, a frame accepted at least one
    /// tick interval after the third, some but fewer runners connected than
    /// the limit allows, and a writer that takes data: the server admits
    /// the connected runners. Nothing at a late tick, where queued events
    /// may be about to change what the evidence shows.
    fn silent_limit(&self, context: &ConcurrencyControlContext, late: bool) -> Option<usize> {
        let admission = &*context.admission;
        if late || !context.writer_ready || admission.streak_count() < REFUSALS_IN_A_ROW {
            return None;
        }
        let third = admission.third_refusal_at()?;
        if context.last_progress_at < third + self.config.tick_interval {
            return None;
        }
        let connected = admission.connected();
        (1..context.max_concurrency)
            .contains(&connected)
            .then(|| connected.max(self.config.min_concurrency).max(1))
    }

    /// Lower the limit to `new_concurrency` by the silent rule.
    fn lower_silently(
        &mut self,
        context: &mut ConcurrencyControlContext,
        new_concurrency: usize,
        now: Instant,
        actions: &mut Vec<StrategyAction>,
    ) {
        bolt_load_utils::telemetry::trace!(
            "[CONTROL STRATEGY] refused connects: lowered to {new_concurrency}"
        );
        context.admission.on_lowered(new_concurrency);
        self.state = StrategyState::Degraded {
            target_concurrency: new_concurrency,
            degraded_at: now,
        };
        actions.push(StrategyAction::ChangeMaxConcurrency(new_concurrency));
    }

    fn handle_degraded_state(
        &mut self,
        stats: &OutcomeStats,
        now: Instant,
        target_concurrency: usize,
        degraded_at: Instant,
        actions: &mut Vec<StrategyAction>,
    ) {
        // if the recovery delay has not passed, do not recover
        if now.duration_since(degraded_at) < self.config.recovery_delay {
            return;
        }

        let recovery_threshold = self.config.failure_rate_threshold * 0.5;
        if stats.failure_rate() < recovery_threshold {
            let trial =
                (target_concurrency + self.config.recovery_step).min(self.initial_max_concurrency);

            if trial > target_concurrency {
                self.state = StrategyState::Recovering {
                    trial_concurrency: trial,
                    started_at: now,
                    initial_failures: stats.total_failures(),
                };
                actions.push(StrategyAction::ChangeMaxConcurrency(trial));
            } else {
                // if the trial concurrency is already the maximum value, return to normal state
                self.state = StrategyState::Normal;
            }
        }
    }

    fn handle_recovering_state(
        &mut self,
        stats: &OutcomeStats,
        now: Instant,
        trial_concurrency: usize,
        started_at: Instant,
        initial_failures: usize,
        actions: &mut Vec<StrategyAction>,
    ) {
        let elapsed = now.duration_since(started_at);

        // if the failure rate has increased during recovery, immediately rollback
        if stats.failure_rate() >= self.config.failure_rate_threshold {
            let fallback = trial_concurrency
                .saturating_sub(self.config.degradation_step)
                .max(self.config.min_concurrency);

            self.state = StrategyState::Degraded {
                target_concurrency: fallback,
                degraded_at: now,
            };
            actions.push(StrategyAction::ChangeMaxConcurrency(fallback));
            return;
        }

        if elapsed >= self.config.recovery_observation_period {
            // if there are new failures during recovery, continue recovery
            let new_failures = stats.total_failures().saturating_sub(initial_failures);
            let recovery_samples = stats.total.saturating_sub(initial_failures);

            // if the failure rate is low during recovery, continue recovery
            let recovery_failure_rate = if recovery_samples > 0 {
                new_failures as f64 / recovery_samples as f64
            } else {
                0.0
            };

            if recovery_failure_rate < self.config.failure_rate_threshold {
                // if the recovery is successful, continue recovery
                if trial_concurrency < self.initial_max_concurrency {
                    // continue to increase the concurrency
                    let next_trial = (trial_concurrency + self.config.recovery_step)
                        .min(self.initial_max_concurrency);

                    self.state = StrategyState::Recovering {
                        trial_concurrency: next_trial,
                        started_at: now,
                        initial_failures: stats.total_failures(),
                    };
                    actions.push(StrategyAction::ChangeMaxConcurrency(next_trial));
                } else {
                    // if the recovery is successful, return to normal state
                    self.state = StrategyState::Normal;
                }
            } else {
                // if the recovery is failed, rollback the concurrency
                let fallback = trial_concurrency
                    .saturating_sub(self.config.recovery_step)
                    .max(self.config.min_concurrency);

                self.state = StrategyState::Degraded {
                    target_concurrency: fallback,
                    degraded_at: now,
                };
                actions.push(StrategyAction::ChangeMaxConcurrency(fallback));
            }
        }
    }

    /// The runners a step would create once the concurrency needs no
    /// change: one per available range, up to the free slots. Changes no
    /// state.
    ///
    /// None before the first step, which creates the first runners, nor
    /// while a congestion waits for the next step to lower the limit:
    /// runners created before it would be counted as the new limit.
    pub fn create_tasks(&self, context: &ConcurrencyControlContext) -> Vec<StrategyAction> {
        if self.first_execute || context.admission.congestion_pending() {
            return Vec::new();
        }
        Self::fill_free_slots(context)
    }

    /// One runner per available range, up to the free slots.
    fn fill_free_slots(context: &ConcurrencyControlContext) -> Vec<StrategyAction> {
        context
            .available_chunks
            .iter()
            .take(
                context
                    .max_concurrency
                    .saturating_sub(context.active_runners),
            )
            .map(|chunk| StrategyAction::CreateTask(chunk.clone()))
            .collect()
    }

    /// Get the current state
    #[expect(dead_code, reason = "debugging aid for the strategy state")]
    pub fn current_state(&self) -> &str {
        match self.state {
            StrategyState::Normal => "normal",
            StrategyState::Degraded { .. } => "degraded",
            StrategyState::Recovering { .. } => "recovering",
        }
    }
}

impl Strategy for ConcurrencyControlStrategy {
    type Context<'a>
        = ConcurrencyControlContext<'a>
    where
        Self: 'a;

    fn name() -> &'static str {
        "concurrency_control"
    }

    #[inline]
    fn step(&mut self, context: &mut Self::Context<'_>) -> Vec<StrategyAction> {
        let now = context.now;
        let late = self.last_tick.replace(now).is_some_and(|last| {
            now.saturating_duration_since(last) > 2 * self.config.tick_interval
        });

        // When first launch the task,
        // Create background runners for all available chunks according to the current concurrency
        if self.first_execute {
            self.first_execute = false;
            return context
                .available_chunks
                .iter()
                .take(context.max_concurrency)
                .map(|chunk| StrategyAction::CreateTask(chunk.clone()))
                .collect();
        }

        // A streak with no refusal for its gap ends at the stamp.
        context.admission.stamp(now);
        context.admission.observe_proven(context.proven_runners);

        if context.admission.take_congestion() {
            bolt_load_utils::telemetry::trace!("[CONTROL STRATEGY] congestion, degraded");
            return self.handle_congestion(context, now);
        }

        let mut actions = Vec::new();
        let stats = context.sampler.stats();

        match self.state {
            StrategyState::Normal => {
                if let Some(new_concurrency) = self.silent_limit(context, late) {
                    self.lower_silently(context, new_concurrency, now, &mut actions);
                }
            }
            StrategyState::Degraded {
                target_concurrency,
                degraded_at,
            } => {
                if let Some(new_concurrency) = self.silent_limit(context, late) {
                    self.lower_silently(context, new_concurrency, now, &mut actions);
                } else {
                    self.handle_degraded_state(
                        &stats,
                        now,
                        target_concurrency,
                        degraded_at,
                        &mut actions,
                    );
                }
            }
            StrategyState::Recovering {
                trial_concurrency,
                started_at,
                initial_failures,
            } => {
                self.handle_recovering_state(
                    &stats,
                    now,
                    trial_concurrency,
                    started_at,
                    initial_failures,
                    &mut actions,
                );
            }
        }

        if !actions.is_empty() {
            return actions;
        }

        // When the active runners are less than the max concurrency, create background runners for all available chunks
        Self::fill_free_slots(context)
    }
}

#[cfg(test)]
mod tests {
    use super::{super::admission_evidence::AdmissionClass, *};

    const TICK: Duration = DEFAULT_STRATEGY_TICK_INTERVAL;
    const SECOND: Duration = Duration::from_secs(1);

    /// The evidence and the sampler a context borrows.
    #[derive(Default)]
    struct Sources {
        sampler: Option<RunnerOutcomeSampler>,
        admission: AdmissionEvidence,
    }

    impl Sources {
        fn context(
            &mut self,
            max_concurrency: usize,
            active_runners: usize,
            available_chunks: Vec<Range<u64>>,
            now: Instant,
        ) -> ConcurrencyControlContext<'_> {
            ConcurrencyControlContext {
                max_concurrency,
                active_runners,
                proven_runners: active_runners,
                sampler: self
                    .sampler
                    .get_or_insert_with(RunnerOutcomeSampler::with_defaults),
                admission: &mut self.admission,
                available_chunks,
                last_progress_at: now,
                writer_ready: true,
                now,
            }
        }
    }

    fn congest(admission: &mut AdmissionEvidence) {
        admission.on_attempt_ended(AdmissionClass::Congestion, 1, 1);
    }

    #[test]
    #[expect(clippy::single_range_in_vec_init, reason = "a list holding one range")]
    fn test_immediate_degradation_never_drops_to_zero() {
        let mut strategy = ConcurrencyControlStrategy::with_default_config(4);
        let mut sources = Sources::default();
        let mut context = sources.context(4, 0, vec![0..100], Instant::now());
        // The first step only schedules the initial runners.
        strategy.step(&mut context);

        congest(context.admission);
        context.now = Instant::now();
        let actions = strategy.step(&mut context);
        assert!(
            matches!(actions.as_slice(), [StrategyAction::ChangeMaxConcurrency(n)] if *n >= 1),
            "unexpected actions: {actions:?}"
        );
    }

    fn created(actions: &[StrategyAction]) -> Vec<Range<u64>> {
        actions
            .iter()
            .map(|action| match action {
                StrategyAction::CreateTask(range) => range.clone(),
                other => panic!("unexpected action: {other:?}"),
            })
            .collect()
    }

    /// Between steps, runners are created only as a step would create them,
    /// and only once the step lowered the limit for a congestion.
    #[test]
    #[expect(clippy::single_range_in_vec_init, reason = "a list holding one range")]
    fn test_create_tasks_waits_for_the_first_step_and_the_degradation() {
        let mut strategy = ConcurrencyControlStrategy::with_default_config(4);
        let mut sources = Sources::default();
        let mut context =
            sources.context(4, 1, vec![0..10, 20..30, 40..50, 60..70], Instant::now());
        assert!(strategy.create_tasks(&context).is_empty());
        strategy.step(&mut context);

        // Three free slots.
        assert_eq!(
            created(&strategy.create_tasks(&context)),
            [0..10, 20..30, 40..50]
        );
        context.active_runners = 4;
        assert!(strategy.create_tasks(&context).is_empty());

        context.active_runners = 1;
        congest(context.admission);
        context.now = Instant::now();
        assert!(strategy.create_tasks(&context).is_empty());
        // Still pending: `create_tasks` left it to the step.
        let actions = strategy.step(&mut context);
        assert!(
            matches!(
                actions.as_slice(),
                [StrategyAction::ChangeMaxConcurrency(1)]
            ),
            "unexpected actions: {actions:?}"
        );
        context.max_concurrency = 1;
        context.active_runners = 0;
        assert_eq!(created(&strategy.create_tasks(&context)), [0..10]);
    }

    /// A strategy past its first step at `t0`, with a limit of `limit`,
    /// and `connected` runners connected.
    fn started(
        limit: usize,
        connected: usize,
        t0: Instant,
    ) -> (ConcurrencyControlStrategy, Sources) {
        let mut strategy = ConcurrencyControlStrategy::with_default_config(limit);
        let mut sources = Sources::default();
        for id in 0..connected {
            sources.admission.on_started(id, id);
        }
        strategy.step(&mut sources.context(limit, connected, Vec::new(), t0));
        (strategy, sources)
    }

    /// `n` refusals, each with `connected` others in flight and one proven.
    fn refuse(admission: &mut AdmissionEvidence, n: usize) {
        let others = admission.connected();
        for _ in 0..n {
            admission.on_attempt_ended(AdmissionClass::Refused, 1, others);
        }
    }

    /// The step at `now`, after the last progress at `progress`; returns
    /// the new limit, if the step changed it.
    fn tick_at(
        strategy: &mut ConcurrencyControlStrategy,
        sources: &mut Sources,
        limit: usize,
        now: Instant,
        progress: Instant,
    ) -> Option<usize> {
        let active = sources.admission.connected();
        let mut context = sources.context(limit, active, Vec::new(), now);
        context.last_progress_at = progress;
        changed_limit(&strategy.step(&mut context))
    }

    fn changed_limit(actions: &[StrategyAction]) -> Option<usize> {
        actions.iter().find_map(|action| match action {
            StrategyAction::ChangeMaxConcurrency(n) => Some(*n),
            _ => None,
        })
    }

    #[test]
    fn test_three_refusals_and_progress_lower_the_limit() {
        let t0 = Instant::now();
        let (mut strategy, mut sources) = started(16, 4, t0);
        refuse(&mut sources.admission, 2);
        sources.admission.stamp(t0);
        // Two are not enough, whatever the progress.
        assert_eq!(
            tick_at(&mut strategy, &mut sources, 16, t0 + TICK, t0 + TICK),
            None
        );
        refuse(&mut sources.admission, 1);
        let third = t0 + TICK + SECOND;
        sources.admission.stamp(third);
        // Progress before one tick interval after the third does not
        // confirm it.
        for now in [third + SECOND, third + TICK] {
            assert_eq!(
                tick_at(
                    &mut strategy,
                    &mut sources,
                    16,
                    now,
                    third + TICK - SECOND / 2
                ),
                None,
                "at {:?}",
                now - third
            );
        }
        assert_eq!(
            tick_at(
                &mut strategy,
                &mut sources,
                16,
                third + 2 * TICK,
                third + TICK
            ),
            Some(4)
        );
    }

    #[test]
    fn test_a_lowered_limit_is_lowered_again() {
        let t0 = Instant::now();
        let (mut strategy, mut sources) = started(16, 4, t0);
        refuse(&mut sources.admission, 3);
        assert_eq!(
            tick_at(&mut strategy, &mut sources, 16, t0 + TICK, t0 + TICK),
            None,
            "the stamp of this step is the third refusal's"
        );
        assert_eq!(
            tick_at(
                &mut strategy,
                &mut sources,
                16,
                t0 + 2 * TICK,
                t0 + 2 * TICK
            ),
            Some(4)
        );
        // Two runners are lost, and their reconnects are refused.
        sources.admission.on_ended(0);
        sources.admission.on_ended(1);
        refuse(&mut sources.admission, 3);
        let third = t0 + 3 * TICK;
        sources.admission.stamp(third);
        assert_eq!(
            tick_at(&mut strategy, &mut sources, 4, third + TICK, third + TICK),
            Some(2)
        );
        // Not below what is connected.
        refuse(&mut sources.admission, 3);
        let third = third + 2 * TICK;
        sources.admission.stamp(third);
        assert_eq!(
            tick_at(&mut strategy, &mut sources, 2, third + TICK, third + TICK),
            None
        );
    }

    #[test]
    fn test_old_successes_do_not_dilute_a_streak() {
        let t0 = Instant::now();
        let mut strategy = ConcurrencyControlStrategy::with_default_config(8);
        let mut sources = Sources::default();
        strategy.step(&mut sources.context(8, 0, Vec::new(), t0));
        // Twenty connects with seven others in flight, then the server
        // goes from eight to seven.
        for id in 0..20 {
            sources.admission.on_started(id, 7);
            if id >= 7 {
                sources.admission.on_ended(id);
            }
        }
        assert_eq!(sources.admission.connected(), 7);
        refuse(&mut sources.admission, 3);
        sources.admission.stamp(t0);
        assert_eq!(
            tick_at(&mut strategy, &mut sources, 8, t0 + TICK, t0 + TICK),
            Some(7)
        );
    }

    #[test]
    fn test_no_progress_after_the_refusals_keeps_the_limit() {
        let t0 = Instant::now();
        let (mut strategy, mut sources) = started(16, 4, t0);
        refuse(&mut sources.admission, 5);
        sources.admission.stamp(t0);
        for n in 1..40 {
            assert_eq!(
                tick_at(&mut strategy, &mut sources, 16, t0 + TICK * n, t0),
                None,
                "tick {n}"
            );
        }
    }

    #[test]
    fn test_a_streak_expires_before_it_is_confirmed() {
        let t0 = Instant::now();
        let (mut strategy, mut sources) = started(16, 4, t0);
        refuse(&mut sources.admission, 3);
        sources.admission.stamp(t0);
        let gap = Duration::from_secs(90);
        let mut now = t0;
        while now < t0 + gap {
            now += TICK;
            tick_at(&mut strategy, &mut sources, 16, now, t0);
        }
        assert_eq!(sources.admission.streak_count(), 0);
        assert_eq!(
            tick_at(&mut strategy, &mut sources, 16, now + TICK, now + TICK),
            None
        );
    }

    /// Two refusals at 0.1 and 0.2 s, ticks every 1.5 s, and a third
    /// refusal at 90.3 s, after the 90 s gap but before the next tick:
    /// it starts a new streak, and the progress after it lowers nothing.
    #[test]
    fn test_a_refusal_after_the_gap_does_not_revive_the_streak() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let (mut strategy, mut sources) = started(16, 4, t0);
        for at in [100, 200] {
            refuse(&mut sources.admission, 1);
            sources.admission.stamp(t0 + ms(at));
        }
        let mut now = t0;
        while now + TICK <= t0 + ms(90_300) {
            now += TICK;
            assert_eq!(tick_at(&mut strategy, &mut sources, 16, now, t0), None);
        }
        // Handled by the loop at its own time, before the next tick.
        refuse(&mut sources.admission, 1);
        sources.admission.stamp(t0 + ms(90_300));
        for _ in 0..4 {
            now += TICK;
            assert_eq!(
                tick_at(&mut strategy, &mut sources, 16, now, now),
                None,
                "at {:?}",
                now - t0
            );
        }
        assert_eq!(sources.admission.streak_count(), 1);
    }

    #[test]
    fn test_uncorroborated_refusals_count_for_nothing() {
        let t0 = Instant::now();
        let (mut strategy, mut sources) = started(16, 4, t0);
        for _ in 0..5 {
            sources
                .admission
                .on_attempt_ended(AdmissionClass::Refused, 0, 4);
        }
        sources.admission.stamp(t0);
        assert_eq!(
            tick_at(
                &mut strategy,
                &mut sources,
                16,
                t0 + 2 * TICK,
                t0 + 2 * TICK
            ),
            None
        );
    }

    /// A stream that ends short after its bytes is no evidence, so it
    /// lowers nothing; the old rule degraded at once for it.
    #[test]
    fn test_a_short_stream_with_bytes_does_not_lower_the_limit() {
        use super::super::admission_evidence::{AttemptPhase, classify};
        use crate::runner::TaskError;

        let t0 = Instant::now();
        let (mut strategy, mut sources) = started(16, 4, t0);
        let class = classify(AttemptPhase::Streaming, &TaskError::SmallerThanTotalSize);
        for _ in 0..5 {
            sources.admission.on_attempt_ended(class, 4, 4);
        }
        for n in 1..4 {
            assert_eq!(
                tick_at(
                    &mut strategy,
                    &mut sources,
                    16,
                    t0 + TICK * n,
                    t0 + TICK * n
                ),
                None
            );
        }
    }

    #[test]
    fn test_a_congestion_never_raises_the_limit() {
        let t0 = Instant::now();
        let (mut strategy, mut sources) = started(4, 4, t0);
        congest(&mut sources.admission);
        // Six active, which a limit of four allowed before it was set.
        let mut context = sources.context(4, 6, Vec::new(), t0 + TICK);
        assert_eq!(changed_limit(&strategy.step(&mut context)), Some(4));
        congest(&mut sources.admission);
        let mut context = sources.context(4, 2, Vec::new(), t0 + 2 * TICK);
        assert_eq!(changed_limit(&strategy.step(&mut context)), Some(2));
    }

    /// At a late tick, queued events may be about to change what the
    /// evidence shows: the silent rule waits for the next tick.
    #[test]
    fn test_a_late_tick_lowers_nothing() {
        let t0 = Instant::now();
        let (mut strategy, mut sources) = started(16, 4, t0);
        refuse(&mut sources.admission, 3);
        sources.admission.stamp(t0);
        let late = t0 + 2 * TICK + SECOND;
        assert_eq!(tick_at(&mut strategy, &mut sources, 16, late, late), None);
        assert_eq!(
            tick_at(&mut strategy, &mut sources, 16, late + TICK, late),
            Some(4)
        );
    }

    /// While the writer takes no data, the progress it would accept is
    /// unknown: the silent rule waits.
    #[test]
    fn test_a_full_writer_lowers_nothing() {
        let t0 = Instant::now();
        let (mut strategy, mut sources) = started(16, 4, t0);
        refuse(&mut sources.admission, 3);
        sources.admission.stamp(t0);
        let mut context = sources.context(16, 4, Vec::new(), t0 + TICK);
        context.writer_ready = false;
        assert_eq!(changed_limit(&strategy.step(&mut context)), None);
        assert_eq!(
            tick_at(
                &mut strategy,
                &mut sources,
                16,
                t0 + 2 * TICK,
                t0 + 2 * TICK
            ),
            Some(4)
        );
    }
}
