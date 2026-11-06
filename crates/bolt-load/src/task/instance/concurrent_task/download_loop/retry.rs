//! Backing off failed ranges, and retrying them once their backoff ends
//! (design §8.1).

use std::time::Instant;

use bolt_load_utils::telemetry::*;

use super::{super::runner_manager::AttemptFailure, DownloadLoop};

impl DownloadLoop {
    /// Back the range of a failed attempt off, and wake the loop once it,
    /// or the task after a congestion, may connect again.
    pub(super) fn back_off(&mut self, failure: &AttemptFailure, now: Instant) {
        let not_before = self
            .backoff
            .on_failure(&failure.error, failure.attempts, now);
        trace!(
            "[TASK] backing off {:?} after {} failed attempts until {:?} from now",
            failure.range,
            failure.attempts,
            not_before - now
        );
        self.manager.cool_down(
            failure.range.clone(),
            failure.attempts,
            failure.charged,
            not_before,
        );
        if let Some(at) = self.next_retry(now) {
            self.deadline.wake_by(at);
        }
    }

    /// Create the runners the strategy would create next, for ranges whose
    /// backoff, or the task's congestion, ended by `now`.
    pub(super) fn retry_backed_off(&mut self, now: Instant) {
        let actions = self.strategy.create_tasks(
            self.max_concurrency,
            &mut self.manager,
            now,
            self.backoff.is_congested(now),
        );
        for action in actions {
            trace!("[TASK] retrying after a backoff: {action:?}");
            self.apply(action, now);
        }
    }

    /// The next instant after `now` a new connection may become possible:
    /// the end of the congestion, otherwise the earliest end of a range's
    /// backoff. `None` when nothing waits.
    ///
    /// No range may connect before the congestion ends, so a range whose
    /// backoff ends earlier waits for it.
    pub(super) fn next_retry(&self, now: Instant) -> Option<Instant> {
        if let Some(until) = self.backoff.congestion_until()
            && now < until
        {
            return Some(until);
        }
        self.manager.next_cooldown_end(now)
    }
}
