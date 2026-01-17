//! A Sampler to tracking the outcome of the runners
//!
//! Based on the sliding window to count the success/failure of the runners

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Runner is failed to connect or download
    /// Network error, timeout, etc.
    Retryable,
    /// The server is rejected the request due to concurrency limit
    /// 429 Too Many Requests, 503 Service Unavailable, etc.
    Unretryable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerOutcome {
    /// The runner completed the chunk
    Completed,
    /// The runner failed to connect to the server
    ConnectFailed(FailureKind),
    /// The runner failed to download the chunk
    StreamClosed(FailureKind),
}

impl RunnerOutcome {
    #[inline]
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::Completed)
    }

    #[inline]
    pub const fn is_unretryable(&self) -> bool {
        matches!(
            self,
            Self::ConnectFailed(FailureKind::Unretryable)
                | Self::StreamClosed(FailureKind::Unretryable)
        )
    }

    #[inline]
    pub const fn failure_kind(&self) -> Option<FailureKind> {
        match self {
            Self::Completed => None,
            Self::ConnectFailed(k) | Self::StreamClosed(k) => Some(*k),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct OutcomeRecord {
    timestamp: Instant,
    outcome: RunnerOutcome,
}

/// Runner Result statistics
#[derive(Debug, Clone, Copy, Default)]
pub struct OutcomeStats {
    /// The total number of runners
    pub total: usize,
    /// The number of completed runners
    pub completed: usize,
    /// The number of retryable connection failures
    pub connect_failed_retryable: usize,
    /// The number of unretryable connection failures
    pub connect_failed_unretryable: usize,
    /// The number of retryable stream closed failures
    pub stream_closed_retryable: usize,
    /// The number of unretryable stream closed failures
    pub stream_closed_unretryable: usize,
}

impl OutcomeStats {
    /// The total number of failures
    #[inline]
    pub fn total_failures(&self) -> usize {
        self.total - self.completed
    }

    /// The total failure rate
    pub fn failure_rate(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.total_failures() as f64 / self.total as f64
    }

    /// The number of unretryable failures
    #[inline]
    pub fn unretryable_failures(&self) -> usize {
        self.connect_failed_unretryable + self.stream_closed_unretryable
    }

    /// The connection failure rate (may indicate high concurrency)
    pub fn connect_failure_rate(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        (self.connect_failed_retryable + self.connect_failed_unretryable) as f64 / self.total as f64
    }
}

/// Runner result sampler
///
/// Based on the sliding window to count the success/failure of the runners
#[derive(Debug)]
pub struct RunnerOutcomeSampler {
    /// The result record queue
    records: VecDeque<OutcomeRecord>,
    /// The time window (only count the records within the window)
    time_window: Duration,
    /// The maximum number of records
    max_records: usize,
    /// The timestamp of the last unretryable failure (for immediate response)
    last_unretryable_at: Option<Instant>,
}

impl RunnerOutcomeSampler {
    /// Create a new sampler
    ///
    /// # Arguments
    /// * `time_window` - The time window to count the records, recommended 30-60 seconds
    /// * `max_records` - The maximum number of records, recommended 50-100
    pub fn new(time_window: Duration, max_records: usize) -> Self {
        Self {
            records: VecDeque::with_capacity(max_records),
            time_window,
            max_records,
            last_unretryable_at: None,
        }
    }

    /// Create a new sampler with default configuration (60 seconds window, 100 records)
    pub fn with_defaults() -> Self {
        Self::new(Duration::from_secs(60), 100)
    }

    /// Record the result of a runner
    pub fn record(&mut self, outcome: RunnerOutcome) {
        let now = Instant::now();

        // Clean up expired records
        self.cleanup_expired(now);

        // Check if it is unretryable
        if outcome.is_unretryable() {
            self.last_unretryable_at = Some(now);
        }

        // Add the record
        self.records.push_back(OutcomeRecord {
            timestamp: now,
            outcome,
        });

        // Limit the maximum number of records
        while self.records.len() > self.max_records {
            self.records.pop_front();
        }
    }

    /// Record the completed outcome
    #[inline]
    pub fn record_completed(&mut self) {
        self.record(RunnerOutcome::Completed);
    }

    /// Record the connection failed outcome
    #[inline]
    pub fn record_connect_failed(&mut self, kind: FailureKind) {
        self.record(RunnerOutcome::ConnectFailed(kind));
    }

    /// Record the stream closed outcome
    #[inline]
    pub fn record_stream_closed(&mut self, kind: FailureKind) {
        self.record(RunnerOutcome::StreamClosed(kind));
    }

    #[inline]
    fn cleanup_expired(&mut self, now: Instant) {
        let cutoff = now
            .checked_sub(self.time_window)
            .expect("overflow when subtracting duration from instant");
        while let Some(front) = self.records.front() {
            if front.timestamp < cutoff {
                self.records.pop_front();
            } else {
                break;
            }
        }
    }

    /// Get the statistics result
    pub fn stats(&mut self) -> OutcomeStats {
        self.cleanup_expired(Instant::now());

        let mut stats = OutcomeStats::default();
        for record in &self.records {
            stats.total += 1;
            match record.outcome {
                RunnerOutcome::Completed => stats.completed += 1,
                RunnerOutcome::ConnectFailed(FailureKind::Retryable) => {
                    stats.connect_failed_retryable += 1;
                }
                RunnerOutcome::ConnectFailed(FailureKind::Unretryable) => {
                    stats.connect_failed_unretryable += 1;
                }
                RunnerOutcome::StreamClosed(FailureKind::Retryable) => {
                    stats.stream_closed_retryable += 1;
                }
                RunnerOutcome::StreamClosed(FailureKind::Unretryable) => {
                    stats.stream_closed_unretryable += 1;
                }
            }
        }
        stats
    }

    /// Check if a unretryable error occurred recently (within the specified time)
    ///
    /// Used to trigger immediate degradation
    pub fn has_recent_unretryable(&mut self, within: Duration) -> bool {
        let now = Instant::now();
        if let Some(last) = self.last_unretryable_at {
            if now.duration_since(last) <= within {
                // 清除标记，避免重复触发
                self.last_unretryable_at = None;
                return true;
            }
        }
        false
    }

    /// Reset the sampler
    pub fn reset(&mut self) {
        self.records.clear();
        self.last_unretryable_at = None;
    }

    /// The number of records within the current window
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}
