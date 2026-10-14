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

/// Default for [`MetaRetry::max_retries`].
pub const DEFAULT_META_MAX_RETRIES: usize = 3;
/// Default for [`MetaRetry::min_delay`].
pub const DEFAULT_META_MIN_DELAY: Duration = Duration::from_millis(200);
/// Default for [`MetaRetry::max_delay`].
pub const DEFAULT_META_MAX_DELAY: Duration = Duration::from_secs(2);

/// Retry policy of the metadata request, applied only to
/// [`AdapterError::Retryable`](crate::adapter::AdapterError::Retryable) errors.
///
/// The delays grow exponentially from `min_delay` with jitter. With the
/// defaults, the three retries wait about 1.4–2.8 s in total.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MetaRetry {
    /// Retries after the first request; 0 disables retrying.
    pub max_retries: usize,
    /// The base delay before the first retry. Must be non-zero.
    pub min_delay: Duration,
    /// The upper bound of every single wait, jitter included. Must be non-zero
    /// and at least `min_delay`.
    pub max_delay: Duration,
}

impl Default for MetaRetry {
    fn default() -> Self {
        Self {
            max_retries: DEFAULT_META_MAX_RETRIES,
            min_delay: DEFAULT_META_MIN_DELAY,
            max_delay: DEFAULT_META_MAX_DELAY,
        }
    }
}

impl MetaRetry {
    /// No retries: the first error is returned.
    pub fn disabled() -> Self {
        Self::default().max_retries(0)
    }

    pub fn max_retries(mut self, retries: usize) -> Self {
        self.max_retries = retries;
        self
    }

    pub fn min_delay(mut self, delay: Duration) -> Self {
        self.min_delay = delay;
        self
    }

    pub fn max_delay(mut self, delay: Duration) -> Self {
        self.max_delay = delay;
        self
    }

    fn validate(&self) -> Option<&'static str> {
        if self.max_retries == 0 {
            None
        } else if self.min_delay.is_zero() {
            Some("meta_retry.min_delay must be greater than zero")
        } else if self.max_delay.is_zero() {
            Some("meta_retry.max_delay must be greater than zero")
        } else if self.min_delay > self.max_delay {
            Some("meta_retry.min_delay must not exceed max_delay")
        } else {
            None
        }
    }
}

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
    /// Retry policy of the metadata request, see [`MetaRetry`].
    pub meta_retry: MetaRetry,
}

impl Default for TaskOptions {
    fn default() -> Self {
        Self {
            no_progress_timeout: DEFAULT_NO_PROGRESS_TIMEOUT,
            max_consecutive_failures: default_max_consecutive_failures(),
            stream_idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
            meta_retry: MetaRetry::default(),
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

    pub fn meta_retry(mut self, retry: MetaRetry) -> Self {
        self.meta_retry = retry;
        self
    }

    pub(crate) fn validate(&self) -> Result<(), TaskManagerBuildError> {
        let message = if self.no_progress_timeout.is_zero() {
            "no_progress_timeout must be greater than zero"
        } else if self.max_consecutive_failures == 0 {
            "max_consecutive_failures must be at least 1"
        } else if self.stream_idle_timeout.is_zero() {
            "stream_idle_timeout must be greater than zero"
        } else if let Some(message) = self.meta_retry.validate() {
            message
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
        assert_eq!(options.meta_retry, MetaRetry::default());
    }

    #[test]
    fn test_zero_values_are_rejected() {
        for options in [
            TaskOptions::default().no_progress_timeout(Duration::ZERO),
            TaskOptions::default().max_consecutive_failures(0),
            TaskOptions::default().stream_idle_timeout(Duration::ZERO),
            TaskOptions::default().meta_retry(MetaRetry::default().min_delay(Duration::ZERO)),
            TaskOptions::default().meta_retry(MetaRetry::default().max_delay(Duration::ZERO)),
            TaskOptions::default().meta_retry(
                MetaRetry::default()
                    .min_delay(Duration::from_secs(3))
                    .max_delay(Duration::from_secs(1)),
            ),
        ] {
            assert!(matches!(
                options.validate(),
                Err(TaskManagerBuildError::FieldValidationFailed { .. })
            ));
        }
    }

    #[test]
    fn test_disabled_meta_retry_skips_delay_checks() {
        let options = TaskOptions::default().meta_retry(
            MetaRetry::disabled()
                .min_delay(Duration::ZERO)
                .max_delay(Duration::ZERO),
        );
        assert!(options.validate().is_ok());
    }
}
