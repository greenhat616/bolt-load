//! When a failed range, and the whole task after a congestion rejection,
//! may connect again (design §8.1).
//!
//! Every method takes `now`, so tests construct instants instead of
//! sleeping.

use std::time::{Duration, Instant};

use backon::{BackoffBuilder, ExponentialBuilder};

use super::failure_policy::congestion_retry_after;
use crate::runner::TaskError;

/// The backoff before the first retry of a range.
pub const MIN_RANGE_BACKOFF: Duration = Duration::from_millis(500);
/// The longest backoff of a range, jitter included.
pub const MAX_RANGE_BACKOFF: Duration = Duration::from_secs(30);
/// The longest a congestion rejection holds back new connections, whatever
/// its `Retry-After`.
pub const MAX_CONGESTION_BACKOFF: Duration = Duration::from_secs(120);

/// The base delay reached [`MAX_RANGE_BACKOFF`] long before this retry;
/// later retries are computed as this one, which gives the same delays.
const LAST_DISTINCT_RETRY: u32 = 16;

/// Backs off failed ranges, and the whole task after a congestion
/// rejection.
#[derive(Debug)]
pub struct Backoff {
    /// Whether to add backon's jitter; tests turn it off for exact delays.
    jitter: bool,
    /// No new connection before this instant.
    congestion_until: Option<Instant>,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Backoff {
    pub fn new() -> Self {
        Self {
            jitter: true,
            congestion_until: None,
        }
    }

    /// A backoff whose delays are exactly the base delays.
    #[cfg(test)]
    pub fn without_jitter() -> Self {
        Self {
            jitter: false,
            congestion_until: None,
        }
    }

    /// The wait before the next attempt on a range that failed `attempts`
    /// times, counted from 1.
    ///
    /// backon's exponential backoff: the base delay is
    /// `min(0.5 s × 2^(attempts − 1), 30 s)`, and the jitter adds up to as
    /// much again. backon adds it after capping the base delay, so the sum
    /// is capped at 30 s once more, as `meta.rs` does.
    pub fn delay(&self, attempts: u32) -> Duration {
        let mut builder = ExponentialBuilder::new()
            .with_min_delay(MIN_RANGE_BACKOFF)
            .with_factor(2.0)
            .with_max_delay(MAX_RANGE_BACKOFF)
            .without_max_times();
        if self.jitter {
            builder = builder.with_jitter();
        }
        let retry = attempts.clamp(1, LAST_DISTINCT_RETRY) as usize;
        builder
            .build()
            .nth(retry - 1)
            .expect("a backoff without max times never ends")
            .min(MAX_RANGE_BACKOFF)
    }

    /// Record the `attempts`-th failure of a range with `error` at `now`;
    /// returns when the range may be claimed again.
    ///
    /// A congestion rejection (429/503) waits for its `Retry-After` when
    /// that is longer than the backoff, at most [`MAX_CONGESTION_BACKOFF`],
    /// and holds back every new connection of the task until then: the
    /// server limits connections, not ranges. A later rejection never ends
    /// the hold earlier.
    pub fn on_failure(&mut self, error: &TaskError, attempts: u32, now: Instant) -> Instant {
        let backoff = self.delay(attempts);
        let Some(retry_after) = congestion_retry_after(error) else {
            return now + backoff;
        };
        self.hold(retry_after, backoff, now)
    }

    /// Hold every new connection back after a congestion rejection
    /// (429/503) at `now`, as [`Self::on_failure`] does, without backing a
    /// range off: a side of a hedged pair was rejected while the other runs
    /// on (design §9.6). `attempts` sets the backoff without a
    /// `Retry-After`: the failed attempts of the range should it fail now.
    ///
    /// Returns whether `error` was a congestion rejection.
    pub fn hold_congestion(&mut self, error: &TaskError, attempts: u32, now: Instant) -> bool {
        let Some(retry_after) = congestion_retry_after(error) else {
            return false;
        };
        let backoff = self.delay(attempts);
        self.hold(retry_after, backoff, now);
        true
    }

    /// Hold new connections back for `retry_after`, or `backoff` when it is
    /// longer or missing, at most [`MAX_CONGESTION_BACKOFF`]; returns when
    /// the range may connect again.
    fn hold(&mut self, retry_after: Option<Duration>, backoff: Duration, now: Instant) -> Instant {
        let delay = retry_after
            .map_or(backoff, |retry_after| retry_after.max(backoff))
            .min(MAX_CONGESTION_BACKOFF);
        let until = now + delay;
        self.congestion_until = Some(self.congestion_until.map_or(until, |held| held.max(until)));
        until
    }

    /// Whether new connections are held back at `now`.
    #[inline]
    pub fn is_congested(&self, now: Instant) -> bool {
        self.congestion_until.is_some_and(|until| now < until)
    }

    /// The end of the latest congestion hold, past or not.
    #[inline]
    pub fn congestion_until(&self) -> Option<Instant> {
        self.congestion_until
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bolt_load_core::adapter::{AdapterError, RetryableError, UnretryableError};
    use pretty_assertions::assert_eq;

    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    fn congestion(retry_after: Option<Duration>) -> TaskError {
        TaskError::StreamError {
            source: AdapterError::Unretryable {
                source: UnretryableError::Congestion {
                    status: 503,
                    retry_after,
                },
            },
        }
    }

    fn reset() -> TaskError {
        TaskError::StreamError {
            source: AdapterError::Retryable {
                source: RetryableError::Io {
                    source: Arc::new(std::io::Error::other("reset")),
                },
            },
        }
    }

    #[test]
    fn test_delay_doubles_from_half_a_second_up_to_thirty() {
        let backoff = Backoff::without_jitter();
        let delays: Vec<_> = (1..=8).map(|attempts| backoff.delay(attempts)).collect();
        assert_eq!(
            delays,
            [0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 30.0, 30.0].map(Duration::from_secs_f64)
        );
        // Out-of-range counts are clamped.
        assert_eq!(backoff.delay(0), MIN_RANGE_BACKOFF);
        assert_eq!(backoff.delay(u32::MAX), MAX_RANGE_BACKOFF);
    }

    /// backon's jitter adds up to the base delay again; the sum stays
    /// within 30 s.
    #[test]
    fn test_jitter_stays_between_the_base_delay_and_twice_it() {
        let exact = Backoff::without_jitter();
        let jittered = Backoff::new();
        for attempts in 1..=10 {
            let base = exact.delay(attempts);
            let upper = (base * 2).min(MAX_RANGE_BACKOFF);
            let samples: Vec<_> = (0..200).map(|_| jittered.delay(attempts)).collect();
            for delay in &samples {
                assert!(
                    base <= *delay && *delay <= upper,
                    "attempt {attempts}: {delay:?} outside {base:?}..={upper:?}"
                );
            }
            if base < MAX_RANGE_BACKOFF {
                assert!(
                    samples.iter().any(|delay| *delay != base),
                    "attempt {attempts}: no jitter"
                );
            }
        }
    }

    #[test]
    fn test_a_failure_backs_its_range_off_only() {
        let now = Instant::now();
        let mut backoff = Backoff::without_jitter();
        assert_eq!(backoff.on_failure(&reset(), 3, now), now + 2 * SECOND);
        assert_eq!(backoff.congestion_until(), None);
        assert!(!backoff.is_congested(now));
    }

    #[test]
    fn test_retry_after_holds_back_the_task() {
        let now = Instant::now();
        let mut backoff = Backoff::without_jitter();
        let until = now + 2 * SECOND;
        assert_eq!(
            backoff.on_failure(&congestion(Some(2 * SECOND)), 1, now),
            until
        );
        assert_eq!(backoff.congestion_until(), Some(until));
        assert!(backoff.is_congested(until - Duration::from_nanos(1)));
        assert!(!backoff.is_congested(until));
    }

    #[test]
    fn test_congestion_waits_for_the_longer_of_retry_after_and_the_backoff() {
        let now = Instant::now();
        // The fourth failure backs off for 4 s.
        let mut backoff = Backoff::without_jitter();
        assert_eq!(
            backoff.on_failure(&congestion(Some(SECOND)), 4, now),
            now + 4 * SECOND
        );
        assert_eq!(backoff.congestion_until(), Some(now + 4 * SECOND));
        // Without `Retry-After`, the backoff alone.
        let mut backoff = Backoff::without_jitter();
        assert_eq!(
            backoff.on_failure(&congestion(None), 1, now),
            now + MIN_RANGE_BACKOFF
        );
        assert_eq!(backoff.congestion_until(), Some(now + MIN_RANGE_BACKOFF));
    }

    #[test]
    fn test_congestion_is_capped() {
        let now = Instant::now();
        let mut backoff = Backoff::without_jitter();
        let hour = Duration::from_secs(3600);
        let until = now + MAX_CONGESTION_BACKOFF;
        assert_eq!(backoff.on_failure(&congestion(Some(hour)), 1, now), until);
        assert_eq!(backoff.congestion_until(), Some(until));
    }

    #[test]
    fn test_congestion_never_ends_earlier() {
        let now = Instant::now();
        let mut backoff = Backoff::without_jitter();
        backoff.on_failure(&congestion(Some(10 * SECOND)), 1, now);
        // A shorter hold later, and a plain failure, keep the first one.
        backoff.on_failure(&congestion(Some(2 * SECOND)), 1, now + SECOND);
        backoff.on_failure(&reset(), 1, now + SECOND);
        assert_eq!(backoff.congestion_until(), Some(now + 10 * SECOND));
        // A longer one extends it.
        backoff.on_failure(&congestion(Some(20 * SECOND)), 1, now + SECOND);
        assert_eq!(backoff.congestion_until(), Some(now + 21 * SECOND));
    }
}
