//! Hedging a stalled runner in the endgame under the doubling strategy
//! (design §9.6, §14.16).

use std::time::{Duration, Instant};

use bolt_load_utils::telemetry::*;

use super::{
    super::{chunk_planner::ChunkStatus, strategy::planned_split_size},
    DownloadLoop,
};
use crate::task::RunnerId;

/// How long a runner must be silent before it is hedged (design §9.6).
pub(super) const HEDGE_IDLE: Duration = Duration::from_secs(2);

impl DownloadLoop {
    /// Hedge the runner silent the longest at `now`, if the task is in its
    /// endgame (design §9.6); only under the doubling strategy, whose rates
    /// tell what may still be split.
    ///
    /// The hedge runner requests the rest of the runner's chunk again, and
    /// the first non-empty frame decides which of the two goes on, see
    /// [`RunnerManager::spawn_hedge`](super::super::RunnerManager::spawn_hedge).
    /// At most one runner is hedged per tick.
    pub(super) fn hedge_stalled_runner(&mut self, now: Instant) {
        #[cfg(test)]
        if !self.hedging {
            return;
        }
        let Some(owner) = self.hedge_candidate(now) else {
            return;
        };
        let Some(slot) = self.manager.try_reserve_slot() else {
            trace!("[TASK] skip hedging runner {owner}: no runner slot is free");
            return;
        };
        let spawner = &self.spawner;
        if self
            .manager
            .spawn_hedge(slot, owner, now, |runner_id, limits, range| {
                spawner.spawn(range, runner_id, limits)
            })
        {
            debug!(
                "[TASK] hedging runner {owner}, silent since {:?}",
                self.manager.last_delivery(owner)
            );
        }
    }

    /// The runner to hedge at `now`: the one silent the longest for at
    /// least [`HEDGE_IDLE`], when another connection may be opened and
    /// nothing is left to split (design §9.6, trigger).
    fn hedge_candidate(&self, now: Instant) -> Option<RunnerId> {
        let rates = &self.rates;
        // While the writer is full, no runner's data is taken, so no
        // runner is silent.
        if !self.writer.can_write()
            || self.backoff.is_congested(now)
            || self.manager.get_active_runners_count()
                >= self
                    .max_concurrency
                    .min(self.strategy.refill_cap(self.manager.admission()))
            || !self.manager.get_eligible_ranges(now).is_empty()
        {
            return None;
        }
        // No speed limit before design §10.
        let min_chunk_size = self.manager.min_chunk_size();
        // The chunks that may still be split: with C11's threshold, but a
        // connecting runner's with the per-runner speed, as it has no rate
        // of its own yet (design §9.6, trigger 1).
        let splittable: Vec<RunnerId> = self
            .manager
            .active_chunks()
            .filter(|chunk| chunk.hedge.is_none())
            .filter_map(|chunk| Some((chunk.runner_id?, chunk)))
            .filter(|&(runner_id, _)| !self.manager.is_stopping(runner_id))
            .filter(|&(runner_id, chunk)| {
                let rate = match chunk.status {
                    ChunkStatus::Pending => None,
                    _ => rates.rate(runner_id),
                };
                let planned =
                    planned_split_size(rate, self.throughput.per_runner_speed, min_chunk_size);
                chunk.remaining() >= planned
            })
            .map(|(runner_id, _)| runner_id)
            .collect();
        // Every condition holds for the candidate before the longest
        // silence is chosen: nothing but its own chunk may be split.
        self.manager
            .active_chunks()
            .filter(|chunk| chunk.hedge.is_none() && chunk.remaining() > 0)
            .filter(|chunk| chunk.remaining() < 2 * min_chunk_size)
            .filter_map(|chunk| {
                let runner_id = chunk.runner_id?;
                let since = self.manager.last_delivery(runner_id)?;
                let silent = now.checked_duration_since(since)? >= HEDGE_IDLE;
                (silent && !self.manager.is_stopping(runner_id)).then_some((runner_id, since))
            })
            .filter(|&(runner_id, _)| splittable.iter().all(|&other| other == runner_id))
            .min_by_key(|&(runner_id, since)| (since, runner_id))
            .map(|(runner_id, _)| runner_id)
    }
}
