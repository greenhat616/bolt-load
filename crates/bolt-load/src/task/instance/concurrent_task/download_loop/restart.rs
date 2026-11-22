//! Restarting slow and silent runners under the doubling strategy (design
//! §9.5, §14.15, §14.19).

use std::time::Instant;

use bolt_load_utils::telemetry::*;

use super::{
    super::{
        chunk_planner::ChunkStatus,
        strategy::{JudgedRunner, SILENT_RESTART_AFTER, SlowRestartContext, slow_runner},
    },
    DownloadLoop,
};
use crate::{runner::BUFFER_SIZE, task::RunnerId};

impl DownloadLoop {
    /// Stop at most one runner at `now`: the slowest, if it is slow enough
    /// (design §9.5), or else the one silent the longest, if it is silent
    /// for long enough (design §14.19).
    pub(super) fn restart_runner(&mut self, now: Instant) {
        if !self.restart_slow_runner(now) {
            self.restart_silent_runner(now);
        }
    }

    /// Stop the slowest runner at `now`, if it is slow enough, for its
    /// range to be retried (design §9.5); only under the doubling
    /// strategy, which samples the rates.
    ///
    /// The runners are judged on the bytes they delivered over the slow
    /// window at the sample this tick just took, see
    /// [`RunnerRates::window_bytes`](super::throughput::RunnerRates::window_bytes),
    /// allowing a frame either way.
    /// The runner is stopped through its own stop, see
    /// [`RunnerManager::stop_runner`]: its range is released, backed off
    /// and retried once it reports `Stopped`.
    ///
    /// Returns whether a runner was stopped.
    ///
    /// [`RunnerManager::stop_runner`]: super::super::RunnerManager::stop_runner
    pub(super) fn restart_slow_runner(&mut self, now: Instant) -> bool {
        let Some(rates) = &self.rates else {
            return false;
        };
        let runners = rates
            .window_bytes()
            .into_iter()
            .filter(|&(runner_id, _)| self.may_restart(runner_id))
            .filter_map(|(runner_id, bytes)| {
                let state = self.manager.get_runner_state(runner_id)?;
                Some(JudgedRunner {
                    runner_id,
                    bytes,
                    remaining: state.remaining(),
                })
            })
            .collect();
        let context = SlowRestartContext {
            runners,
            frame: BUFFER_SIZE as u64,
            min_chunk_size: self.manager.min_chunk_size(),
            congested: self.backoff.is_congested(now),
            // No speed limit before design §10.
            speed_limit: None,
        };
        let Some(runner_id) = slow_runner(&context) else {
            return false;
        };
        let stopped = self.manager.stop_runner(runner_id);
        if stopped {
            debug!("[TASK] stopping slow runner {runner_id}: {context:?}");
            self.strategy.on_restarted();
        }
        stopped
    }

    /// Stop the runner silent the longest at `now`, if it neither delivered
    /// a frame nor read any of its body for at least
    /// [`SILENT_RESTART_AFTER`] (design §14.19); only under the doubling
    /// strategy, which samples the rates.
    ///
    /// Its silence runs from when it was last active reading, by its own
    /// clock, or from its spawn, see
    /// [`RunnerManager::silent_since`](super::super::RunnerManager::silent_since):
    /// a runner reading slowly holds its bytes until a frame is full, and is
    /// not silent, nor is one pushing a frame. A full writer does not count, and while it is full none
    /// is silent. It must also have been running, past its connection,
    /// since a sample at least as old: the connect timeout deals with a
    /// runner that does not connect. As for a slow runner, it needs at
    /// least twice `min_chunk_size` left: below that, the endgame hedge
    /// deals with it. None while the task is congested.
    fn restart_silent_runner(&mut self, now: Instant) {
        let Some(runner_id) = self.silent_runner(now) else {
            return;
        };
        if self.manager.stop_runner(runner_id) {
            debug!(
                "[TASK] stopping silent runner {runner_id}, silent since {:?}",
                self.manager.silent_since(runner_id, now)
            );
            self.strategy.on_restarted();
        }
    }

    /// The runner [`Self::restart_silent_runner`] stops at `now`: the one
    /// silent the longest, the lower ID first.
    fn silent_runner(&self, now: Instant) -> Option<RunnerId> {
        let rates = self.rates.as_ref()?;
        // No speed limit before design §10.
        if self.backoff.is_congested(now) || !self.writer.can_write() {
            return None;
        }
        let since = now.checked_sub(SILENT_RESTART_AFTER)?;
        let least_left = self.manager.min_chunk_size().saturating_mul(2);
        self.manager
            .active_chunks()
            .filter(|chunk| chunk.status == ChunkStatus::Running && chunk.remaining() >= least_left)
            .filter_map(|chunk| chunk.runner_id)
            .filter(|&runner_id| self.may_restart(runner_id))
            // Frames queued that the loop has not taken: it delivers, the
            // loop is behind, and a stop now would drop those frames with
            // its receiver.
            .filter(|&runner_id| !self.manager.has_queued_data(runner_id))
            .filter_map(|runner_id| {
                let last = self.manager.silent_since(runner_id, now)?;
                (last <= since && rates.running_since(runner_id, since))
                    .then_some((last, runner_id))
            })
            .min()
            .map(|(_, runner_id)| runner_id)
    }

    /// Whether `runner_id` may be judged, and so restarted: not once it is
    /// stopping, nor while it is a side of an undecided pair (design §9.6).
    /// A runner left out is not in the median either.
    fn may_restart(&self, runner_id: RunnerId) -> bool {
        !self.manager.is_stopping(runner_id) && !self.manager.is_in_undecided_pair(runner_id)
    }
}
