//! The Strategy that controls the max concurrency of the task,
//! according to the failure rate of the runners
//!

use std::{
    ops::Range,
    time::{Duration, Instant},
};

use super::{
    Strategy, StrategyAction,
    runner_outcome_sampler::{OutcomeStats, RunnerOutcomeSampler},
};

#[derive(Debug, Clone)]
/// The configuration for the concurrency control strategy
pub struct ConcurrencyControlConfig {
    /// The threshold for triggering degradation
    pub failure_rate_threshold: f64,
    /// The minimum number of samples (to avoid misjudgment when the samples are too few)
    pub min_samples: usize,
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
    /// The window for detecting unretryable errors
    pub unretryable_detection_window: Duration,
}

impl Default for ConcurrencyControlConfig {
    fn default() -> Self {
        Self {
            failure_rate_threshold: 0.3,
            // runner count is relatively few, reduce the threshold
            min_samples: 5,
            recovery_delay: Duration::from_secs(30),
            recovery_observation_period: Duration::from_secs(15),
            degradation_step: 1,
            recovery_step: 1,
            min_concurrency: 1,
            unretryable_detection_window: Duration::from_secs(5),
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
    /// The sampler
    #[debug(skip)]
    pub sampler: &'a mut RunnerOutcomeSampler,
    /// The available chunks
    pub available_chunks: Vec<Range<u64>>,
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
}

impl ConcurrencyControlStrategy {
    pub fn new(config: ConcurrencyControlConfig, initial_max_concurrency: usize) -> Self {
        Self {
            config,
            state: StrategyState::Normal,
            initial_max_concurrency,
            first_execute: true,
        }
    }

    pub fn with_default_config(initial_max_concurrency: usize) -> Self {
        Self::new(ConcurrencyControlConfig::default(), initial_max_concurrency)
    }

    /// Handle the immediate degradation, when a unretryable error occurred recently
    #[inline]
    fn handle_immediate_degradation(
        &mut self,
        context: &ConcurrencyControlContext,
        now: Instant,
    ) -> Vec<StrategyAction> {
        let new_concurrency = context.active_runners;

        self.state = StrategyState::Degraded {
            target_concurrency: new_concurrency,
            degraded_at: now,
        };

        vec![StrategyAction::ChangeMaxConcurrency(new_concurrency)]
    }

    #[inline]
    fn handle_normal_state(
        &mut self,
        context: &ConcurrencyControlContext,
        stats: &OutcomeStats,
        now: Instant,
        actions: &mut Vec<StrategyAction>,
    ) {
        // if the sample count is less than the minimum samples, do not degrade
        if stats.total < self.config.min_samples {
            return;
        }

        if stats.failure_rate() >= self.config.failure_rate_threshold
            && context.max_concurrency > self.config.min_concurrency
        {
            let new_concurrency = context
                .max_concurrency
                .saturating_sub(self.config.degradation_step)
                .max(self.config.min_concurrency);

            self.state = StrategyState::Degraded {
                target_concurrency: new_concurrency,
                degraded_at: now,
            };

            actions.push(StrategyAction::ChangeMaxConcurrency(new_concurrency));
        }
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

    /// Get the current state
    #[allow(dead_code)]
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
        let now = Instant::now();

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

        if context
            .sampler
            .has_recent_unretryable(self.config.unretryable_detection_window)
        {
            tracing::trace!("[CONTROL STRATEGY] Has recent unretryable, degraded");
            return self.handle_immediate_degradation(context, now);
        }

        let mut actions = Vec::new();
        let stats = context.sampler.stats();

        match self.state {
            StrategyState::Normal => {
                self.handle_normal_state(context, &stats, now, &mut actions);
            }
            StrategyState::Degraded {
                target_concurrency,
                degraded_at,
            } => {
                self.handle_degraded_state(
                    &stats,
                    now,
                    target_concurrency,
                    degraded_at,
                    &mut actions,
                );
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
        if context.active_runners < context.max_concurrency {
            return context
                .available_chunks
                .iter()
                .take(context.max_concurrency - context.active_runners)
                .map(|chunk| StrategyAction::CreateTask(chunk.clone()))
                .collect();
        }

        // No Action
        actions
    }
}
