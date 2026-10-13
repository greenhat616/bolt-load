//! Decides when a concurrent task must give up.
//!
//! Independent from concurrency tuning: the rolling outcome sampler forgets
//! old samples and cannot serve as a retry budget.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use bolt_load_core::adapter::{AdapterError, UnretryableError};

use super::super::runner_manager::AttemptFailure;
use crate::{runner::TaskError, task::TaskOptions};

/// A server rejecting the connection count (429/503), which the concurrency
/// strategy answers by degrading instead of giving up.
fn is_congestion(error: &TaskError) -> bool {
    matches!(
        error,
        TaskError::StreamError {
            source: AdapterError::Unretryable {
                source: UnretryableError::ServiceUnavailable { .. }
            }
        }
    )
}

#[derive(Debug)]
pub struct FailurePolicy {
    max_consecutive: usize,
    no_progress: Duration,
    consecutive: usize,
    last_progress_at: Instant,
    last_accepted: u64,
}

impl FailurePolicy {
    pub fn new(options: &TaskOptions, now: Instant) -> Self {
        Self {
            max_consecutive: options.max_consecutive_failures,
            no_progress: options.no_progress_timeout,
            consecutive: 0,
            last_progress_at: now,
            last_accepted: 0,
        }
    }

    /// Record the total number of unique bytes accepted so far.
    pub fn on_bytes_accepted(&mut self, total_unique: u64, now: Instant) {
        if total_unique > self.last_accepted {
            self.last_accepted = total_unique;
            self.last_progress_at = now;
        }
    }

    /// A successful non-empty write resets the consecutive failure count.
    pub fn on_write_ok(&mut self, written: usize) {
        if written > 0 {
            self.consecutive = 0;
        }
    }

    /// Returns the terminal error if this failure ends the task.
    ///
    /// `in_flight` is the number of other attempts still allocated. A
    /// congestion rejection (429/503) while other attempts are in flight is
    /// left to the concurrency strategy; the no-progress deadline still
    /// bounds it. Any other unretryable failure ends the task.
    pub fn on_failure(&mut self, failure: &AttemptFailure, in_flight: usize) -> Option<TaskError> {
        if !failure.retryable {
            let tolerated = in_flight > 0 && is_congestion(&failure.error);
            return (!tolerated).then(|| failure.error.clone());
        }
        self.consecutive += 1;
        (self.consecutive >= self.max_consecutive).then(|| TaskError::RetryLimitExceeded {
            limit: self.max_consecutive,
            source: Arc::new(failure.error.clone()),
        })
    }

    pub fn deadline(&self) -> Instant {
        self.last_progress_at + self.no_progress
    }

    pub fn check(&self, now: Instant) -> Option<TaskError> {
        (now >= self.deadline()).then_some(TaskError::NoProgress {
            timeout: self.no_progress,
        })
    }
}

#[cfg(test)]
mod tests {
    use bolt_load_core::adapter::{AdapterError, error::retryable::RetryableError};

    use super::*;

    fn policy(max_consecutive: usize, no_progress: Duration, now: Instant) -> FailurePolicy {
        FailurePolicy::new(
            &TaskOptions::default()
                .max_consecutive_failures(max_consecutive)
                .no_progress_timeout(no_progress),
            now,
        )
    }

    fn retryable() -> AttemptFailure {
        AttemptFailure {
            runner_id: 0,
            error: TaskError::StreamError {
                source: AdapterError::Retryable {
                    source: RetryableError::Io {
                        source: Arc::new(std::io::Error::other("reset")),
                    },
                },
            },
            retryable: true,
        }
    }

    fn unretryable() -> AttemptFailure {
        AttemptFailure {
            runner_id: 0,
            error: TaskError::Other {
                message: "forbidden".into(),
            },
            retryable: false,
        }
    }

    fn congestion() -> AttemptFailure {
        AttemptFailure {
            runner_id: 0,
            error: TaskError::StreamError {
                source: AdapterError::Unretryable {
                    source: UnretryableError::ServiceUnavailable {
                        status: "429".into(),
                        description: "Too Many Requests".into(),
                    },
                },
            },
            retryable: false,
        }
    }

    #[test]
    fn test_retry_limit_exceeded_on_kth_failure() {
        let mut p = policy(3, Duration::from_secs(60), Instant::now());
        assert!(p.on_failure(&retryable(), 0).is_none());
        assert!(p.on_failure(&retryable(), 0).is_none());
        match p.on_failure(&retryable(), 0) {
            Some(TaskError::RetryLimitExceeded { limit, source }) => {
                assert_eq!(limit, 3);
                assert!(source.is_stream_error());
            }
            other => panic!("expected RetryLimitExceeded, got {other:?}"),
        }
    }

    #[test]
    fn test_non_empty_write_resets_consecutive_failures() {
        let mut p = policy(2, Duration::from_secs(60), Instant::now());
        assert!(p.on_failure(&retryable(), 0).is_none());
        p.on_write_ok(0);
        assert!(p.on_failure(&retryable(), 0).is_some());

        let mut p = policy(2, Duration::from_secs(60), Instant::now());
        assert!(p.on_failure(&retryable(), 0).is_none());
        p.on_write_ok(1);
        assert!(p.on_failure(&retryable(), 0).is_none());
    }

    #[test]
    fn test_unretryable_fails_immediately_when_nothing_else_in_flight() {
        let mut p = policy(8, Duration::from_secs(60), Instant::now());
        assert!(matches!(
            p.on_failure(&unretryable(), 0),
            Some(TaskError::Other { .. })
        ));
    }

    #[test]
    fn test_unretryable_is_terminal_even_with_attempts_in_flight() {
        let mut p = policy(8, Duration::from_secs(60), Instant::now());
        assert!(p.on_failure(&unretryable(), 3).is_some());
    }

    #[test]
    fn test_congestion_is_tolerated_only_while_other_attempts_are_in_flight() {
        let mut p = policy(1, Duration::from_secs(60), Instant::now());
        assert!(p.on_failure(&congestion(), 2).is_none());
        // Not counted towards the consecutive budget either.
        assert_eq!(p.consecutive, 0);
        assert!(matches!(
            p.on_failure(&congestion(), 0),
            Some(TaskError::StreamError { .. })
        ));
    }

    #[test]
    fn test_no_progress_deadline() {
        let start = Instant::now();
        let timeout = Duration::from_secs(10);
        let mut p = policy(8, timeout, start);
        assert_eq!(p.deadline(), start + timeout);
        assert!(p.check(start + Duration::from_secs(9)).is_none());

        // Same total does not count as progress.
        p.on_bytes_accepted(0, start + Duration::from_secs(5));
        assert_eq!(p.deadline(), start + timeout);

        p.on_bytes_accepted(100, start + Duration::from_secs(5));
        assert_eq!(p.deadline(), start + Duration::from_secs(15));
        assert!(p.check(start + Duration::from_secs(12)).is_none());
        assert!(matches!(
            p.check(start + Duration::from_secs(15)),
            Some(TaskError::NoProgress { timeout: t }) if t == timeout
        ));
    }
}
