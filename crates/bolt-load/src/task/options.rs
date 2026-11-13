use std::{
    num::{NonZeroU32, NonZeroU64, NonZeroUsize},
    time::Duration,
};

use super::BuildError;

/// Default for [`TaskOptions::no_progress_timeout`].
pub const DEFAULT_NO_PROGRESS_TIMEOUT: Duration = Duration::from_secs(60);
/// Default for [`TaskOptions::max_connections`], whatever the CPU count.
pub const DEFAULT_MAX_CONNECTIONS: NonZeroUsize = NonZeroUsize::new(8).unwrap();
/// Lower bound of the default [`TaskOptions::max_consecutive_failures`].
///
/// The default is `max(8, 2 × max_connections)`, so a single failed wave of
/// parallel connections cannot exhaust the budget.
pub const DEFAULT_MAX_CONSECUTIVE_FAILURES: usize = 8;
/// Default for [`TaskOptions::max_attempts_per_range`].
pub const DEFAULT_MAX_ATTEMPTS_PER_RANGE: NonZeroU32 = NonZeroU32::new(5).unwrap();
/// Default for [`TaskOptions::stream_idle_timeout`].
pub const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = crate::runner::DEFAULT_IDLE_TIMEOUT;
/// Default for [`TaskOptions::connect_timeout`].
pub const DEFAULT_CONNECT_TIMEOUT: Duration = crate::runner::DEFAULT_CONNECT_TIMEOUT;

/// Default for [`MetaRetry::max_retries`].
pub const DEFAULT_META_MAX_RETRIES: usize = 3;
/// Default for [`MetaRetry::min_delay`].
pub const DEFAULT_META_MIN_DELAY: Duration = Duration::from_millis(200);
/// Default for [`MetaRetry::max_delay`].
pub const DEFAULT_META_MAX_DELAY: Duration = Duration::from_secs(2);
/// A resumable task takes a checkpoint after this many bytes were written
/// since the last one (design §11.5.5).
pub const DEFAULT_CHECKPOINT_BYTES: NonZeroU64 = NonZeroU64::new(8 << 20).unwrap();
/// A resumable task takes a checkpoint this long after the last one.
pub const DEFAULT_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(2);

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

/// Connection limit and termination policy of a download task.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TaskOptions {
    /// The most connections a concurrent task has open at once, connecting
    /// ones included. The concurrency strategy may use fewer, never more.
    pub max_connections: NonZeroUsize,
    /// Fail the task when no new non-empty bytes are accepted for this long.
    pub no_progress_timeout: Duration,
    /// Fail the concurrent task after this many consecutive failed attempts
    /// without a successful non-empty write in between. Must be at least 1.
    ///
    /// `None` takes `max(8, 2 × max_connections)` when the task starts, see
    /// [`Self::resolved_max_consecutive_failures`].
    pub max_consecutive_failures: Option<usize>,
    /// Fail the concurrent task with
    /// [`TaskError::RangeExhausted`](crate::runner::TaskError::RangeExhausted)
    /// once this many failures of a single range count against it, however
    /// well the other connections go. 1 fails the task with the first one.
    ///
    /// A range counts on from its failures when it is retried; a range
    /// split off a running one starts at none. A restart, a timeout or a
    /// dropped stream after the attempt delivered bytes, does not count:
    /// that range only backs off, and `no_progress_timeout` still bounds
    /// it. Neither does a `Congestion` failure (429/503), nor a refused
    /// connect above what the server was seen to admit: a connect that
    /// failed before it started, while another attempt had delivered bytes
    /// and at least as many other attempts were in flight as had delivered
    /// bytes at once since the connection limit was last lowered. Every
    /// other retryable failure counts, a lost runner even after it
    /// delivered bytes.
    ///
    /// While refusals stay exempt, nothing bounds them per range: the
    /// task then ends when the other connections have downloaded the rest,
    /// or by `no_progress_timeout` or `max_consecutive_failures`.
    pub max_attempts_per_range: NonZeroU32,
    /// A runner reports a retryable `Timeout` when its upstream yields no
    /// non-empty data for this long. Must be non-zero.
    pub stream_idle_timeout: Duration,
    /// A runner reports a retryable `Timeout` when it is not connected this
    /// long after it started connecting, however much other runners progress
    /// meanwhile; it also bounds the singleton task's connection, which
    /// `no_progress_timeout` used to. Must be non-zero.
    pub connect_timeout: Duration,
    /// Retry policy of the metadata request, see [`MetaRetry`].
    pub meta_retry: MetaRetry,
    /// A task that keeps a state file for a later resume records the
    /// blocks it completed once this many bytes were written since its
    /// last checkpoint (design §11.5.5).
    pub checkpoint_bytes: NonZeroU64,
    /// Or once this long passed since its last checkpoint, whichever comes
    /// first. Must be non-zero. Either way, a periodic checkpoint starts no
    /// sooner than 0.5 s after the last one ended; the one at teardown does
    /// not wait for that.
    pub checkpoint_interval: Duration,
}

impl Default for TaskOptions {
    fn default() -> Self {
        Self {
            max_connections: DEFAULT_MAX_CONNECTIONS,
            no_progress_timeout: DEFAULT_NO_PROGRESS_TIMEOUT,
            max_consecutive_failures: None,
            max_attempts_per_range: DEFAULT_MAX_ATTEMPTS_PER_RANGE,
            stream_idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            meta_retry: MetaRetry::default(),
            checkpoint_bytes: DEFAULT_CHECKPOINT_BYTES,
            checkpoint_interval: DEFAULT_CHECKPOINT_INTERVAL,
        }
    }
}

impl TaskOptions {
    pub fn max_connections(mut self, limit: NonZeroUsize) -> Self {
        self.max_connections = limit;
        self
    }

    pub fn no_progress_timeout(mut self, timeout: Duration) -> Self {
        self.no_progress_timeout = timeout;
        self
    }

    pub fn max_consecutive_failures(mut self, limit: usize) -> Self {
        self.max_consecutive_failures = Some(limit);
        self
    }

    /// The consecutive failure limit in effect: `max_consecutive_failures`,
    /// or `max(8, 2 × max_connections)` when it is `None`.
    pub fn resolved_max_consecutive_failures(&self) -> usize {
        self.max_consecutive_failures.unwrap_or_else(|| {
            DEFAULT_MAX_CONSECUTIVE_FAILURES.max(self.max_connections.get().saturating_mul(2))
        })
    }

    pub fn max_attempts_per_range(mut self, limit: NonZeroU32) -> Self {
        self.max_attempts_per_range = limit;
        self
    }

    pub fn stream_idle_timeout(mut self, timeout: Duration) -> Self {
        self.stream_idle_timeout = timeout;
        self
    }

    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    pub fn meta_retry(mut self, retry: MetaRetry) -> Self {
        self.meta_retry = retry;
        self
    }

    pub fn checkpoint_bytes(mut self, bytes: NonZeroU64) -> Self {
        self.checkpoint_bytes = bytes;
        self
    }

    pub fn checkpoint_interval(mut self, interval: Duration) -> Self {
        self.checkpoint_interval = interval;
        self
    }

    pub(crate) fn validate(&self) -> Result<(), BuildError> {
        let reason = if self.no_progress_timeout.is_zero() {
            "no_progress_timeout must be greater than zero"
        } else if self.max_consecutive_failures == Some(0) {
            "max_consecutive_failures must be at least 1"
        } else if self.stream_idle_timeout.is_zero() {
            "stream_idle_timeout must be greater than zero"
        } else if self.connect_timeout.is_zero() {
            "connect_timeout must be greater than zero"
        } else if let Some(reason) = self.meta_retry.validate() {
            reason
        } else if self.checkpoint_interval.is_zero() {
            "checkpoint_interval must be greater than zero"
        } else {
            return Ok(());
        };
        Err(BuildError::InvalidOptions { reason })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_options_are_valid() {
        let options = TaskOptions::default();
        assert!(options.validate().is_ok());
        assert_eq!(options.max_connections.get(), 8);
        assert_eq!(options.no_progress_timeout, DEFAULT_NO_PROGRESS_TIMEOUT);
        assert_eq!(options.max_consecutive_failures, None);
        assert_eq!(options.resolved_max_consecutive_failures(), 16);
        assert_eq!(options.max_attempts_per_range.get(), 5);
        assert_eq!(options.stream_idle_timeout, DEFAULT_STREAM_IDLE_TIMEOUT);
        assert_eq!(options.connect_timeout, DEFAULT_CONNECT_TIMEOUT);
        assert_eq!(options.meta_retry, MetaRetry::default());
        assert_eq!(options.checkpoint_bytes.get(), 8 << 20);
        assert_eq!(options.checkpoint_interval, Duration::from_secs(2));
    }

    #[test]
    fn test_zero_values_are_rejected() {
        for options in [
            TaskOptions::default().no_progress_timeout(Duration::ZERO),
            TaskOptions::default().max_consecutive_failures(0),
            TaskOptions::default().stream_idle_timeout(Duration::ZERO),
            TaskOptions::default().connect_timeout(Duration::ZERO),
            TaskOptions::default().checkpoint_interval(Duration::ZERO),
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
                Err(BuildError::InvalidOptions { .. })
            ));
        }
    }

    #[test]
    fn test_the_default_failure_limit_follows_max_connections() {
        for (connections, limit) in [(1, 8), (4, 8), (5, 10), (8, 16), (32, 64)] {
            let options =
                TaskOptions::default().max_connections(NonZeroUsize::new(connections).unwrap());
            assert_eq!(
                options.resolved_max_consecutive_failures(),
                limit,
                "{connections} connections"
            );
        }
        // A limit that is set does not follow.
        let options = TaskOptions::default()
            .max_consecutive_failures(3)
            .max_connections(NonZeroUsize::new(32).unwrap());
        assert_eq!(options.resolved_max_consecutive_failures(), 3);
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
