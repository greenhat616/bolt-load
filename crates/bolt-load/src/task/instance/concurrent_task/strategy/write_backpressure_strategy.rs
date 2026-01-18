//! Write backpressure strategy to prevent memory exhaustion from slow disk I/O
//!
//! This strategy monitors the file writer queue depth and write speed,
//! dynamically adjusting concurrency to prevent excessive memory usage.

use std::time::{Duration, Instant};

use bolt_load_utils::telemetry::*;

use super::{Strategy, StrategyAction};
use crate::task::instance::concurrent_task::file_writer::FILE_WRITER_QUEUE_SIZE;

/// Queue depth thresholds as percentage of FILE_WRITER_QUEUE_SIZE (2048)
const QUEUE_DEPTH_HIGH_THRESHOLD: f64 = 0.75; // 1536 items
const QUEUE_DEPTH_CRITICAL_THRESHOLD: f64 = 0.90; // 1843 items

/// Write speed ratio threshold (write_speed / download_speed)
const WRITE_SPEED_RATIO_THRESHOLD: f64 = 0.5; // Write speed < 50% of download speed

/// Minimum time between backpressure actions to avoid thrashing
const MIN_ACTION_INTERVAL: Duration = Duration::from_secs(3);

/// Configuration for write backpressure control
#[derive(Debug, Clone)]
pub struct WriteBackpressureConfig {
    /// High queue depth threshold (as percentage of max queue size)
    pub queue_depth_high_threshold: f64,
    /// Critical queue depth threshold (as percentage of max queue size)
    pub queue_depth_critical_threshold: f64,
    /// Write/download speed ratio threshold
    pub write_speed_ratio_threshold: f64,
    /// Minimum interval between actions
    pub min_action_interval: Duration,
    /// Degradation step (how many runners to reduce)
    pub degradation_step: usize,
    /// Minimum concurrency to maintain
    pub min_concurrency: usize,
}

impl Default for WriteBackpressureConfig {
    fn default() -> Self {
        Self {
            queue_depth_high_threshold: QUEUE_DEPTH_HIGH_THRESHOLD,
            queue_depth_critical_threshold: QUEUE_DEPTH_CRITICAL_THRESHOLD,
            write_speed_ratio_threshold: WRITE_SPEED_RATIO_THRESHOLD,
            min_action_interval: MIN_ACTION_INTERVAL,
            degradation_step: 1,
            min_concurrency: 1,
        }
    }
}

/// Backpressure state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackpressureState {
    /// Normal operation
    Normal,
    /// High queue depth detected
    High,
    /// Critical queue depth - immediate action required
    Critical,
}

/// Write backpressure strategy context
#[derive(Debug)]
pub struct WriteBackpressureContext {
    /// Current queue depth
    pub queue_depth: usize,
    /// Write speed (bytes/sec)
    pub write_speed: f64,
    /// Download speed (bytes/sec)
    pub download_speed: f64,
    /// Current active concurrency
    pub current_concurrency: usize,
    /// Maximum allowed concurrency
    pub max_concurrency: usize,
}

/// Write backpressure control strategy
pub struct WriteBackpressureStrategy {
    config: WriteBackpressureConfig,
    state: BackpressureState,
    last_action_time: Option<Instant>,
}

impl WriteBackpressureStrategy {
    /// Create a new write backpressure strategy with default config
    pub fn with_default_config() -> Self {
        Self {
            config: WriteBackpressureConfig::default(),
            state: BackpressureState::Normal,
            last_action_time: None,
        }
    }

    /// Create a new write backpressure strategy with custom config
    pub fn with_config(config: WriteBackpressureConfig) -> Self {
        Self {
            config,
            state: BackpressureState::Normal,
            last_action_time: None,
        }
    }

    /// Determine the current backpressure state based on queue depth
    fn determine_state(&self, queue_depth: usize) -> BackpressureState {
        let queue_depth_ratio = queue_depth as f64 / FILE_WRITER_QUEUE_SIZE as f64;

        if queue_depth_ratio >= self.config.queue_depth_critical_threshold {
            BackpressureState::Critical
        } else if queue_depth_ratio >= self.config.queue_depth_high_threshold {
            BackpressureState::High
        } else {
            BackpressureState::Normal
        }
    }

    /// Check if we should take action based on time throttling
    fn should_take_action(&self) -> bool {
        match self.last_action_time {
            None => true,
            Some(last_time) => last_time.elapsed() >= self.config.min_action_interval,
        }
    }

    /// Calculate the new concurrency level
    fn calculate_new_concurrency(
        &self,
        current: usize,
        state: BackpressureState,
    ) -> Option<usize> {
        match state {
            BackpressureState::Critical => {
                // Reduce by 25% or degradation_step, whichever is larger
                let reduction = std::cmp::max(current / 4, self.config.degradation_step);
                let new_concurrency = current.saturating_sub(reduction);
                Some(std::cmp::max(new_concurrency, self.config.min_concurrency))
            }
            BackpressureState::High => {
                // Gradual reduction
                let new_concurrency = current.saturating_sub(self.config.degradation_step);
                Some(std::cmp::max(new_concurrency, self.config.min_concurrency))
            }
            BackpressureState::Normal => None,
        }
    }
}

impl Strategy for WriteBackpressureStrategy {
    type Context<'a> = WriteBackpressureContext;

    fn name() -> &'static str {
        "WriteBackpressure"
    }

    fn step(&mut self, context: &mut Self::Context<'_>) -> Vec<StrategyAction> {
        let new_state = self.determine_state(context.queue_depth);

        trace!(
            "[STRATEGY] Write backpressure check: queue_depth={}/{}, write_speed={:.2} MB/s, download_speed={:.2} MB/s, state={:?}",
            context.queue_depth,
            FILE_WRITER_QUEUE_SIZE,
            context.write_speed / 1024.0 / 1024.0,
            context.download_speed / 1024.0 / 1024.0,
            new_state
        );

        // Update state
        let prev_state = self.state;
        self.state = new_state;

        // Check if we need to take action
        let mut actions = Vec::new();

        match new_state {
            BackpressureState::Critical => {
                // Critical state: always take action immediately
                if let Some(new_concurrency) =
                    self.calculate_new_concurrency(context.current_concurrency, new_state)
                {
                    if new_concurrency < context.current_concurrency {
                        warn!(
                            "[STRATEGY] CRITICAL write backpressure detected! Queue depth: {}/{} ({:.1}%), reducing concurrency {} -> {}",
                            context.queue_depth,
                            FILE_WRITER_QUEUE_SIZE,
                            (context.queue_depth as f64 / FILE_WRITER_QUEUE_SIZE as f64) * 100.0,
                            context.current_concurrency,
                            new_concurrency
                        );
                        actions.push(StrategyAction::ChangeMaxConcurrency(new_concurrency));
                        self.last_action_time = Some(Instant::now());
                    }
                }
            }
            BackpressureState::High => {
                // High state: take action if write speed is also slow
                let write_ratio = if context.download_speed > 0.0 {
                    context.write_speed / context.download_speed
                } else {
                    1.0
                };

                if write_ratio < self.config.write_speed_ratio_threshold && self.should_take_action()
                {
                    if let Some(new_concurrency) =
                        self.calculate_new_concurrency(context.current_concurrency, new_state)
                    {
                        if new_concurrency < context.current_concurrency {
                            info!(
                                "[STRATEGY] High write backpressure detected! Queue depth: {}/{} ({:.1}%), write/download ratio: {:.2}, reducing concurrency {} -> {}",
                                context.queue_depth,
                                FILE_WRITER_QUEUE_SIZE,
                                (context.queue_depth as f64 / FILE_WRITER_QUEUE_SIZE as f64) * 100.0,
                                write_ratio,
                                context.current_concurrency,
                                new_concurrency
                            );
                            actions.push(StrategyAction::ChangeMaxConcurrency(new_concurrency));
                            self.last_action_time = Some(Instant::now());
                        }
                    }
                }
            }
            BackpressureState::Normal => {
                // Normal state: log recovery if we were in a backpressure state before
                if prev_state != BackpressureState::Normal {
                    debug!(
                        "[STRATEGY] Write backpressure recovered. Queue depth: {}/{} ({:.1}%)",
                        context.queue_depth,
                        FILE_WRITER_QUEUE_SIZE,
                        (context.queue_depth as f64 / FILE_WRITER_QUEUE_SIZE as f64) * 100.0
                    );
                }
            }
        }

        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_determine_state_normal() {
        let strategy = WriteBackpressureStrategy::with_default_config();
        let state = strategy.determine_state(100); // ~5% of 2048
        assert_eq!(state, BackpressureState::Normal);
    }

    #[test]
    fn test_determine_state_high() {
        let strategy = WriteBackpressureStrategy::with_default_config();
        let state = strategy.determine_state(1600); // ~78% of 2048
        assert_eq!(state, BackpressureState::High);
    }

    #[test]
    fn test_determine_state_critical() {
        let strategy = WriteBackpressureStrategy::with_default_config();
        let state = strategy.determine_state(1900); // ~93% of 2048
        assert_eq!(state, BackpressureState::Critical);
    }

    #[test]
    fn test_calculate_new_concurrency_critical() {
        let strategy = WriteBackpressureStrategy::with_default_config();
        let new_concurrency =
            strategy.calculate_new_concurrency(8, BackpressureState::Critical);
        assert_eq!(new_concurrency, Some(6)); // 25% reduction: 8 * 0.75 = 6
    }

    #[test]
    fn test_calculate_new_concurrency_high() {
        let strategy = WriteBackpressureStrategy::with_default_config();
        let new_concurrency = strategy.calculate_new_concurrency(8, BackpressureState::High);
        assert_eq!(new_concurrency, Some(7)); // degradation_step = 1
    }

    #[test]
    fn test_calculate_new_concurrency_min() {
        let strategy = WriteBackpressureStrategy::with_default_config();
        let new_concurrency =
            strategy.calculate_new_concurrency(1, BackpressureState::Critical);
        assert_eq!(new_concurrency, Some(1)); // Can't go below min_concurrency
    }

    #[test]
    fn test_strategy_step_critical() {
        let mut strategy = WriteBackpressureStrategy::with_default_config();
        let mut context = WriteBackpressureContext {
            queue_depth: 1900, // Critical
            write_speed: 1_000_000.0,
            download_speed: 10_000_000.0,
            current_concurrency: 8,
            max_concurrency: 8,
        };

        let actions = strategy.step(&mut context);
        assert!(!actions.is_empty());
        match &actions[0] {
            StrategyAction::ChangeMaxConcurrency(new_concurrency) => {
                assert!(*new_concurrency < 8);
            }
            _ => panic!("Expected ChangeMaxConcurrency action"),
        }
    }

    #[test]
    fn test_strategy_step_high_with_slow_write() {
        let mut strategy = WriteBackpressureStrategy::with_default_config();
        let mut context = WriteBackpressureContext {
            queue_depth: 1600,                // High
            write_speed: 1_000_000.0,         // 1 MB/s
            download_speed: 10_000_000.0,     // 10 MB/s (ratio = 0.1 < 0.5)
            current_concurrency: 8,
            max_concurrency: 8,
        };

        let actions = strategy.step(&mut context);
        assert!(!actions.is_empty());
    }

    #[test]
    fn test_strategy_step_high_with_fast_write() {
        let mut strategy = WriteBackpressureStrategy::with_default_config();
        let mut context = WriteBackpressureContext {
            queue_depth: 1600,            // High
            write_speed: 8_000_000.0,     // 8 MB/s
            download_speed: 10_000_000.0, // 10 MB/s (ratio = 0.8 > 0.5)
            current_concurrency: 8,
            max_concurrency: 8,
        };

        let actions = strategy.step(&mut context);
        // Should not reduce concurrency because write speed is still good
        assert!(actions.is_empty());
    }

    #[test]
    fn test_strategy_step_normal() {
        let mut strategy = WriteBackpressureStrategy::with_default_config();
        let mut context = WriteBackpressureContext {
            queue_depth: 100, // Normal
            write_speed: 5_000_000.0,
            download_speed: 10_000_000.0,
            current_concurrency: 8,
            max_concurrency: 8,
        };

        let actions = strategy.step(&mut context);
        assert!(actions.is_empty());
    }
}
