use std::time::Duration;

use super::TaskManagerBuildError;

/// Default for [`TaskOptions::no_progress_timeout`].
pub const DEFAULT_NO_PROGRESS_TIMEOUT: Duration = Duration::from_secs(60);
/// Lower bound of the default [`TaskOptions::max_consecutive_failures`].
///
/// The actual default is `max(8, 2 × available parallelism)`, so a single
/// failed wave of parallel connections cannot exhaust the budget.
pub const DEFAULT_MAX_CONSECUTIVE_FAILURES: usize = 8;

fn default_max_consecutive_failures() -> usize {
    let parallelism = std::thread::available_parallelism().map_or(1, |n| n.get());
    DEFAULT_MAX_CONSECUTIVE_FAILURES.max(parallelism.saturating_mul(2))
}
/// Default for [`TaskOptions::stream_idle_timeout`].
pub const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = crate::runner::DEFAULT_IDLE_TIMEOUT;

/// Termination policy of a download task.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TaskOptions {
    /// Fail the task when no new non-empty bytes are accepted for this long.
    pub no_progress_timeout: Duration,
    /// Fail the concurrent task after this many consecutive failed attempts
    /// without a successful non-empty write in between. Must be at least 1.
    pub max_consecutive_failures: usize,
    /// A runner reports a retryable `Timeout` when its upstream yields no
    /// non-empty data for this long. Must be non-zero.
    pub stream_idle_timeout: Duration,
}

impl Default for TaskOptions {
    fn default() -> Self {
        Self {
            no_progress_timeout: DEFAULT_NO_PROGRESS_TIMEOUT,
            max_consecutive_failures: default_max_consecutive_failures(),
            stream_idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
        }
    }
}

impl TaskOptions {
    pub fn no_progress_timeout(mut self, timeout: Duration) -> Self {
        self.no_progress_timeout = timeout;
        self
    }

    pub fn max_consecutive_failures(mut self, limit: usize) -> Self {
        self.max_consecutive_failures = limit;
        self
    }

    pub fn stream_idle_timeout(mut self, timeout: Duration) -> Self {
        self.stream_idle_timeout = timeout;
        self
    }

    pub(crate) fn validate(&self) -> Result<(), TaskManagerBuildError> {
        let message = if self.no_progress_timeout.is_zero() {
            "no_progress_timeout must be greater than zero"
        } else if self.max_consecutive_failures == 0 {
            "max_consecutive_failures must be at least 1"
        } else if self.stream_idle_timeout.is_zero() {
            "stream_idle_timeout must be greater than zero"
        } else {
            return Ok(());
        };
        Err(TaskManagerBuildError::FieldValidationFailed {
            message: message.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_options_are_valid() {
        let options = TaskOptions::default();
        assert!(options.validate().is_ok());
        assert_eq!(options.no_progress_timeout, DEFAULT_NO_PROGRESS_TIMEOUT);
        assert!(options.max_consecutive_failures >= DEFAULT_MAX_CONSECUTIVE_FAILURES);
        assert_eq!(options.stream_idle_timeout, DEFAULT_STREAM_IDLE_TIMEOUT);
    }

    #[test]
    fn test_zero_values_are_rejected() {
        for options in [
            TaskOptions::default().no_progress_timeout(Duration::ZERO),
            TaskOptions::default().max_consecutive_failures(0),
            TaskOptions::default().stream_idle_timeout(Duration::ZERO),
        ] {
            assert!(matches!(
                options.validate(),
                Err(TaskManagerBuildError::FieldValidationFailed { .. })
            ));
        }
    }
}
