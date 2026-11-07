//! Runner Manager - Unified management of runner lifecycle and communication
//!
//! This module provides a unified interface to manage:
//! - Runner ID allocation and recycling (IdGenerator)
//! - Chunk allocation and progress tracking (ChunkPlanner)
//! - Aggregated reception of runner messages (RunnerNotification)
//! - Channels for sending control messages to specific runners

use std::{
    collections::{HashMap, VecDeque},
    ops::Range,
    task::{Context, Poll},
    time::Instant,
};

use bolt_load_utils::telemetry::*;
use bytes::Bytes;
use smol_cancellation_token::CancellationToken;

/// A chunk of data downloaded by a runner, ready to be written to disk.
#[derive(Debug)]
pub(super) struct DownloadedChunk {
    /// The runner that delivered it, whose silence it ends (design §9.6).
    pub runner_id: RunnerId,
    pub range: Range<u64>,
    pub bytes: Bytes,
}

/// A failed attempt of a single runner (connect or transfer).
///
/// Each attempt is reported at most once.
#[derive(Debug, Clone)]
pub(super) struct AttemptFailure {
    #[expect(dead_code, reason = "kept for diagnostics in `Debug` output")]
    pub runner_id: RunnerId,
    pub error: TaskError,
    pub retryable: bool,
    /// The unfinished range the runner gave back, fresh until the loop
    /// backs it off.
    pub range: Range<u64>,
    /// Failed attempts on `range`, this one included.
    pub attempts: u32,
    /// Failures on `range` charged to its budget, this one included when
    /// [`charges_range`] charges it (design §14.13).
    pub charged: u32,
    /// Bytes the planner accepted from this attempt, see
    /// [`Released::received`](super::chunk_planner::Released::received);
    /// 0 for a failed connect. Of a hedged attempt, only those of the
    /// failing runner's own side (design §9.6).
    pub received: u64,
    /// The loop stopped the runner for being slow (design §9.5), see
    /// [`Self::slow`].
    // Not `expect`: without tracing, `runner_id` and this field are
    // reported as one unread pair, which fulfils only the first
    // expectation.
    #[cfg_attr(
        not(feature = "tracing"),
        allow(dead_code, reason = "only read by tracing and the tests")
    )]
    pub slow: bool,
}

impl AttemptFailure {
    /// The failure of the runner whose range was `released`.
    #[cfg(test)]
    pub(super) fn new(
        runner_id: RunnerId,
        error: TaskError,
        retryable: bool,
        released: Released,
    ) -> Self {
        let own = released.received;
        Self::of_attempt(runner_id, error, retryable, released, own, None)
    }

    /// The failure of the runner whose range was `released`, which
    /// delivered `own` bytes itself; `first` is how the other side of its
    /// hedged attempt failed, if it did (design §9.6).
    ///
    /// The attempt is charged at most once for both sides: when neither is
    /// a restart and either is charged. Alone, a runner is charged as
    /// [`charges_range`] has it.
    pub(super) fn of_attempt(
        runner_id: RunnerId,
        error: TaskError,
        retryable: bool,
        released: Released,
        own: u64,
        first: Option<SideFailure>,
    ) -> Self {
        let charges = charges_range(&error, retryable, own);
        let charge = match first {
            None => charges,
            Some(first) => !first.restart && !is_restart(&error, own) && (first.charged || charges),
        };
        Self {
            runner_id,
            charged: released.charged.saturating_add(charge.into()),
            error,
            retryable,
            range: released.range,
            attempts: released.attempts,
            received: own,
            slow: false,
        }
    }

    /// The attempt of a runner the loop stopped for being slow, whose range
    /// was `released` (design §9.5, §14.15).
    ///
    /// It is reported as a retryable [`TaskError::Timeout`]: the runner
    /// fell too far behind the others for too long. So it is classified
    /// like one (design §8.2): after accepted bytes it is a restart, which
    /// is neither charged nor a consecutive failure; without, it is
    /// charged, so a runner that never delivers cannot be restarted
    /// forever.
    #[cfg(test)]
    pub(super) fn slow(runner_id: RunnerId, released: Released) -> Self {
        let own = released.received;
        Self::slow_of_attempt(runner_id, released, own, None)
    }

    /// [`Self::slow`] of a runner that delivered `own` bytes itself, see
    /// [`Self::of_attempt`].
    pub(super) fn slow_of_attempt(
        runner_id: RunnerId,
        released: Released,
        own: u64,
        first: Option<SideFailure>,
    ) -> Self {
        Self {
            slow: true,
            ..Self::of_attempt(runner_id, TaskError::Timeout, true, released, own, first)
        }
    }
}

/// A side of an undecided pair that failed while the other ran on (design
/// §9.6): its range stays with the other side, so nothing is released or
/// backed off, and the budget waits for the pair.
#[derive(Debug, Clone)]
pub(super) struct SideFailed {
    #[expect(dead_code, reason = "kept for diagnostics in `Debug` output")]
    pub runner_id: RunnerId,
    pub error: TaskError,
    pub retryable: bool,
    /// Bytes the planner accepted from this side itself.
    pub received: u64,
    /// The failed attempts of the range should it fail now, which set the
    /// backoff of a congestion without `Retry-After`.
    pub attempts: u32,
}

/// Bytes received from runners since the previous snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MeterSnapshot {
    /// Runners that delivered data and have not stopped since.
    pub runners: usize,
    /// Bytes received before the planner clipped them.
    pub bytes: usize,
}

/// Result of a single `RunnerManager::tick()` call.
#[derive(Debug)]
pub(super) struct RunnerTick {
    pub state: TaskState,
    pub downloaded: Option<DownloadedChunk>,
    pub failure: Option<AttemptFailure>,
    /// Boxed: it is rare and large, and every runner event carries a tick.
    pub side_failure: Option<Box<SideFailed>>,
}

impl RunnerTick {
    fn downloading() -> Self {
        Self {
            state: TaskState::Downloading,
            downloaded: None,
            failure: None,
            side_failure: None,
        }
    }

    fn finished() -> Self {
        Self {
            state: TaskState::Finished,
            ..Self::downloading()
        }
    }

    fn with_chunk(chunk: DownloadedChunk) -> Self {
        Self {
            downloaded: Some(chunk),
            ..Self::downloading()
        }
    }

    fn with_failure(failure: AttemptFailure) -> Self {
        Self {
            failure: Some(failure),
            ..Self::downloading()
        }
    }

    fn with_side_failure(failure: SideFailed) -> Self {
        Self {
            side_failure: Some(Box::new(failure)),
            ..Self::downloading()
        }
    }
}

/// Whether another attempt on the same range may succeed.
///
/// Besides adapter-retryable errors, a prematurely closed stream or channel is
/// treated as transient.
fn is_attempt_retryable(error: &TaskError) -> bool {
    error.is_retryable()
        || matches!(
            error,
            TaskError::SmallerThanTotalSize | TaskError::ChannelClosed
        )
}

use super::{
    ChunkPlanner, PlannerGuard,
    chunk_planner::{ChunkState, ChunkStatus, Released, Resize, SideFailure, SplitCandidate},
    strategy::{RunnerOutcomeSampler, charges_range, is_restart},
};
use crate::{
    runner::{
        DataFrameReceiver, LifecycleReceiver, LimitReceiver, LimitSender, TaskError, limit_mailbox,
    },
    task::{
        RunnerId,
        instance::{Generator, concurrent_task::strategy::RunnerFailureKind},
    },
};

mod data;
mod lifecycle;
mod stream;

use data::DataAggregator;
use lifecycle::LifecycleAggregator;

/// The receiving ends of a spawned runner, and its own stop.
pub(super) struct RunnerChannels {
    pub lifecycle_rx: LifecycleReceiver,
    pub data_rx: DataFrameReceiver,
    /// Cancels this runner alone: a child of the task's cancellation, see
    /// [`RunnerManager::stop_runner`].
    pub stop: CancellationToken,
}

/// A runner that could not be spawned.
#[derive(Debug)]
pub(super) struct SpawnFailed {
    pub message: String,
}

/// Why the loop stopped a runner through its own stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StopReason {
    /// Too slow (design §9.5, §14.15): its range is restarted.
    Slow,
    /// The beaten side of a hedged pair (design §9.6): nothing of it
    /// counts, and only its slot is reclaimed.
    Loser,
}

/// Errors that may occur when sending messages
#[derive(Debug, snafu::Snafu)]
pub enum SendError {
    #[snafu(display("Runner {runner_id} not found"))]
    RunnerNotFound { runner_id: RunnerId },
    #[snafu(display("Runner {runner_id} no longer receives limits"))]
    ReceiverGone { runner_id: RunnerId },
}

/// Errors that may occur when resizing a runner's chunk.
#[derive(Debug, snafu::Snafu)]
pub enum ResizeError {
    #[snafu(transparent)]
    Planner { source: super::chunk_planner::Error },
    #[snafu(transparent)]
    Limit { source: SendError },
}

/// A reserved runner ID and the mailbox for its limits.
///
/// Reserved by [`RunnerManager::try_reserve_slot`]; give it back with
/// [`RunnerManager::spawn_in`] or [`RunnerManager::release`].
#[must_use = "a reserved slot must be spawned in or released"]
pub struct RunnerSlot {
    /// Runner ID
    pub runner_id: RunnerId,
    /// The limits the manager sends to the runner
    pub limits: LimitReceiver,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// The task is downloading
    Downloading,
    /// No runner is allocated or running
    #[expect(dead_code, reason = "pausing is not implemented yet")]
    Paused,
    /// The task is finished
    Finished,
}

/// Runner Manager - Unified management of runner lifecycle and communication
pub struct RunnerManager {
    /// ID generator for managing runner ID allocation and recycling
    id_generator: Generator,
    /// Chunk planner for managing chunk allocation and progress
    chunk_planner: ChunkPlanner,

    /// Where to send each runner its limits
    limit_senders: HashMap<RunnerId, LimitSender>,
    /// Runners whose spawn was rejected, reported by the next tick
    spawn_failures: VecDeque<(RunnerId, SpawnFailed)>,
    /// Sampler for tracking the outcome of the runners
    runner_outcome_sampler: RunnerOutcomeSampler,
    /// Lifecycle aggregator for runners' lifecycle events
    lifecycle_aggregator: LifecycleAggregator,
    /// Data receivers of connecting runners, polled only once their runner
    /// reported `Started`: polling it with the lifecycle is not atomic, so
    /// data could otherwise be seen first.
    connecting_data: HashMap<RunnerId, DataFrameReceiver>,
    /// Data aggregator for runners' data frames
    data_aggregator: DataAggregator,
    /// Bytes received per runner since the last meter snapshot.
    ///
    /// An entry is created when a runner delivers data and removed when it
    /// stops or is lost; snapshots reset the counts but keep the entries.
    meters: HashMap<RunnerId, usize>,
    /// Each spawned runner's own stop, until it is released.
    stops: HashMap<RunnerId, CancellationToken>,
    /// Runners the loop stopped that did not confirm it yet, and why:
    /// by [`Self::stop_runner`], or beaten in a hedged pair.
    stopping: HashMap<RunnerId, StopReason>,
    /// When each runner last delivered a non-empty frame the planner
    /// accepted, or was spawned, or the writer could take data again: the
    /// start of its silence (design §9.6).
    last_delivery: HashMap<RunnerId, Instant>,
}

impl RunnerManager {
    /// Creates a new RunnerManager
    ///
    /// # Arguments
    /// * `total` - Total size of the file to download
    /// * `max_concurrency` - Maximum concurrency level
    pub fn new(total: u64, max_concurrency: usize) -> Self {
        Self {
            id_generator: Generator::new(max_concurrency),
            chunk_planner: ChunkPlanner::new(total),
            limit_senders: HashMap::with_capacity(max_concurrency),
            spawn_failures: VecDeque::new(),
            runner_outcome_sampler: RunnerOutcomeSampler::with_defaults(),
            lifecycle_aggregator: LifecycleAggregator::with_capacity(max_concurrency),
            connecting_data: HashMap::with_capacity(max_concurrency),
            data_aggregator: DataAggregator::with_capacity(max_concurrency),
            meters: HashMap::with_capacity(max_concurrency),
            stops: HashMap::with_capacity(max_concurrency),
            stopping: HashMap::new(),
            last_delivery: HashMap::with_capacity(max_concurrency),
        }
    }

    /// Take the bytes received since the previous snapshot.
    pub(super) fn take_meters_snapshot(&mut self) -> MeterSnapshot {
        MeterSnapshot {
            runners: self.meters.len(),
            bytes: self.meters.values_mut().map(std::mem::take).sum(),
        }
    }

    pub fn runner_outcome_sampler_mut(&mut self) -> &mut RunnerOutcomeSampler {
        &mut self.runner_outcome_sampler
    }

    // ==================== ID Generator Related Methods ====================

    /// Reserve a runner ID and the mailbox for its limits.
    ///
    /// `None` when every ID is taken: a runner keeps its ID until it
    /// stops, even after its chunk is complete.
    pub fn try_reserve_slot(&mut self) -> Option<RunnerSlot> {
        let runner_id = self.id_generator.next()?;
        let (sender, limits) = limit_mailbox();
        self.limit_senders.insert(runner_id, sender);
        Some(RunnerSlot { runner_id, limits })
    }

    /// Give back a slot no runner was spawned in.
    pub fn release(&mut self, slot: RunnerSlot) {
        self.release_runner(slot.runner_id);
    }

    /// Releases a runner ID and its associated resources
    ///
    /// This will:
    /// - Close and remove the limit mailbox
    /// - Remove from RunnerNotification (legacy) and aggregators (new)
    /// - Forget the runner's stop, that it was stopping, and its silence
    /// - Recycle the runner ID
    pub fn release_runner(&mut self, runner_id: RunnerId) {
        // The runner sees its limits closed once the sender is dropped
        self.limit_senders.remove(&runner_id);
        self.lifecycle_aggregator.remove(runner_id);
        self.connecting_data.remove(&runner_id);
        self.data_aggregator.unregister(runner_id);
        self.stops.remove(&runner_id);
        self.stopping.remove(&runner_id);
        self.last_delivery.remove(&runner_id);
        self.id_generator.release(runner_id);
    }

    /// Stop the runner of `runner_id` alone, for the loop to restart its
    /// range (design §9.5, §14.15); returns whether it was stopped.
    ///
    /// The runner is cancelled through its own stop, so it reports
    /// `Stopped` with [`TaskError::Cancelled`], which is then an
    /// [`AttemptFailure::slow`]: its range is released from the bytes the
    /// planner accepted. Its slot is freed only then. A runner that
    /// completes or fails on its own before it notices the stop is handled
    /// as such.
    ///
    /// Only a running runner with bytes left is stopped, once; not one
    /// without a stop of its own, nor a side of an undecided pair (design
    /// §9.6).
    pub fn stop_runner(&mut self, runner_id: RunnerId) -> bool {
        let running = self
            .chunk_planner
            .get_runner_state(runner_id)
            .is_some_and(|state| {
                state.status == ChunkStatus::Running
                    && !state.is_complete()
                    && !state.is_undecided()
            });
        if !running || self.stopping.contains_key(&runner_id) {
            return false;
        }
        let Some(stop) = self.stops.get(&runner_id) else {
            return false;
        };
        stop.cancel();
        self.stopping.insert(runner_id, StopReason::Slow);
        true
    }

    /// Whether the loop stopped the runner, for being slow or beaten in a
    /// hedged pair, and it has not confirmed it yet.
    #[inline]
    pub fn is_stopping(&self, runner_id: RunnerId) -> bool {
        self.stopping.contains_key(&runner_id)
    }

    /// The chunk `runner_id` downloads: its own, or as the hedge of an
    /// undecided pair, the original's.
    #[cfg(test)]
    pub(super) fn chunk_of(&self, runner_id: RunnerId) -> Option<&ChunkState> {
        self.chunk_planner
            .partner_chunk(runner_id)
            .or_else(|| self.chunk_planner.get_runner_state(runner_id))
    }

    /// Why the loop stopped the runner, while it has not confirmed it.
    #[inline]
    pub(super) fn stop_reason(&self, runner_id: RunnerId) -> Option<StopReason> {
        self.stopping.get(&runner_id).copied()
    }

    /// Whether the runner is a side of an undecided pair (design §9.6):
    /// either side while both run, or the side left after the other
    /// failed, until it delivers.
    pub fn is_in_undecided_pair(&self, runner_id: RunnerId) -> bool {
        self.chunk_planner.partner_chunk(runner_id).is_some()
            || self
                .chunk_planner
                .get_runner_state(runner_id)
                .is_some_and(ChunkState::is_undecided)
    }

    /// The other attempts in flight when one fails: the runners holding a
    /// slot, without those the loop stopped (design §9.6, M3). The runner
    /// that failed was released before.
    pub fn in_flight(&self) -> usize {
        self.id_generator.allocated_count() - self.stopping.len()
    }

    /// Beat `loser` in its hedged pair: stop it and drop its data, queued
    /// frames included. Its slot is kept until it confirms, see
    /// [`StopReason::Loser`]. The planner must have let it go already.
    fn stop_loser(&mut self, loser: RunnerId) {
        if let Some(stop) = self.stops.get(&loser) {
            stop.cancel();
        }
        self.stopping.insert(loser, StopReason::Loser);
        self.data_aggregator.unregister(loser);
        self.connecting_data.remove(&loser);
        self.meters.remove(&loser);
        self.last_delivery.remove(&loser);
    }

    // ==================== Silence (design §9.6) ====================

    /// The runner delivered a non-empty frame the planner accepted at `now`.
    pub(super) fn note_delivery(&mut self, runner_id: RunnerId, now: Instant) {
        if let Some(at) = self.last_delivery.get_mut(&runner_id) {
            *at = now;
        }
    }

    /// The writer can take data again at `now`: every silence starts over.
    pub(super) fn restart_silences(&mut self, now: Instant) {
        for at in self.last_delivery.values_mut() {
            *at = now;
        }
    }

    /// When the silence of `runner_id` started; `None` for a runner the
    /// loop did not spawn.
    pub(super) fn last_delivery(&self, runner_id: RunnerId) -> Option<Instant> {
        self.last_delivery.get(&runner_id).copied()
    }

    /// Gets the number of currently allocated runners
    #[cfg(test)]
    #[inline]
    pub fn allocated_runner_count(&self) -> usize {
        self.id_generator.allocated_count()
    }

    #[cfg(test)]
    /// Checks if maximum concurrency has been reached
    #[inline]
    pub fn is_full(&self) -> bool {
        self.id_generator.is_full()
    }

    #[cfg(test)]
    /// Checks if there are no runners
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.id_generator.is_empty()
    }

    // ==================== Control Related Methods ====================

    #[cfg(test)]
    /// Lower the runner's size limit to `limit`, replacing any limit it has
    /// not read yet.
    pub fn limit_runner(&self, runner_id: RunnerId, limit: u64) -> Result<(), SendError> {
        let sender = self
            .limit_senders
            .get(&runner_id)
            .ok_or(SendError::RunnerNotFound { runner_id })?;
        if sender.send(limit) {
            Ok(())
        } else {
            Err(SendError::ReceiverGone { runner_id })
        }
    }

    /// The limit the runner has not read yet.
    #[cfg(test)]
    pub(super) fn unread_limit(&self, runner_id: RunnerId) -> Option<u64> {
        self.limit_senders.get(&runner_id)?.unread()
    }

    // ==================== Runner Notification Related Methods ====================

    /// Registers both lifecycle and data receivers for a runner; the data of
    /// a connecting runner is polled once it reported `Started`.
    pub fn register_runner_channels(
        &mut self,
        runner_id: RunnerId,
        lifecycle_rx: LifecycleReceiver,
        data_rx: DataFrameReceiver,
    ) {
        self.lifecycle_aggregator.add(runner_id, lifecycle_rx);
        if self.is_connecting(runner_id) {
            self.connecting_data.insert(runner_id, data_rx);
        } else {
            self.data_aggregator.register(runner_id, data_rx);
        }
    }

    /// Gets the total file size
    #[inline]
    pub fn total(&self) -> u64 {
        self.chunk_planner.total
    }

    #[cfg(test)]
    /// Allocates a chunk to the specified runner
    #[inline]
    pub fn allocate_chunk(&mut self, range: Range<u64>, runner_id: RunnerId) -> bool {
        self.chunk_planner.allocate_chunk(range, Some(runner_id))
    }

    #[cfg(test)]
    /// Updates the download progress of a runner
    #[inline]
    pub fn update_progress(
        &mut self,
        runner_id: RunnerId,
        bytes_downloaded: u64,
    ) -> Result<Range<u64>, String> {
        self.chunk_planner
            .update_progress(runner_id, bytes_downloaded)
    }

    #[cfg(test)]
    /// Marks a runner as finished
    #[inline]
    pub fn mark_finished(
        &mut self,
        runner_id: RunnerId,
    ) -> Result<(), super::chunk_planner::Error> {
        self.chunk_planner.mark_finished(runner_id)
    }

    #[cfg(test)]
    /// Marks a runner as failed
    #[inline]
    pub fn mark_failed(
        &mut self,
        runner_id: RunnerId,
    ) -> Result<super::chunk_planner::Released, super::chunk_planner::Error> {
        self.chunk_planner.mark_failed(runner_id)
    }

    /// Gets the total amount downloaded
    #[inline]
    pub fn get_total_downloaded(&self) -> u64 {
        self.chunk_planner.get_total_downloaded()
    }

    /// Gets the downloaded ranges
    #[inline]
    pub fn get_downloaded_ranges(&self) -> Vec<Range<u64>> {
        self.chunk_planner.get_downloaded_ranges()
    }

    /// Gets the available ranges
    #[cfg(test)]
    #[inline]
    pub fn get_available_ranges(&self) -> Vec<Range<u64>> {
        self.chunk_planner.get_available_ranges()
    }

    /// Gets the available ranges a runner may claim at `now`
    #[inline]
    pub fn get_eligible_ranges(&self, now: Instant) -> Vec<Range<u64>> {
        self.chunk_planner.get_eligible_ranges(now)
    }

    /// Back an available range off, see [`ChunkPlanner::cool_down`]
    #[inline]
    pub fn cool_down(
        &mut self,
        range: Range<u64>,
        attempts: u32,
        charged: u32,
        not_before: Instant,
    ) {
        self.chunk_planner
            .cool_down(range, attempts, charged, not_before);
    }

    /// Until when a backoff still holds `range` back at `now`
    #[inline]
    pub fn cooling_until(&self, range: &Range<u64>, now: Instant) -> Option<Instant> {
        self.chunk_planner.cooling_until(range, now)
    }

    /// The earliest backoff that ends after `now`
    #[inline]
    pub fn next_cooldown_end(&self, now: Instant) -> Option<Instant> {
        self.chunk_planner.next_cooldown_end(now)
    }

    /// The backoffs of available ranges, in order
    #[cfg(test)]
    pub(super) fn cooldowns(&self) -> Vec<(Range<u64>, super::chunk_planner::Cooldown)> {
        self.chunk_planner.cooldowns()
    }

    /// Gets the number of active runners
    #[inline]
    pub fn get_active_runners_count(&self) -> usize {
        self.chunk_planner.get_active_runners_count()
    }

    /// Gets the state of the specified runner
    #[inline]
    pub fn get_runner_state(&self, runner_id: RunnerId) -> Option<&ChunkState> {
        self.chunk_planner.get_runner_state(runner_id)
    }

    /// Finds a chunk that can be split at `now`
    #[inline]
    pub fn find_chunk_to_split(&self, required_size: u64, now: Instant) -> Option<SplitCandidate> {
        self.chunk_planner.find_chunk_to_split(required_size, now)
    }

    /// The running runner with the most left to download, see
    /// [`ChunkPlanner::largest_running_chunk`]; not one that is stopping,
    /// whose range is about to be released.
    #[inline]
    pub fn largest_running_chunk(&self) -> Option<(RunnerId, Range<u64>)> {
        self.chunk_planner
            .largest_running_chunk(|runner_id| self.stopping.contains_key(&runner_id))
    }

    /// The smallest chunk the planner hands out
    #[inline]
    pub fn min_chunk_size(&self) -> u64 {
        self.chunk_planner.min_chunk_size
    }

    /// The chunks of the connecting and the running runners
    pub(super) fn active_chunks(&self) -> impl Iterator<Item = &ChunkState> {
        self.chunk_planner
            .iter_chunks()
            .filter(|chunk| chunk.status.is_active())
    }

    /// Gets incomplete states
    #[inline]
    pub fn get_incomplete_states(
        &self,
        min_chunk_size: Option<u64>,
    ) -> Vec<(RunnerId, Range<u64>)> {
        self.chunk_planner.get_incomplete_states(min_chunk_size)
    }

    // ==================== Combined Operations ====================

    #[cfg(test)]
    /// Allocates a runner that has already started on `range`.
    ///
    /// # Returns
    /// * `None` - If unable to allocate a runner ID or the range
    pub fn allocate_runner_with_chunk(&mut self, range: Range<u64>) -> Option<RunnerSlot> {
        let slot = self.try_reserve_slot()?;
        if !self.allocate_chunk(range, slot.runner_id) {
            self.release(slot);
            return None;
        }
        self.chunk_planner
            .mark_running(slot.runner_id)
            .expect("the chunk was just allocated");
        Some(slot)
    }

    #[cfg(test)]
    /// Allocate a runner for `range` and spawn it with `spawn`.
    ///
    /// # Returns
    /// * `None` - If unable to allocate a runner ID or the range
    pub fn spawn_runner_with_chunk(
        &mut self,
        range: Range<u64>,
        spawn: impl FnOnce(RunnerId, LimitReceiver) -> Result<RunnerChannels, SpawnFailed>,
    ) -> Option<()> {
        let slot = self.try_reserve_slot()?;
        self.spawn_in(slot, range, Instant::now(), spawn)
            .then_some(())
    }

    /// Allocate `range` to the runner of `slot` and spawn it with `spawn`
    /// at `now`, where its silence starts (design §9.6).
    ///
    /// The chunk stays pending until the runner reports `Started`. A rejected
    /// spawn is reported by the next tick, once.
    ///
    /// Returns `false`, with the slot released, when `range` cannot be
    /// allocated.
    pub fn spawn_in(
        &mut self,
        slot: RunnerSlot,
        range: Range<u64>,
        now: Instant,
        spawn: impl FnOnce(RunnerId, LimitReceiver) -> Result<RunnerChannels, SpawnFailed>,
    ) -> bool {
        let runner_id = slot.runner_id;
        if !self.chunk_planner.allocate_pending_chunk(range, runner_id) {
            self.release(slot);
            return false;
        }
        self.start_runner(runner_id, slot.limits, now, spawn);
        true
    }

    /// Hedge the chunk of `owner` with a runner in `slot`, spawned with
    /// `spawn` at `now` on the rest of the chunk (design §9.6).
    ///
    /// The planner allocates no range: the runner joins the chunk as the
    /// hedge side of an undecided pair. A rejected spawn is reported by the
    /// next tick, once.
    ///
    /// Returns `false`, with the slot released, when the chunk cannot be
    /// hedged.
    pub fn spawn_hedge(
        &mut self,
        slot: RunnerSlot,
        owner: RunnerId,
        now: Instant,
        spawn: impl FnOnce(RunnerId, LimitReceiver, Range<u64>) -> Result<RunnerChannels, SpawnFailed>,
    ) -> bool {
        let runner_id = slot.runner_id;
        let Ok(range) = self.chunk_planner.fork_hedge(owner, runner_id) else {
            self.release(slot);
            return false;
        };
        self.start_runner(runner_id, slot.limits, now, |runner_id, limits| {
            spawn(runner_id, limits, range)
        });
        true
    }

    fn start_runner(
        &mut self,
        runner_id: RunnerId,
        limits: LimitReceiver,
        now: Instant,
        spawn: impl FnOnce(RunnerId, LimitReceiver) -> Result<RunnerChannels, SpawnFailed>,
    ) {
        self.last_delivery.insert(runner_id, now);
        match spawn(runner_id, limits) {
            Ok(channels) => {
                self.register_runner_channels(runner_id, channels.lifecycle_rx, channels.data_rx);
                self.stops.insert(runner_id, channels.stop);
            }
            Err(failed) => self.spawn_failures.push_back((runner_id, failed)),
        }
    }

    #[cfg(test)]
    /// Fully releases a runner (including marking chunk as complete)
    ///
    /// # Arguments
    /// * `runner_id` - The runner ID to release
    /// * `finished` - Whether the runner completed successfully
    ///
    /// # Returns
    /// * `Ok(Option<Range<u64>>)` - If runner failed, returns the unfinished range
    /// * `Err` - If the operation fails
    pub fn release_runner_full(
        &mut self,
        runner_id: RunnerId,
        finished: bool,
    ) -> Result<Option<Range<u64>>, super::chunk_planner::Error> {
        let unfinished_range = if finished {
            self.mark_finished(runner_id)?;
            None
        } else {
            Some(self.mark_failed(runner_id)?.range)
        };

        self.release_runner(runner_id);
        Ok(unfinished_range)
    }

    /// Shrink the runner's chunk to end `new_size` bytes after its downloaded
    /// part, and limit the runner to match.
    ///
    /// On error nothing changes: the planner is rolled back when the runner
    /// no longer receives limits.
    #[inline]
    pub fn resize_runner(&mut self, runner_id: RunnerId, new_size: u64) -> Result<(), ResizeError> {
        let mut guard = PlannerGuard::new(&mut self.chunk_planner);
        let limit = match guard.planner().resize_runner_state(runner_id, new_size)? {
            Resize::Unchanged => return Ok(()),
            Resize::Shrunk { limit } => limit,
        };
        // The chunk only shrinks, so the new limit may replace an unread one.
        let sender = self
            .limit_senders
            .get(&runner_id)
            .ok_or(SendError::RunnerNotFound { runner_id })?;
        if !sender.send(limit) {
            return Err(SendError::ReceiverGone { runner_id }.into());
        }
        guard.commit();
        Ok(())
    }

    /// Returns true if the task is complete
    fn mark_runner_completed(&mut self, runner_id: RunnerId) -> bool {
        self.chunk_planner
            .mark_finished(runner_id)
            .expect("chunk planner should not fail");
        self.runner_outcome_sampler.record_completed();

        if self.chunk_planner.is_complete() {
            trace!("[TASK] all chunks finished");
            return true;
        }

        false
    }

    /// Whether the runner has not reported `Started` yet.
    fn is_connecting(&self, runner_id: RunnerId) -> bool {
        if let Some(chunk) = self.chunk_planner.partner_chunk(runner_id) {
            return chunk.partner().is_some_and(|partner| !partner.running);
        }
        self.chunk_planner
            .get_runner_state(runner_id)
            .is_some_and(|state| state.status == ChunkStatus::Pending)
    }

    /// Poll the runners: lifecycle first, so a runner starting or stopping
    /// is never starved by data, then data when `data_enabled`.
    #[inline]
    pub(super) fn poll_tick(
        &mut self,
        cx: &mut Context<'_>,
        data_enabled: bool,
    ) -> Poll<RunnerTick> {
        if let Poll::Ready(tick) = self.poll_lifecycle(cx) {
            return Poll::Ready(tick);
        }
        if data_enabled {
            self.poll_data(cx)
        } else {
            Poll::Pending
        }
    }

    /// Handle a runner's `Started` as its lifecycle event would.
    #[cfg(test)]
    pub(super) fn mark_started(&mut self, runner_id: RunnerId) {
        self.chunk_planner
            .mark_running(runner_id)
            .expect("a connecting runner has a chunk");
        if let Some(data_rx) = self.connecting_data.remove(&runner_id) {
            self.data_aggregator.register(runner_id, data_rx);
        }
    }

    /// The allocated range of every chunk.
    #[cfg(test)]
    pub(super) fn chunk_ranges(&self) -> Vec<Range<u64>> {
        self.chunk_planner
            .iter_chunks()
            .map(|chunk| chunk.allocated.clone())
            .collect()
    }

    #[cfg(test)]
    pub async fn tick(&mut self) -> RunnerTick {
        std::future::poll_fn(|cx| self.poll_tick(cx, true)).await
    }

    #[cfg(test)]
    pub async fn tick_lifecycle_only(&mut self) -> RunnerTick {
        std::future::poll_fn(|cx| self.poll_tick(cx, false)).await
    }
}

#[cfg(test)]
mod tests {
    use std::{pin::pin, time::Duration};

    use async_ringbuf::traits::Producer;
    use futures::StreamExt;

    use super::*;
    use crate::runner::{
        DATA_FRAME_CHANNEL_CAPACITY, LIFECYCLE_CHANNEL_CAPACITY, LifecycleEvent, StoppedReason,
    };

    #[test]
    fn test_runner_manager_creation() {
        let manager = RunnerManager::new(1000, 4);
        assert_eq!(manager.total(), 1000);
        assert_eq!(manager.allocated_runner_count(), 0);
        assert!(!manager.is_full());
        assert!(manager.is_empty());
    }

    #[test]
    fn test_allocate_and_release_runner() {
        let mut manager = RunnerManager::new(1000, 2);

        // Allocate the first runner
        let reg1 = manager.try_reserve_slot().unwrap();
        assert_eq!(reg1.runner_id, 0);
        assert_eq!(manager.allocated_runner_count(), 1);

        // Allocate the second runner
        let reg2 = manager.try_reserve_slot().unwrap();
        assert_eq!(reg2.runner_id, 1);
        assert_eq!(manager.allocated_runner_count(), 2);
        assert!(manager.is_full());

        // Cannot allocate more
        assert!(manager.try_reserve_slot().is_none());

        // Release the first runner
        manager.release_runner(reg1.runner_id);
        assert_eq!(manager.allocated_runner_count(), 1);
        assert!(!manager.is_full());

        // Can allocate again
        let reg3 = manager.try_reserve_slot().unwrap();
        assert_eq!(reg3.runner_id, 0); // Reuse released ID
    }

    #[test]
    fn test_limit_runner() {
        let mut manager = RunnerManager::new(1000, 2);
        let mut reg = manager.try_reserve_slot().unwrap();

        manager.limit_runner(reg.runner_id, 500).unwrap();
        assert_eq!(reg.limits.try_recv(), Some(500));

        assert!(matches!(
            manager.limit_runner(999, 500),
            Err(SendError::RunnerNotFound { runner_id: 999 })
        ));
        drop(reg.limits);
        assert!(matches!(
            manager.limit_runner(reg.runner_id, 400),
            Err(SendError::ReceiverGone { runner_id: 0 })
        ));
    }

    #[test]
    fn test_allocate_runner_with_chunk() {
        let mut manager = RunnerManager::new(1000, 2);

        // Allocate runner and chunk
        let reg = manager.allocate_runner_with_chunk(0..500).unwrap();

        assert_eq!(reg.runner_id, 0);

        // Verify chunk is allocated
        let state = manager.get_runner_state(reg.runner_id).unwrap();
        assert_eq!(state.allocated, 0..500);
    }

    #[test]
    fn test_resize_runner_shrinks_and_limits_the_runner() {
        let mut manager = RunnerManager::new(1000, 2);
        let mut reg = manager.allocate_runner_with_chunk(0..500).unwrap();
        manager.update_progress(reg.runner_id, 100).unwrap();

        manager.resize_runner(reg.runner_id, 150).unwrap();
        assert_eq!(
            manager.get_runner_state(reg.runner_id).unwrap().allocated,
            0..250
        );
        assert_eq!(reg.limits.try_recv(), Some(250));
    }

    #[test]
    fn test_resize_runner_rejected_or_unchanged_sends_nothing() {
        let mut manager = RunnerManager::new(1000, 2);
        let mut reg = manager.allocate_runner_with_chunk(0..500).unwrap();
        manager.update_progress(reg.runner_id, 100).unwrap();

        // Growing is rejected, keeping the chunk.
        assert!(matches!(
            manager.resize_runner(reg.runner_id, 600),
            Err(ResizeError::Planner {
                source: super::super::chunk_planner::Error::InvalidResize { .. }
            })
        ));
        // The same end is a no-op.
        manager.resize_runner(reg.runner_id, 400).unwrap();

        assert_eq!(
            manager.get_runner_state(reg.runner_id).unwrap().allocated,
            0..500
        );
        assert_eq!(reg.limits.try_recv(), None);
    }

    #[test]
    fn test_resize_unread_runner_keeps_the_latest_limit() {
        let mut manager = RunnerManager::new(1 << 40, 2);
        let mut reg = manager.allocate_runner_with_chunk(0..1 << 40).unwrap();
        // Far more reductions than any queue would hold, none read.
        for _ in 0..4096 {
            let incomplete = manager.get_runner_state(0).unwrap().incomplete_range();
            manager.resize_runner(0, incomplete.end - 1).unwrap();
        }
        assert_eq!(
            manager.get_runner_state(0).unwrap().allocated,
            0..(1 << 40) - 4096
        );
        assert_eq!(reg.limits.try_recv(), Some((1 << 40) - 4096));
        assert_eq!(reg.limits.try_recv(), None);
    }

    #[test]
    fn test_resize_runner_rolls_back_when_the_runner_is_gone() {
        let mut manager = RunnerManager::new(1000, 2);
        let reg = manager.allocate_runner_with_chunk(0..512).unwrap();
        drop(reg.limits);
        assert!(matches!(
            manager.resize_runner(0, 256),
            Err(ResizeError::Limit {
                source: SendError::ReceiverGone { runner_id: 0 }
            })
        ));
        assert_eq!(manager.get_runner_state(0).unwrap().allocated, 0..512);
    }

    #[test]
    fn test_release_runner_full() {
        let mut manager = RunnerManager::new(1000, 2);

        // Allocate runner and chunk
        let reg = manager.allocate_runner_with_chunk(0..500).unwrap();

        // Update progress
        manager.update_progress(reg.runner_id, 200).unwrap();

        // Release (failure scenario)
        let result = manager.release_runner_full(reg.runner_id, false).unwrap();
        assert_eq!(result, Some(200..500));

        // Runner should have been released
        assert!(manager.is_empty());
    }

    /// Channels for a runner, and their sending ends.
    fn test_channels() -> (
        RunnerChannels,
        crate::runner::LifecycleSender,
        crate::runner::DataFrameSender,
    ) {
        use async_ringbuf::{AsyncHeapRb, traits::Split};

        use crate::runner::{DataFrame, LifecycleEvent};

        let (lifecycle_tx, lifecycle_rx) =
            AsyncHeapRb::<LifecycleEvent>::new(LIFECYCLE_CHANNEL_CAPACITY).split();
        let (data_tx, data_rx) = AsyncHeapRb::<DataFrame>::new(DATA_FRAME_CHANNEL_CAPACITY).split();
        let channels = RunnerChannels {
            lifecycle_rx,
            data_rx,
            stop: Default::default(),
        };
        (channels, lifecycle_tx, data_tx)
    }

    /// Spawn a runner on `range` whose channels the test drives.
    fn spawn_test_runner(
        manager: &mut RunnerManager,
        range: Range<u64>,
    ) -> Option<(
        crate::runner::LifecycleSender,
        crate::runner::DataFrameSender,
    )> {
        let mut senders = None;
        manager.spawn_runner_with_chunk(range, |_, _limits| {
            let (channels, lifecycle_tx, data_tx) = test_channels();
            senders = Some((lifecycle_tx, data_tx));
            Ok(channels)
        })?;
        senders
    }

    fn unretryable_stream_error() -> TaskError {
        TaskError::StreamError {
            source: bolt_load_core::adapter::AdapterError::Unretryable {
                source: crate::adapter::UnretryableError::Whatever {
                    message: "test connection error".to_string(),
                    source: None,
                },
            },
        }
    }

    #[test]
    fn test_spawned_runner_is_pending_until_started() {
        let mut manager = RunnerManager::new(1000, 4);
        for i in 0..4 {
            assert!(spawn_test_runner(&mut manager, i * 250..(i + 1) * 250).is_some());
        }
        assert_eq!(manager.allocated_runner_count(), 4);
        assert!(manager.is_full());
        for id in 0..4 {
            assert_eq!(
                manager.get_runner_state(id).unwrap().status,
                ChunkStatus::Pending
            );
        }
        // Cannot allocate more
        assert!(
            manager
                .spawn_runner_with_chunk(0..100, |_, _| unreachable!("no runner id is left"))
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_started_marks_the_chunk_running() {
        let mut manager = RunnerManager::new(1000, 2);
        let (mut lifecycle_tx, mut data_tx) = spawn_test_runner(&mut manager, 0..500).unwrap();
        lifecycle_tx.try_push(LifecycleEvent::Started).unwrap();
        data_tx
            .try_push(crate::runner::DataFrame {
                data: Bytes::from_static(b"abc"),
            })
            .unwrap();

        // The lifecycle comes first, so the chunk runs before its data arrives.
        let tick = manager.tick().await;
        assert_eq!(tick.state, TaskState::Downloading);
        assert!(tick.downloaded.is_none());
        assert_eq!(
            manager.get_runner_state(0).unwrap().status,
            ChunkStatus::Running
        );
        assert_eq!(manager.tick().await.downloaded.unwrap().range, 0..3);
    }

    #[tokio::test]
    async fn test_data_cannot_overtake_started_between_source_polls() {
        let mut manager = RunnerManager::new(3, 1);
        let (mut lifecycle_tx, mut data_tx) = spawn_test_runner(&mut manager, 0..3).unwrap();

        // Both arrive between a tick's lifecycle poll and its data poll.
        assert!(futures::poll!(pin!(manager.lifecycle_aggregator.next())).is_pending());
        lifecycle_tx.try_push(LifecycleEvent::Started).unwrap();
        data_tx
            .try_push(crate::runner::DataFrame {
                data: Bytes::from_static(b"abc"),
            })
            .unwrap();
        assert!(futures::poll!(pin!(manager.data_aggregator.next())).is_pending());
        assert_eq!(manager.get_total_downloaded(), 0);

        // Started is handled under writer backpressure too.
        assert!(manager.tick_lifecycle_only().await.downloaded.is_none());
        assert_eq!(
            manager.get_runner_state(0).unwrap().status,
            ChunkStatus::Running
        );
        assert_eq!(manager.tick().await.downloaded.unwrap().range, 0..3);
    }

    #[test]
    fn test_poll_tick_parks_once_every_source_is_exhausted() {
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut manager = RunnerManager::new(1000, 2);
        assert!(manager.poll_tick(&mut cx, true).is_pending());

        let (lifecycle_tx, data_tx) = spawn_test_runner(&mut manager, 0..500).unwrap();
        drop((lifecycle_tx, data_tx));
        let Poll::Ready(tick) = manager.poll_tick(&mut cx, true) else {
            panic!("the closed lifecycle is reported");
        };
        assert!(matches!(
            tick.failure,
            Some(AttemptFailure {
                error: TaskError::ChannelClosed,
                ..
            })
        ));
        // Nothing is left: no source keeps reporting it ended.
        for data_enabled in [true, false, true] {
            assert!(manager.poll_tick(&mut cx, data_enabled).is_pending());
        }
    }

    #[tokio::test]
    async fn test_stopped_before_started_is_a_connect_failure() {
        let mut manager = RunnerManager::new(1000, 2);
        let (mut lifecycle_tx, _data_tx) = spawn_test_runner(&mut manager, 0..500).unwrap();
        lifecycle_tx
            .try_push(LifecycleEvent::failed(unretryable_stream_error()))
            .unwrap();

        let failure = manager.tick().await.failure.unwrap();
        assert!(failure.error.is_stream_error());
        assert!(!failure.retryable);
        assert!(manager.get_runner_state(0).is_none());
        assert_eq!(manager.get_available_ranges(), vec![0..1000]);
        assert!(manager.is_empty());
        let stats = manager.runner_outcome_sampler_mut().stats();
        assert_eq!(stats.connect_failed_unretryable, 1);
        assert_eq!(stats.stream_closed_unretryable, 0);
    }

    #[tokio::test]
    async fn test_congestion_while_connecting_is_still_recognised() {
        use std::time::Instant;

        use super::super::strategy::FailurePolicy;
        use crate::{adapter::UnretryableError, task::TaskOptions};

        let mut manager = RunnerManager::new(1000, 2);
        let (mut lifecycle_tx, _data_tx) = spawn_test_runner(&mut manager, 0..500).unwrap();
        let _other = spawn_test_runner(&mut manager, 500..1000).unwrap();
        lifecycle_tx
            .try_push(LifecycleEvent::failed(TaskError::StreamError {
                source: bolt_load_core::adapter::AdapterError::Unretryable {
                    source: UnretryableError::Congestion {
                        status: 429,
                        retry_after: None,
                    },
                },
            }))
            .unwrap();

        let failure = manager.tick().await.failure.unwrap();
        let mut policy = FailurePolicy::new(&TaskOptions::default(), Instant::now());
        // Left to the concurrency strategy while the other runner is in flight.
        assert!(
            policy
                .on_failure(&failure, manager.allocated_runner_count())
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_exceeded_total_size_before_started_does_not_finish_the_chunk() {
        let mut manager = RunnerManager::new(500, 1);
        let (mut lifecycle_tx, _data_tx) = spawn_test_runner(&mut manager, 0..500).unwrap();
        lifecycle_tx
            .try_push(LifecycleEvent::failed(TaskError::ExceededTotalSize))
            .unwrap();

        let tick = manager.tick().await;
        assert_eq!(tick.state, TaskState::Downloading);
        assert!(matches!(
            tick.failure,
            Some(AttemptFailure {
                error: TaskError::ExceededTotalSize,
                ..
            })
        ));
        assert_eq!(manager.get_available_ranges(), vec![0..500]);
    }

    #[tokio::test]
    async fn test_cancelled_while_connecting_is_not_a_failure() {
        let mut manager = RunnerManager::new(1000, 2);
        let (mut lifecycle_tx, _data_tx) = spawn_test_runner(&mut manager, 0..500).unwrap();
        lifecycle_tx
            .try_push(LifecycleEvent::failed(TaskError::Cancelled))
            .unwrap();

        assert!(manager.tick().await.failure.is_none());
        assert_eq!(manager.get_available_ranges(), vec![0..1000]);
        assert_eq!(manager.runner_outcome_sampler_mut().stats().total, 0);
    }

    #[tokio::test]
    async fn test_lifecycle_closed_before_started_is_a_lost_connect() {
        let mut manager = RunnerManager::new(1000, 2);
        let (lifecycle_tx, _data_tx) = spawn_test_runner(&mut manager, 0..500).unwrap();
        drop(lifecycle_tx);

        let failure = manager.tick_lifecycle_only().await.failure.unwrap();
        assert!(matches!(failure.error, TaskError::ChannelClosed));
        assert!(failure.retryable);
        let stats = manager.runner_outcome_sampler_mut().stats();
        assert_eq!(stats.connect_failed_retryable, 1);
        assert_eq!(stats.stream_closed_retryable, 0);
    }

    #[tokio::test]
    async fn test_rejected_spawn_is_reported_once() {
        let mut manager = RunnerManager::new(1000, 2);
        manager
            .spawn_runner_with_chunk(0..500, |_, _| {
                Err(SpawnFailed {
                    message: "runtime is shut down".to_string(),
                })
            })
            .unwrap();
        assert_eq!(
            manager.get_runner_state(0).unwrap().status,
            ChunkStatus::Pending
        );

        let failure = manager.tick_lifecycle_only().await.failure.unwrap();
        assert!(matches!(failure.error, TaskError::Other { .. }));
        assert!(!failure.retryable);
        assert!(manager.is_empty());
        assert_eq!(manager.get_available_ranges(), vec![0..1000]);
        assert_eq!(
            manager
                .runner_outcome_sampler_mut()
                .stats()
                .connect_failed_unretryable,
            1
        );
        // Nothing else is left to report.
        assert!(futures::poll!(std::pin::pin!(manager.tick())).is_pending());
    }

    // ==================== New Two-Channel Architecture Tests ====================

    fn register_test_channels(
        manager: &mut RunnerManager,
        runner_id: RunnerId,
    ) -> (
        crate::runner::LifecycleSender,
        crate::runner::DataFrameSender,
    ) {
        use async_ringbuf::{AsyncHeapRb, traits::Split};

        use crate::runner::{DataFrame, LifecycleEvent};

        let lifecycle_rb = AsyncHeapRb::<LifecycleEvent>::new(LIFECYCLE_CHANNEL_CAPACITY);
        let (lifecycle_prod, lifecycle_cons) = lifecycle_rb.split();

        let data_rb = AsyncHeapRb::<DataFrame>::new(DATA_FRAME_CHANNEL_CAPACITY);
        let (data_prod, data_cons) = data_rb.split();

        manager.register_runner_channels(runner_id, lifecycle_cons, data_cons);
        (lifecycle_prod, data_prod)
    }

    #[tokio::test]
    async fn test_new_arch_lifecycle_stopped_cleanup() {
        use async_ringbuf::{
            AsyncHeapRb,
            traits::{AsyncProducer, Split},
        };

        use crate::runner::{DataFrame, LifecycleEvent};

        let mut manager = RunnerManager::new(1000, 2);
        let runner_id = 0;

        // Create lifecycle channel using ringbuf
        let lifecycle_rb = AsyncHeapRb::<LifecycleEvent>::new(LIFECYCLE_CHANNEL_CAPACITY);
        let (mut lifecycle_prod, lifecycle_cons) = lifecycle_rb.split();

        // Allocate runner and chunk
        let _reg = manager.allocate_runner_with_chunk(0..500).unwrap();

        // Create data channel
        let data_rb = AsyncHeapRb::<DataFrame>::new(DATA_FRAME_CHANNEL_CAPACITY);
        let (_data_prod, data_cons) = data_rb.split();

        // Register both channels atomically
        manager.register_runner_channels(runner_id, lifecycle_cons, data_cons);

        // Now emit Stopped
        lifecycle_prod
            .push(LifecycleEvent::Stopped(StoppedReason::Finished))
            .await
            .unwrap();
        let tick = manager.tick().await;

        assert_eq!(tick.state, TaskState::Downloading);
        assert!(tick.downloaded.is_none());

        // Runner should be released
        assert!(manager.get_runner_state(runner_id).is_none());
        // Data aggregator should not contain the runner
        assert!(!manager.data_aggregator.contains(runner_id));
    }

    #[tokio::test]
    async fn test_exceeded_total_size_last_runner_finishes() {
        use async_ringbuf::traits::AsyncProducer;

        use crate::runner::LifecycleEvent;

        let mut manager = RunnerManager::new(256, 1);
        let runner_id = 0;
        let _reg = manager.allocate_runner_with_chunk(0..256).unwrap();
        let (mut lifecycle_prod, _data_prod) = register_test_channels(&mut manager, runner_id);

        lifecycle_prod
            .push(LifecycleEvent::Stopped(StoppedReason::Failed(
                TaskError::ExceededTotalSize,
            )))
            .await
            .unwrap();
        let tick = tokio::time::timeout(Duration::from_secs(1), manager.tick())
            .await
            .expect("exceeded-total-size lifecycle event should be observed");

        assert_eq!(tick.state, TaskState::Finished);
        assert!(tick.downloaded.is_none());
        assert!(manager.get_runner_state(runner_id).is_none());
        assert!(manager.is_empty());
    }

    #[tokio::test]
    async fn test_lifecycle_only_exceeded_total_size_last_runner_finishes() {
        use async_ringbuf::traits::AsyncProducer;

        use crate::runner::LifecycleEvent;

        let mut manager = RunnerManager::new(256, 1);
        let runner_id = 0;
        let _reg = manager.allocate_runner_with_chunk(0..256).unwrap();
        let (mut lifecycle_prod, _data_prod) = register_test_channels(&mut manager, runner_id);

        lifecycle_prod
            .push(LifecycleEvent::Stopped(StoppedReason::Failed(
                TaskError::ExceededTotalSize,
            )))
            .await
            .unwrap();
        let tick = tokio::time::timeout(Duration::from_secs(1), manager.tick_lifecycle_only())
            .await
            .expect("exceeded-total-size lifecycle event should be observed");

        assert_eq!(tick.state, TaskState::Finished);
        assert!(tick.downloaded.is_none());
        assert!(manager.get_runner_state(runner_id).is_none());
        assert!(manager.is_empty());
    }

    #[tokio::test]
    async fn test_tick_finishes_when_lifecycle_stream_closes_after_full_progress() {
        let mut manager = RunnerManager::new(256, 1);
        let runner_id = 0;
        let _reg = manager.allocate_runner_with_chunk(0..256).unwrap();
        manager.update_progress(runner_id, 256).unwrap();
        let (lifecycle_prod, _data_prod) = register_test_channels(&mut manager, runner_id);

        drop(lifecycle_prod);
        let tick = tokio::time::timeout(Duration::from_secs(1), manager.tick())
            .await
            .expect("closed lifecycle stream should be observed");

        assert_eq!(tick.state, TaskState::Finished);
        assert!(tick.downloaded.is_none());
        assert!(manager.get_runner_state(runner_id).is_none());
        assert!(manager.is_empty());
    }

    #[tokio::test]
    async fn test_lifecycle_only_finishes_when_lifecycle_stream_closes_after_full_progress() {
        let mut manager = RunnerManager::new(256, 1);
        let runner_id = 0;
        let _reg = manager.allocate_runner_with_chunk(0..256).unwrap();
        manager.update_progress(runner_id, 256).unwrap();
        let (lifecycle_prod, _data_prod) = register_test_channels(&mut manager, runner_id);

        drop(lifecycle_prod);
        let tick = tokio::time::timeout(Duration::from_secs(1), manager.tick_lifecycle_only())
            .await
            .expect("closed lifecycle stream should be observed");

        assert_eq!(tick.state, TaskState::Finished);
        assert!(tick.downloaded.is_none());
        assert!(manager.get_runner_state(runner_id).is_none());
        assert!(manager.is_empty());
    }

    #[tokio::test]
    async fn test_data_channel_closed_before_lifecycle_stopped_is_handled() {
        use async_ringbuf::{
            AsyncHeapRb,
            traits::{AsyncProducer, Split},
        };

        use crate::runner::{DataFrame, LifecycleEvent};

        let mut manager = RunnerManager::new(500, 1);
        let runner_id = 0;
        let _reg = manager.allocate_runner_with_chunk(0..500).unwrap();

        let data_rb = AsyncHeapRb::<DataFrame>::new(DATA_FRAME_CHANNEL_CAPACITY);
        let (data_prod, data_cons) = data_rb.split();

        let lifecycle_rb = AsyncHeapRb::<LifecycleEvent>::new(LIFECYCLE_CHANNEL_CAPACITY);
        let (mut lifecycle_prod, lifecycle_cons) = lifecycle_rb.split();

        manager.register_runner_channels(runner_id, lifecycle_cons, data_cons);

        drop(data_prod);
        let tick = tokio::time::timeout(Duration::from_secs(1), manager.tick())
            .await
            .expect("data channel close should be observed without blocking");
        assert_eq!(tick.state, TaskState::Downloading);
        assert!(tick.downloaded.is_none());
        assert!(!manager.data_aggregator.contains(runner_id));
        assert!(manager.get_runner_state(runner_id).is_some());

        lifecycle_prod
            .push(LifecycleEvent::Stopped(StoppedReason::Finished))
            .await
            .unwrap();

        let tick = tokio::time::timeout(Duration::from_secs(1), manager.tick())
            .await
            .expect("lifecycle stopped should still be handled after data closes");
        assert_eq!(tick.state, TaskState::Finished);
        assert!(tick.downloaded.is_none());
        assert!(manager.get_runner_state(runner_id).is_none());
        assert!(manager.is_empty());
    }

    #[tokio::test]
    async fn test_lifecycle_only_tick_handles_stopped_during_writer_backpressure() {
        use async_ringbuf::{
            AsyncHeapRb,
            traits::{AsyncProducer, Split},
        };

        use crate::runner::{DataFrame, LifecycleEvent};

        let mut manager = RunnerManager::new(256, 1);
        let runner_id = 0;
        let _reg = manager.allocate_runner_with_chunk(0..256).unwrap();

        let data_rb = AsyncHeapRb::<DataFrame>::new(DATA_FRAME_CHANNEL_CAPACITY);
        let (mut data_prod, data_cons) = data_rb.split();

        let lifecycle_rb = AsyncHeapRb::<LifecycleEvent>::new(LIFECYCLE_CHANNEL_CAPACITY);
        let (mut lifecycle_prod, lifecycle_cons) = lifecycle_rb.split();

        manager.register_runner_channels(runner_id, lifecycle_cons, data_cons);
        data_prod
            .push(DataFrame {
                data: Bytes::from(vec![1; 64]),
            })
            .await
            .unwrap();
        lifecycle_prod
            .push(LifecycleEvent::Stopped(StoppedReason::Failed(
                TaskError::Cancelled,
            )))
            .await
            .unwrap();
        let tick = tokio::time::timeout(Duration::from_secs(1), manager.tick_lifecycle_only())
            .await
            .expect("lifecycle-only tick should not wait for writer capacity");

        assert_eq!(tick.state, TaskState::Downloading);
        assert!(tick.downloaded.is_none());
        assert!(manager.get_runner_state(runner_id).is_none());
        assert!(!manager.data_aggregator.contains(runner_id));
        assert_eq!(manager.get_available_ranges(), vec![0..256]);
    }

    #[tokio::test]
    async fn test_drain_timeout_releases_chunk_for_retry() {
        use async_stream::stream;
        use smol_cancellation_token::CancellationToken;

        use crate::runner::TaskRunner;

        let mut manager = RunnerManager::new(64, 1);
        let runner_id = 0;
        let test_stream = stream! {
            yield Ok(Bytes::from(vec![7; 64]));
        };
        let mut runner = None;
        manager
            .spawn_runner_with_chunk(0..64, |id, limits| {
                let (built, lifecycle_rx, data_rx) = TaskRunner::builder()
                    .total(64)
                    .stream(Box::pin(test_stream))
                    .runner_id(id)
                    .limit_receiver(limits)
                    .cancel_token(CancellationToken::new())
                    .data_drain_timeout(Duration::from_millis(50))
                    .build()
                    .unwrap();
                runner = Some(built);
                Ok(RunnerChannels {
                    lifecycle_rx,
                    data_rx,
                    stop: Default::default(),
                })
            })
            .unwrap();
        let mut runner = runner.unwrap();

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });
        let tick = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let tick = manager.tick_lifecycle_only().await;
                if manager.get_runner_state(runner_id).is_none() {
                    break tick;
                }
            }
        })
        .await
        .expect("drain timeout should surface as lifecycle failure");

        assert_eq!(tick.state, TaskState::Downloading);
        assert!(tick.downloaded.is_none());
        assert_eq!(manager.get_available_ranges(), vec![0..64]);
        assert!(manager.is_empty());

        let stats = manager.runner_outcome_sampler_mut().stats();
        assert_eq!(stats.stream_closed_retryable, 1);
        assert_eq!(stats.stream_closed_unretryable, 0);

        runner_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_new_arch_data_aggregator_receives_data() {
        use async_ringbuf::{
            AsyncHeapRb,
            traits::{AsyncProducer, Split},
        };

        use crate::runner::{DataFrame, LifecycleEvent};

        let mut manager = RunnerManager::new(1000, 2);
        let runner_id = 0;

        // Allocate runner
        let _ = manager.allocate_runner_with_chunk(0..500).unwrap();

        // Create lifecycle channel (needed for combined registration)
        let lifecycle_rb = AsyncHeapRb::<LifecycleEvent>::new(LIFECYCLE_CHANNEL_CAPACITY);
        let (_lifecycle_prod, lifecycle_cons) = lifecycle_rb.split();

        // Create data channel
        let data_rb = AsyncHeapRb::<DataFrame>::new(DATA_FRAME_CHANNEL_CAPACITY);
        let (mut data_prod, data_cons) = data_rb.split();

        // Register both channels atomically
        manager.register_runner_channels(runner_id, lifecycle_cons, data_cons);

        // Send data
        data_prod
            .push(DataFrame {
                data: Bytes::from(vec![1; 100]),
            })
            .await
            .unwrap();

        // Tick to process data
        let tick = manager.tick().await;

        assert_eq!(tick.state, TaskState::Downloading);
        let chunk = tick.downloaded.expect("should have downloaded chunk");
        assert_eq!(chunk.range, 0..100);
        assert_eq!(chunk.bytes.len(), 100);
    }

    async fn push_frame(data_tx: &mut crate::runner::DataFrameSender, len: usize) {
        use async_ringbuf::traits::AsyncProducer;

        data_tx
            .push(crate::runner::DataFrame {
                data: Bytes::from(vec![1; len]),
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_meters_count_bytes_before_clipping() {
        let mut manager = RunnerManager::new(1000, 2);
        let runner_id = manager
            .allocate_runner_with_chunk(0..100)
            .unwrap()
            .runner_id;
        let (_lifecycle_tx, mut data_tx) = register_test_channels(&mut manager, runner_id);

        push_frame(&mut data_tx, 150).await;
        let chunk = manager.tick().await.downloaded.expect("a chunk");

        assert_eq!(chunk.range, 0..100);
        assert_eq!(
            manager.take_meters_snapshot(),
            MeterSnapshot {
                runners: 1,
                bytes: 150
            }
        );
    }

    /// A failure reports as received only what the planner accepted from
    /// the attempt (design §14.13): not the frames left in the channel when
    /// the drain timed out, nor the bytes clipped after a resize. Only an
    /// attempt with accepted bytes restarts; any other is charged.
    #[tokio::test]
    async fn test_a_failure_reports_only_the_bytes_the_planner_accepted() {
        use async_ringbuf::traits::Producer;

        let mut manager = RunnerManager::new(1000, 3);
        let timed_out = || LifecycleEvent::failed(TaskError::Timeout);

        // The drain timed out with every frame still in the channel.
        let first = manager
            .allocate_runner_with_chunk(0..300)
            .unwrap()
            .runner_id;
        let (mut lifecycle_tx, mut data_tx) = register_test_channels(&mut manager, first);
        push_frame(&mut data_tx, 100).await;
        lifecycle_tx.try_push(timed_out()).unwrap();
        let failure = manager.tick_lifecycle_only().await.failure.unwrap();
        assert_eq!(
            (
                failure.range,
                failure.received,
                failure.attempts,
                failure.charged
            ),
            (0..300, 0, 1, 1)
        );

        // One frame was accepted, the next one was left in the channel.
        let second = manager
            .allocate_runner_with_chunk(300..600)
            .unwrap()
            .runner_id;
        let (mut lifecycle_tx, mut data_tx) = register_test_channels(&mut manager, second);
        push_frame(&mut data_tx, 100).await;
        assert_eq!(manager.tick().await.downloaded.unwrap().range, 300..400);
        push_frame(&mut data_tx, 50).await;
        lifecycle_tx.try_push(timed_out()).unwrap();
        let failure = manager.tick_lifecycle_only().await.failure.unwrap();
        assert_eq!(
            (
                failure.range,
                failure.received,
                failure.attempts,
                failure.charged
            ),
            (400..600, 100, 1, 0)
        );

        // The chunk shrank to end 50 bytes after its 100 downloaded ones,
        // but the runner sent 200 more before it read its new limit.
        let mut third = manager.allocate_runner_with_chunk(600..1000).unwrap();
        let (mut lifecycle_tx, mut data_tx) = register_test_channels(&mut manager, third.runner_id);
        push_frame(&mut data_tx, 100).await;
        manager.tick().await;
        manager.resize_runner(third.runner_id, 50).unwrap();
        assert_eq!(third.limits.try_recv(), Some(150));
        push_frame(&mut data_tx, 200).await;
        assert_eq!(manager.tick().await.downloaded.unwrap().range, 700..750);
        lifecycle_tx.try_push(timed_out()).unwrap();
        let failure = manager.tick_lifecycle_only().await.failure.unwrap();
        assert_eq!(
            (failure.range, failure.received, failure.charged),
            (750..750, 150, 0)
        );
    }

    #[tokio::test]
    async fn test_meters_snapshot_resets_counts_but_keeps_entries() {
        let mut manager = RunnerManager::new(1000, 2);
        let runner_id = manager
            .allocate_runner_with_chunk(0..500)
            .unwrap()
            .runner_id;
        let (_lifecycle_tx, mut data_tx) = register_test_channels(&mut manager, runner_id);

        push_frame(&mut data_tx, 10).await;
        manager.tick().await;

        assert_eq!(
            manager.take_meters_snapshot(),
            MeterSnapshot {
                runners: 1,
                bytes: 10
            }
        );
        assert_eq!(
            manager.take_meters_snapshot(),
            MeterSnapshot {
                runners: 1,
                bytes: 0
            }
        );
    }

    #[tokio::test]
    async fn test_meters_entry_is_removed_before_runner_id_is_reused() {
        use async_ringbuf::traits::AsyncProducer;

        let mut manager = RunnerManager::new(1000, 2);
        let runner_id = manager
            .allocate_runner_with_chunk(0..100)
            .unwrap()
            .runner_id;
        let (mut lifecycle_tx, mut data_tx) = register_test_channels(&mut manager, runner_id);
        push_frame(&mut data_tx, 10).await;
        manager.tick().await;
        lifecycle_tx
            .push(LifecycleEvent::failed(TaskError::Timeout))
            .await
            .unwrap();
        assert!(manager.tick().await.failure.is_some());

        let reused = manager
            .allocate_runner_with_chunk(10..100)
            .unwrap()
            .runner_id;
        assert_eq!(reused, runner_id);
        let (_lifecycle_tx, mut data_tx) = register_test_channels(&mut manager, reused);
        push_frame(&mut data_tx, 5).await;
        manager.tick().await;

        assert_eq!(
            manager.take_meters_snapshot(),
            MeterSnapshot {
                runners: 1,
                bytes: 5
            }
        );
    }

    /// A runner on `range` whose channels the test drives, and its stop.
    fn spawn_stoppable_runner(
        manager: &mut RunnerManager,
        range: Range<u64>,
    ) -> (
        crate::runner::LifecycleSender,
        crate::runner::DataFrameSender,
        CancellationToken,
    ) {
        let stop = CancellationToken::new();
        let mut senders = None;
        manager
            .spawn_runner_with_chunk(range, |_, _limits| {
                let (channels, lifecycle_tx, data_tx) = test_channels();
                senders = Some((lifecycle_tx, data_tx));
                Ok(RunnerChannels {
                    stop: stop.clone(),
                    ..channels
                })
            })
            .unwrap();
        let (lifecycle_tx, data_tx) = senders.unwrap();
        (lifecycle_tx, data_tx, stop)
    }

    /// A runner is stopped alone through its own stop, once, and only
    /// while it runs with bytes left. Its `Stopped` with `Cancelled` is
    /// then a slow attempt, released from the bytes the planner accepted:
    /// a frame still in its channel is dropped. Its slot is freed then,
    /// and the concurrency strategy does not count it.
    #[tokio::test]
    async fn test_a_stopped_runner_is_a_slow_attempt() {
        let mut manager = RunnerManager::new(1000, 4);
        let (mut lifecycle_tx, mut data_tx, stop) = spawn_stoppable_runner(&mut manager, 0..500);
        let unstoppable = manager.allocate_runner_with_chunk(500..1000).unwrap();
        assert!(!manager.stop_runner(unstoppable.runner_id), "no stop");
        assert!(!manager.stop_runner(0), "still connecting");
        assert!(!stop.is_cancelled());

        lifecycle_tx.try_push(LifecycleEvent::Started).unwrap();
        manager.tick().await;
        push_frame(&mut data_tx, 100).await;
        assert_eq!(manager.tick().await.downloaded.unwrap().range, 0..100);
        assert!(manager.stop_runner(0));
        assert!(stop.is_cancelled() && manager.is_stopping(0));
        assert!(!manager.stop_runner(0), "stopped once");
        assert_eq!(manager.allocated_runner_count(), 2);

        push_frame(&mut data_tx, 50).await;
        lifecycle_tx
            .try_push(LifecycleEvent::failed(TaskError::Cancelled))
            .unwrap();
        let failure = manager.tick().await.failure.unwrap();
        assert!(failure.slow && failure.retryable, "{failure:?}");
        assert!(matches!(failure.error, TaskError::Timeout));
        assert_eq!(
            (
                failure.range,
                failure.received,
                failure.attempts,
                failure.charged
            ),
            (100..500, 100, 1, 0)
        );
        assert_eq!(manager.get_total_downloaded(), 100);
        assert_eq!(manager.allocated_runner_count(), 1);
        assert!(!manager.is_stopping(0));
        assert_eq!(manager.runner_outcome_sampler_mut().stats().total, 0);
        // The released ID is a new runner, not a stopping one.
        let (_lifecycle_tx, _data_tx, stop) = spawn_stoppable_runner(&mut manager, 100..500);
        assert!(!manager.is_stopping(0) && !stop.is_cancelled());
    }

    /// A stopping runner whose chunk the planner completed before its
    /// `Stopped` finished it, whatever it reports: no failure, and nothing
    /// is released. The growth does not split a stopping runner.
    #[tokio::test]
    async fn test_a_runner_that_completes_while_stopping_finishes() {
        for finished in [true, false] {
            let reason = || {
                if finished {
                    StoppedReason::Finished
                } else {
                    StoppedReason::Failed(TaskError::Cancelled)
                }
            };
            let mut manager = RunnerManager::new(1000, 4);
            let (mut lifecycle_tx, mut data_tx, _stop) =
                spawn_stoppable_runner(&mut manager, 0..800);
            let _other = manager.allocate_runner_with_chunk(800..1000).unwrap();
            lifecycle_tx.try_push(LifecycleEvent::Started).unwrap();
            manager.tick().await;
            assert_eq!(manager.largest_running_chunk(), Some((0, 0..800)));
            assert!(manager.stop_runner(0));
            assert_eq!(manager.largest_running_chunk(), Some((1, 800..1000)));

            push_frame(&mut data_tx, 800).await;
            assert_eq!(manager.tick().await.downloaded.unwrap().range, 0..800);
            lifecycle_tx
                .try_push(LifecycleEvent::Stopped(reason()))
                .unwrap();
            let tick = manager.tick().await;
            assert!(tick.failure.is_none(), "{:?}: {tick:?}", reason());
            assert_eq!(tick.state, TaskState::Downloading);
            assert_eq!(manager.get_available_ranges(), [], "{:?}", reason());
            assert_eq!(manager.allocated_runner_count(), 1, "{:?}", reason());
            assert_eq!(manager.runner_outcome_sampler_mut().stats().completed, 1);
        }
    }
}
