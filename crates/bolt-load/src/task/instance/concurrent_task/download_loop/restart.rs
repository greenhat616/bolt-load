//! Restarting slow runners under the doubling strategy (design §9.5,
//! §14.15).

use std::time::Instant;

use bolt_load_utils::telemetry::*;

use super::{
    super::strategy::{JudgedRunner, SlowRestartContext, slow_runner},
    DownloadLoop,
};
use crate::{runner::BUFFER_SIZE, task::RunnerId};

impl DownloadLoop {
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
    /// [`RunnerManager::stop_runner`]: super::super::RunnerManager::stop_runner
    pub(super) fn restart_slow_runner(&mut self, now: Instant) {
        let Some(rates) = &self.rates else {
            return;
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
            return;
        };
        if self.manager.stop_runner(runner_id) {
            debug!("[TASK] stopping slow runner {runner_id}: {context:?}");
        }
    }

    /// Whether `runner_id` may be judged, and so restarted: not once it is
    /// stopping.
    ///
    /// The endgame hedge (T27, design §9.6) also leaves out either side of
    /// a hedged pair here.
    fn may_restart(&self, runner_id: RunnerId) -> bool {
        !self.manager.is_stopping(runner_id)
    }
}
