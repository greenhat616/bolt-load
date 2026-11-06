use std::{
    ops::Range,
    task::{Context, Poll},
    time::Instant,
};

use bolt_load_utils::telemetry::*;

use super::{
    super::{
        runner_manager::RunnerSlot,
        strategy::{StrategyAction, planned_split_size},
    },
    DownloadLoop, LoopEvent,
};
use crate::{runtime::EventTimer, task::RunnerId};

/// The strategy's periodic tick.
pub(super) struct StrategyTick {
    timer: EventTimer,
}

impl StrategyTick {
    pub(super) fn new(timer: EventTimer) -> Self {
        Self { timer }
    }

    #[inline]
    pub(super) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<LoopEvent> {
        self.timer.poll_tick(cx).map(|()| LoopEvent::StrategyTimer)
    }
}

impl DownloadLoop {
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    pub(super) fn on_strategy_tick(&mut self, now: Instant) {
        // Every tick, so each sample spans one tick.
        let tick = self
            .rates
            .as_mut()
            .map(|rates| rates.sample(&self.manager, now));
        let Some((strategy_name, actions)) = self.strategy.execute(
            self.max_concurrency,
            self.throughput.speed,
            self.throughput.per_runner_speed,
            &mut self.manager,
            now,
            self.backoff.is_congested(now),
            tick,
        ) else {
            return;
        };
        for action in actions {
            trace!("[TASK] applying strategy: {strategy_name}, action: {action:?}");
            self.apply(action, now);
        }
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    pub(super) fn apply(&mut self, action: StrategyAction, now: Instant) {
        match action {
            // Split current all task into two separate tasks
            StrategyAction::SplitAllTask(planned_chunk_size) => {
                trace!("[TASK] Strategy: split all task");
                let states = self.manager.get_incomplete_states(Some(planned_chunk_size));
                for (runner_id, incomplete_range) in states {
                    trace!("split task: {runner_id}, {incomplete_range:?}");
                    self.split(runner_id, incomplete_range, now);
                }
            }
            // Split the given task into two separate tasks
            StrategyAction::SplitGivenTask(task_id) => {
                trace!("[TASK] Strategy: split given task: {task_id}");
                let Some(state) = self.manager.get_runner_state(task_id) else {
                    trace!("[TASK] skip splitting runner {task_id}: it has no chunk");
                    return;
                };
                self.split(task_id, state.incomplete_range(), now);
            }
            // Change the max concurrency
            StrategyAction::ChangeMaxConcurrency(new_max_concurrency) => {
                trace!("[TASK] Strategy: change max concurrency: {new_max_concurrency}");
                self.max_concurrency = new_max_concurrency;
            }
            // Create background runners for specific range
            StrategyAction::CreateTask(range) => {
                trace!("[TASK] Strategy: create task: {range:?}");
                self.spawn_runner(range, now);
            }
            StrategyAction::Grow(runners) => {
                trace!("[TASK] Strategy: grow by {runners} runners");
                let added = self.grow(runners, now);
                self.strategy.on_grown(added);
            }
        }
    }

    /// Add up to `runners` runners, each by splitting the running chunk
    /// with the most left to download (design §9.2); returns how many were
    /// added.
    ///
    /// A chunk is split, at its half, only when it has at least the
    /// planned size for its runner left, see
    /// [`planned_split_size`](super::super::strategy::planned_split_size).
    /// Stops at the first chunk too small and the first split refused.
    fn grow(&mut self, runners: usize, now: Instant) -> usize {
        let mut added = 0;
        while added < runners {
            let Some((runner_id, incomplete)) = self.manager.largest_running_chunk() else {
                break;
            };
            let remaining = incomplete.end - incomplete.start;
            let planned = self.planned_split_size(runner_id);
            if remaining < planned {
                trace!(
                    "[TASK] skip growing: runner {runner_id} has {remaining} bytes left, \
                     {planned} planned"
                );
                break;
            }
            if !self.split(runner_id, incomplete, now) {
                break;
            }
            added += 1;
        }
        added
    }

    /// The planned split size for the chunk of `runner_id` (design §9.3).
    pub(super) fn planned_split_size(&self, runner_id: RunnerId) -> u64 {
        planned_split_size(
            self.rates.as_ref().and_then(|rates| rates.rate(runner_id)),
            self.throughput.per_runner_speed,
            self.manager.min_chunk_size(),
        )
    }

    /// Split the given runner's incomplete range in two, spawning a runner for the second half.
    ///
    /// The new runner's slot is reserved first, so the runner is only
    /// shrunk when the second half can be handed over; without a free slot
    /// nothing changes. Should the second half not be allocated after all,
    /// it stays unassigned, for the strategy to create a runner for.
    ///
    /// The new runner opens a connection, so nothing is split while the
    /// task is congested at `now`. The second half was part of a chunk,
    /// so it is never backing off.
    ///
    /// Returns whether a runner was spawned for the second half.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, fields(runner_id, range))
    )]
    pub(super) fn split(
        &mut self,
        runner_id: RunnerId,
        incomplete_range: Range<u64>,
        now: Instant,
    ) -> bool {
        if self.backoff.is_congested(now) {
            trace!("[TASK] skip splitting runner {runner_id}: the task is congested");
            return false;
        }
        let Some(slot) = self.manager.try_reserve_slot() else {
            trace!("[TASK] skip splitting runner {runner_id}: no runner slot is free");
            return false;
        };
        let half_size = (incomplete_range.end - incomplete_range.start) / 2;
        if let Err(e) = self.manager.resize_runner(runner_id, half_size) {
            trace!("[TASK] skip splitting runner {runner_id}: {e}");
            self.manager.release(slot);
            return false;
        }
        self.spawn_in(
            slot,
            incomplete_range.start + half_size..incomplete_range.end,
        )
    }

    /// Spawn a runner for `range`; without a free slot, when `range` is
    /// taken, while the task is congested at `now`, or while part of
    /// `range` is backing off, nothing changes.
    pub(super) fn spawn_runner(&mut self, range: Range<u64>, now: Instant) {
        if self.backoff.is_congested(now) {
            trace!("[TASK] skip spawning a runner for {range:?}: the task is congested");
            return;
        }
        if let Some(until) = self.manager.cooling_until(&range, now) {
            trace!(
                "[TASK] skip spawning a runner for {range:?}: backing off for {:?}",
                until - now
            );
            return;
        }
        let Some(slot) = self.manager.try_reserve_slot() else {
            trace!("[TASK] skip spawning a runner for {range:?}: no runner slot is free");
            return;
        };
        self.spawn_in(slot, range);
    }

    /// Returns whether a runner was spawned, see [`RunnerManager::spawn_in`].
    ///
    /// [`RunnerManager::spawn_in`]: super::super::RunnerManager::spawn_in
    fn spawn_in(&mut self, slot: RunnerSlot, range: Range<u64>) -> bool {
        let spawner = &self.spawner;
        let spawned = self
            .manager
            .spawn_in(slot, range.clone(), |runner_id, limits| {
                spawner.spawn(range.clone(), runner_id, limits)
            });
        if !spawned {
            trace!("[TASK] skip spawning a runner for {range:?}: the range is taken");
        }
        spawned
    }
}
