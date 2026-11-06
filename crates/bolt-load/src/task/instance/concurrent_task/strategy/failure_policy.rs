//! Decides when a concurrent task must give up.
//!
//! Independent from concurrency tuning: the rolling outcome sampler forgets
//! old samples and cannot serve as a retry budget.
//!
//! Three tiers bound the retries (design §8.2):
//! - a restart, a timeout or a dropped stream after the attempt delivered
//!   bytes, only backs its range off;
//! - each range fails at most `max_attempts_per_range` times, however well
//!   the other connections go;
//! - the task fails after `max_consecutive_failures` failures without a
//!   successful non-empty write in between.
//!
//! The no-progress deadline bounds them all.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use bolt_load_core::adapter::{AdapterError, RetryableError, UnretryableError};

use super::super::runner_manager::AttemptFailure;
use crate::{runner::TaskError, task::TaskOptions};

/// A server rejecting the connection count (429/503), which the concurrency
/// strategy answers by degrading instead of giving up.
fn is_congestion(error: &TaskError) -> bool {
    congestion_retry_after(error).is_some()
}

/// The `Retry-After` of a congestion rejection (429/503), `Some(None)` when
/// it sent none; `None` for any other error.
pub(super) fn congestion_retry_after(error: &TaskError) -> Option<Option<Duration>> {
    match error {
        TaskError::StreamError {
            source:
                AdapterError::Unretryable {
                    source: UnretryableError::Congestion { retry_after, .. },
                },
        } => Some(*retry_after),
        _ => None,
    }
}

/// A timeout, or a stream that broke off before its range ended:
///
/// - [`TaskError::Timeout`]: the idle timeout, the drain timeout, and the
///   connect timeout, which never has data;
/// - [`RetryableError::Io`]: a failed connect, or a body that broke off,
///   such as a reset or an EOF before the end (design §14.1);
/// - [`TaskError::SmallerThanTotalSize`]: the body ended cleanly before the
///   range did.
///
/// Not a retryable status, which comes before any byte, and not
/// [`TaskError::ChannelClosed`]: a runner that is lost failed on this side,
/// not on the connection.
fn is_timeout_or_drop(error: &TaskError) -> bool {
    matches!(
        error,
        TaskError::Timeout
            | TaskError::SmallerThanTotalSize
            | TaskError::StreamError {
                source: AdapterError::Retryable {
                    source: RetryableError::Io { .. }
                }
            }
    )
}

/// A restart (design §8.2): a timeout or a dropped stream after the
/// attempt delivered `received` bytes. Its range only backs off.
///
/// Delivered means accepted by the planner, so a restart always shrinks
/// its range, and restarts cannot go on without progress.
fn is_restart(error: &TaskError, received: u64) -> bool {
    received > 0 && is_timeout_or_drop(error)
}

/// Whether a failure is charged to its range's budget (design §8.2): a
/// retryable failure that is not a restart.
///
/// Never a congestion rejection (429/503), which the concurrency strategy
/// answers: the server limits connections, not this range. Any other
/// unretryable failure ends the task anyway.
pub(in super::super) fn charges_range(error: &TaskError, retryable: bool, received: u64) -> bool {
    retryable && !is_restart(error, received)
}

#[derive(Debug)]
pub struct FailurePolicy {
    max_consecutive: usize,
    max_attempts_per_range: u32,
    no_progress: Duration,
    consecutive: usize,
    last_progress_at: Instant,
}

impl FailurePolicy {
    pub fn new(options: &TaskOptions, now: Instant) -> Self {
        Self {
            max_consecutive: options.resolved_max_consecutive_failures(),
            max_attempts_per_range: options.max_attempts_per_range.get(),
            no_progress: options.no_progress_timeout,
            consecutive: 0,
            last_progress_at: now,
        }
    }

    /// Record that new bytes were accepted at `now`.
    #[inline]
    pub fn on_progress(&mut self, now: Instant) {
        self.last_progress_at = now;
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
    ///
    /// A restart only backs its range off. Any other retryable failure is
    /// charged to its range, and ends the task once the range has been
    /// charged `max_attempts_per_range` times, see
    /// [`TaskError::RangeExhausted`]; this comes before the consecutive
    /// limit, as it names the range. Otherwise it counts towards the
    /// consecutive failures.
    pub fn on_failure(&mut self, failure: &AttemptFailure, in_flight: usize) -> Option<TaskError> {
        if !failure.retryable {
            let tolerated = in_flight > 0 && is_congestion(&failure.error);
            return (!tolerated).then(|| failure.error.clone());
        }
        if is_restart(&failure.error, failure.received) {
            return None;
        }
        // Nothing is left to retry of a range that was complete.
        if !failure.range.is_empty() && failure.charged >= self.max_attempts_per_range {
            return Some(TaskError::RangeExhausted {
                range: failure.range.clone(),
                attempts: failure.charged,
                source: Arc::new(failure.error.clone()),
            });
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
    use std::ops::Range;

    use super::{super::super::chunk_planner::Released, *};

    fn policy(max_consecutive: usize, no_progress: Duration, now: Instant) -> FailurePolicy {
        FailurePolicy::new(
            &TaskOptions::default()
                .max_consecutive_failures(max_consecutive)
                .no_progress_timeout(no_progress),
            now,
        )
    }

    fn failure(error: TaskError, retryable: bool) -> AttemptFailure {
        AttemptFailure::new(
            0,
            error,
            retryable,
            Released {
                range: 0..1,
                attempts: 1,
                charged: 0,
                received: 0,
            },
        )
    }

    fn retryable() -> AttemptFailure {
        failure(
            TaskError::StreamError {
                source: AdapterError::Retryable {
                    source: RetryableError::Io {
                        source: Arc::new(std::io::Error::other("reset")),
                    },
                },
            },
            true,
        )
    }

    fn unretryable() -> AttemptFailure {
        failure(
            TaskError::Other {
                message: "forbidden".into(),
            },
            false,
        )
    }

    fn congestion() -> AttemptFailure {
        failure(
            TaskError::StreamError {
                source: AdapterError::Unretryable {
                    source: UnretryableError::Congestion {
                        status: 429,
                        retry_after: None,
                    },
                },
            },
            false,
        )
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

    /// Without a set limit, the policy allows `2 × max_connections` failures.
    #[test]
    fn test_default_limit_follows_max_connections() {
        let options =
            TaskOptions::default().max_connections(std::num::NonZeroUsize::new(16).unwrap());
        let mut p = FailurePolicy::new(&options, Instant::now());
        for _ in 1..32 {
            assert!(p.on_failure(&retryable(), 0).is_none());
        }
        match p.on_failure(&retryable(), 0) {
            Some(TaskError::RetryLimitExceeded { limit, .. }) => assert_eq!(limit, 32),
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
    fn test_congestion_keeps_its_retry_after() {
        let delay = Some(Duration::from_secs(2));
        let error = |retry_after| TaskError::StreamError {
            source: AdapterError::Unretryable {
                source: UnretryableError::Congestion {
                    status: 503,
                    retry_after,
                },
            },
        };
        assert_eq!(congestion_retry_after(&error(delay)), Some(delay));
        assert_eq!(congestion_retry_after(&error(None)), Some(None));
        assert_eq!(congestion_retry_after(&retryable().error), None);
        assert_eq!(congestion_retry_after(&unretryable().error), None);
    }

    #[test]
    fn test_no_progress_deadline() {
        let start = Instant::now();
        let timeout = Duration::from_secs(10);
        let mut p = policy(8, timeout, start);
        assert_eq!(p.deadline(), start + timeout);
        assert!(p.check(start + Duration::from_secs(9)).is_none());

        p.on_progress(start + Duration::from_secs(5));
        assert_eq!(p.deadline(), start + Duration::from_secs(15));
        assert!(p.check(start + Duration::from_secs(12)).is_none());
        assert!(matches!(
            p.check(start + Duration::from_secs(15)),
            Some(TaskError::NoProgress { timeout: t }) if t == timeout
        ));
    }

    // === Three tiers (design §8.2) ===

    fn budget_policy(max_consecutive: usize, max_attempts_per_range: u32) -> FailurePolicy {
        FailurePolicy::new(
            &TaskOptions::default()
                .max_consecutive_failures(max_consecutive)
                .max_attempts_per_range(std::num::NonZeroU32::new(max_attempts_per_range).unwrap()),
            Instant::now(),
        )
    }

    fn io(message: &str) -> TaskError {
        TaskError::StreamError {
            source: AdapterError::Retryable {
                source: RetryableError::Io {
                    source: Arc::new(std::io::Error::other(message.to_owned())),
                },
            },
        }
    }

    /// The retryable `attempts`th failure of `range` with `error`, after
    /// `charged` failures were charged to it and the attempt delivered
    /// `received` bytes; charged as the runner manager charges it.
    fn failed_range(
        error: TaskError,
        range: Range<u64>,
        attempts: u32,
        charged: u32,
        received: u64,
    ) -> AttemptFailure {
        AttemptFailure::new(
            1,
            error,
            true,
            Released {
                range,
                attempts,
                charged,
                received,
            },
        )
    }

    /// G6: the other connections' writes reset the consecutive failures,
    /// but not the failures of the range, which ends the task with its last
    /// error.
    #[test]
    fn test_a_failing_range_ends_the_task_despite_healthy_writes() {
        let mut p = budget_policy(2, 5);
        let errors = [
            io("reset"),
            TaskError::Timeout,
            TaskError::ChannelClosed,
            io("reset again"),
            io("refused"),
        ];
        let last = errors[4].to_string();
        for (charged, error) in (0..).zip(errors) {
            let failure = failed_range(error, 100..200, charged + 1, charged, 0);
            assert_eq!(failure.charged, charged + 1);
            match p.on_failure(&failure, 3) {
                None if charged < 4 => p.on_write_ok(64),
                Some(TaskError::RangeExhausted {
                    range,
                    attempts,
                    source,
                }) if charged == 4 => {
                    assert_eq!((range, attempts), (100..200, 5));
                    assert_eq!(source.to_string(), last);
                }
                other => panic!("failure {}: unexpected {other:?}", charged + 1),
            }
        }
    }

    /// Without the writes, the same failures end the task by the
    /// consecutive limit first.
    #[test]
    fn test_without_writes_the_consecutive_limit_ends_the_task_first() {
        let mut p = budget_policy(2, 5);
        assert!(
            p.on_failure(&failed_range(io("reset"), 100..200, 1, 0, 0), 3)
                .is_none()
        );
        assert!(matches!(
            p.on_failure(&failed_range(io("reset"), 100..200, 2, 1, 0), 3),
            Some(TaskError::RetryLimitExceeded { limit: 2, .. })
        ));
    }

    /// A timeout or a dropped stream after data is a restart: not a
    /// consecutive failure, and not charged to the range.
    #[test]
    fn test_a_timeout_after_data_is_not_a_consecutive_failure() {
        let mut p = budget_policy(1, 1);
        for error in [
            TaskError::Timeout,
            io("reset"),
            TaskError::SmallerThanTotalSize,
        ] {
            let failure = failed_range(error, 0..100, 3, 0, 10);
            assert_eq!(failure.charged, 0, "{failure:?}");
            assert!(p.on_failure(&failure, 0).is_none(), "{failure:?}");
            assert_eq!(p.consecutive, 0);
        }
    }

    #[test]
    fn test_a_timeout_without_data_still_counts() {
        let mut p = budget_policy(2, 5);
        let failure = failed_range(TaskError::Timeout, 0..100, 1, 0, 0);
        assert_eq!(failure.charged, 1);
        assert!(p.on_failure(&failure, 0).is_none());
        assert_eq!(p.consecutive, 1);
        match p.on_failure(&failed_range(TaskError::Timeout, 100..200, 1, 0, 0), 0) {
            Some(TaskError::RetryLimitExceeded { limit: 2, source }) => {
                assert!(matches!(*source, TaskError::Timeout), "{source:?}");
            }
            other => panic!("expected RetryLimitExceeded, got {other:?}"),
        }
    }

    /// Only a timeout or a dropped stream is a restart, and only after data;
    /// only retryable failures are charged.
    #[test]
    fn test_what_restarts_and_what_is_charged() {
        let status = TaskError::StreamError {
            source: AdapterError::Retryable {
                source: RetryableError::Status { status: 500 },
            },
        };
        for (error, restart) in [
            (TaskError::Timeout, true),
            (io("reset"), true),
            (TaskError::SmallerThanTotalSize, true),
            (TaskError::ChannelClosed, false),
            (status, false),
        ] {
            assert_eq!(is_restart(&error, 1), restart, "{error}");
            assert!(!is_restart(&error, 0), "{error} without data");
            assert_eq!(charges_range(&error, true, 1), !restart, "{error}");
            assert!(charges_range(&error, true, 0), "{error} without data");
        }
        assert!(!charges_range(&congestion().error, false, 0));
        assert!(!charges_range(&unretryable().error, false, 0));
    }

    /// The range ends the task with its `max_attempts_per_range`th charged
    /// failure, not before.
    #[test]
    fn test_the_range_budget_ends_at_exactly_its_limit() {
        let mut p = budget_policy(16, 3);
        assert!(
            p.on_failure(&failed_range(io("reset"), 0..100, 2, 1, 0), 0)
                .is_none()
        );
        assert!(matches!(
            p.on_failure(&failed_range(io("reset"), 0..100, 3, 2, 0), 0),
            Some(TaskError::RangeExhausted { attempts: 3, .. })
        ));

        // A limit of 1 ends the task with the first charged failure, but
        // a restart still only backs off.
        let mut p = budget_policy(16, 1);
        assert!(
            p.on_failure(&failed_range(TaskError::Timeout, 0..100, 1, 0, 10), 0)
                .is_none()
        );
        assert!(matches!(
            p.on_failure(&failed_range(io("reset"), 0..100, 2, 0, 0), 0),
            Some(TaskError::RangeExhausted { attempts: 1, .. })
        ));
    }

    /// Restarts and congestion rejections add to the attempts, which the
    /// backoff counts, but not to the range's budget.
    #[test]
    fn test_restarts_and_congestion_are_not_charged() {
        let mut p = budget_policy(16, 2);
        let congested = AttemptFailure::new(
            0,
            congestion().error,
            false,
            Released {
                range: 0..100,
                attempts: 6,
                charged: 0,
                received: 0,
            },
        );
        assert_eq!((congested.attempts, congested.charged), (6, 0));
        assert!(p.on_failure(&congested, 1).is_none());
        // The range's first charged failure.
        assert!(
            p.on_failure(&failed_range(io("reset"), 0..100, 7, 0, 0), 0)
                .is_none()
        );
        assert!(matches!(
            p.on_failure(&failed_range(io("reset"), 0..100, 8, 1, 0), 0),
            Some(TaskError::RangeExhausted { attempts: 2, .. })
        ));
    }

    /// When both limits are reached, the error names the range.
    #[test]
    fn test_the_range_budget_comes_before_the_consecutive_limit() {
        let mut p = budget_policy(1, 1);
        assert!(matches!(
            p.on_failure(&failed_range(io("reset"), 0..100, 1, 0, 0), 0),
            Some(TaskError::RangeExhausted { .. })
        ));
    }

    /// A runner that failed after its range was complete leaves nothing to
    /// exhaust; the failure still counts as a consecutive one.
    #[test]
    fn test_a_complete_range_is_not_exhausted() {
        let mut p = budget_policy(2, 1);
        let failure = failed_range(TaskError::ChannelClosed, 100..100, 1, 0, 100);
        assert_eq!(failure.charged, 1);
        assert!(p.on_failure(&failure, 0).is_none());
        assert_eq!(p.consecutive, 1);
    }
}
