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
    pin::pin,
};

use bolt_load_utils::telemetry::*;
use bytes::Bytes;
use futures::{FutureExt, StreamExt};
use stream::RunnerTaggedStreamItem;

/// A chunk of data downloaded by a runner, ready to be written to disk.
#[derive(Debug)]
pub(super) struct DownloadedChunk {
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
}

impl RunnerTick {
    fn downloading() -> Self {
        Self {
            state: TaskState::Downloading,
            downloaded: None,
            failure: None,
        }
    }

    fn finished() -> Self {
        Self {
            state: TaskState::Finished,
            downloaded: None,
            failure: None,
        }
    }

    fn with_chunk(chunk: DownloadedChunk) -> Self {
        Self {
            state: TaskState::Downloading,
            downloaded: Some(chunk),
            failure: None,
        }
    }

    fn with_failure(failure: AttemptFailure) -> Self {
        Self {
            state: TaskState::Downloading,
            downloaded: None,
            failure: Some(failure),
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
    chunk_planner::{ChunkState, ChunkStatus, Resize},
    strategy::RunnerOutcomeSampler,
};
use crate::{
    runner::{
        DataFrameReceiver, LifecycleEvent, LifecycleReceiver, LimitReceiver, LimitSender,
        StoppedReason, TaskError, limit_mailbox,
    },
    task::{
        RunnerId,
        instance::{Generator, concurrent_task::strategy::RunnerFailureKind},
    },
};

mod data;
mod lifecycle;
mod stream;

pub use data::{DataAggregator, DataStreamEvent};
pub use lifecycle::{LifecycleAggregator, LifecycleStreamEvent};

/// The receiving ends of a spawned runner.
pub(super) struct RunnerChannels {
    pub lifecycle_rx: LifecycleReceiver,
    pub data_rx: DataFrameReceiver,
}

/// A runner that could not be spawned.
#[derive(Debug)]
pub(super) struct SpawnFailed {
    pub message: String,
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

/// Runner registration information
pub struct RunnerRegistration {
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

    /// Allocates a new runner ID and the mailbox for its limits
    ///
    /// # Returns
    /// * `Some(RunnerRegistration)` - If allocation is successful
    /// * `None` - If maximum concurrency has been reached
    pub fn allocate_runner(&mut self) -> Option<RunnerRegistration> {
        let runner_id = self.id_generator.next()?;
        let (sender, limits) = limit_mailbox();
        self.limit_senders.insert(runner_id, sender);
        Some(RunnerRegistration { runner_id, limits })
    }

    /// Releases a runner ID and its associated resources
    ///
    /// This will:
    /// - Close and remove the limit mailbox
    /// - Remove from RunnerNotification (legacy) and aggregators (new)
    /// - Recycle the runner ID
    pub fn release_runner(&mut self, runner_id: RunnerId) {
        // The runner sees its limits closed once the sender is dropped
        self.limit_senders.remove(&runner_id);
        self.lifecycle_aggregator.remove(runner_id);
        self.connecting_data.remove(&runner_id);
        self.data_aggregator.unregister(runner_id);
        self.id_generator.release(runner_id);
    }

    /// Gets the number of currently allocated runners
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
    ) -> Result<Range<u64>, super::chunk_planner::Error> {
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
    #[inline]
    pub fn get_available_ranges(&self) -> Vec<Range<u64>> {
        self.chunk_planner.get_available_ranges()
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

    /// Finds a chunk that can be split
    #[inline]
    pub fn find_chunk_to_split(
        &self,
        required_size: u64,
    ) -> Option<(Option<RunnerId>, Range<u64>)> {
        self.chunk_planner.find_chunk_to_split(required_size)
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
    pub fn allocate_runner_with_chunk(&mut self, range: Range<u64>) -> Option<RunnerRegistration> {
        let registration = self.allocate_runner()?;
        if !self.allocate_chunk(range, registration.runner_id) {
            self.release_runner(registration.runner_id);
            return None;
        }
        self.chunk_planner
            .mark_running(registration.runner_id)
            .expect("the chunk was just allocated");
        Some(registration)
    }

    /// Allocate a runner for `range` and spawn it with `spawn`.
    ///
    /// The chunk stays pending until the runner reports `Started`. A rejected
    /// spawn is reported by the next tick, once.
    ///
    /// # Returns
    /// * `None` - If unable to allocate a runner ID or the range
    pub fn spawn_runner_with_chunk(
        &mut self,
        range: Range<u64>,
        spawn: impl FnOnce(RunnerId, LimitReceiver) -> Result<RunnerChannels, SpawnFailed>,
    ) -> Option<()> {
        let registration = self.allocate_runner()?;
        let runner_id = registration.runner_id;
        if !self.chunk_planner.allocate_pending_chunk(range, runner_id) {
            self.release_runner(runner_id);
            return None;
        }
        match spawn(runner_id, registration.limits) {
            Ok(channels) => {
                self.register_runner_channels(runner_id, channels.lifecycle_rx, channels.data_rx)
            }
            Err(failed) => self.spawn_failures.push_back((runner_id, failed)),
        }
        Some(())
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
            Some(self.mark_failed(runner_id)?)
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
        self.chunk_planner
            .get_runner_state(runner_id)
            .is_some_and(|state| state.status == ChunkStatus::Pending)
    }

    fn handle_lifecycle(&mut self, event: RunnerTaggedStreamItem<LifecycleEvent>) -> RunnerTick {
        let runner_id = event.runner_id;
        let connecting = self.is_connecting(runner_id);
        let reason = match event.item {
            LifecycleEvent::Started => {
                trace!("runner {runner_id} started");
                debug_assert!(connecting, "runner {runner_id} started twice");
                self.chunk_planner
                    .mark_running(runner_id)
                    .expect("chunk planner should not fail");
                let data_rx = self
                    .connecting_data
                    .remove(&runner_id)
                    .expect("a connecting runner has a data receiver");
                self.data_aggregator.register(runner_id, data_rx);
                return RunnerTick::downloading();
            }
            LifecycleEvent::Stopped(reason) => reason,
        };
        self.meters.remove(&runner_id);
        // Remove control channel when runner stops
        self.release_runner(runner_id);
        if connecting {
            let error = match reason {
                StoppedReason::Failed(error) => error,
                StoppedReason::Finished => {
                    debug_assert!(false, "runner {runner_id} finished before starting");
                    TaskError::ChannelClosed
                }
            };
            return self.handle_connect_failed(runner_id, error);
        }
        let kind = match reason {
            StoppedReason::Finished | StoppedReason::Failed(TaskError::ExceededTotalSize) => {
                trace!("runner {runner_id} finished");
                return if self.mark_runner_completed(runner_id) {
                    RunnerTick::finished()
                } else {
                    RunnerTick::downloading()
                };
            }
            StoppedReason::Failed(kind) => kind,
        };
        error!("runner {runner_id} failed: {kind:?}");
        let unfinished_range = self
            .chunk_planner
            .mark_failed(runner_id)
            .expect("chunk planner should not fail");
        trace!("runner {runner_id} failed, released range: {unfinished_range:?}");
        if kind.is_cancelled() {
            return RunnerTick::downloading();
        }
        self.runner_outcome_sampler
            .record_stream_closed(if kind.is_retryable() {
                RunnerFailureKind::Retryable
            } else {
                RunnerFailureKind::Unretryable
            });
        RunnerTick::with_failure(AttemptFailure {
            runner_id,
            retryable: is_attempt_retryable(&kind),
            error: kind,
        })
    }

    /// Handle a runner that stopped before it connected; its range is
    /// released and it never counts as finished.
    fn handle_connect_failed(&mut self, runner_id: RunnerId, error: TaskError) -> RunnerTick {
        self.chunk_planner
            .mark_failed(runner_id)
            .expect("chunk planner should not fail");
        if error.is_cancelled() {
            trace!("runner {runner_id} cancelled while connecting");
            return RunnerTick::downloading();
        }
        error!("runner {runner_id} failed to connect: {error:?}");
        let retryable = is_attempt_retryable(&error);
        self.runner_outcome_sampler
            .record_connect_failed(if retryable {
                RunnerFailureKind::Retryable
            } else {
                RunnerFailureKind::Unretryable
            });
        RunnerTick::with_failure(AttemptFailure {
            runner_id,
            error,
            retryable,
        })
    }

    /// Handle a runner whose spawn was rejected.
    #[cold]
    fn handle_spawn_failed(&mut self, runner_id: RunnerId, failed: SpawnFailed) -> RunnerTick {
        error!("failed to spawn runner {runner_id}: {}", failed.message);
        self.release_runner(runner_id);
        self.handle_connect_failed(
            runner_id,
            TaskError::Other {
                message: failed.message,
            },
        )
    }

    /// Handle a runner whose stream closed without sending `Stopped`.
    fn handle_runner_lost(&mut self, runner_id: RunnerId) -> RunnerTick {
        warn!("runner {runner_id} lifecycle stream closed unexpectedly");
        let connecting = self.is_connecting(runner_id);
        self.meters.remove(&runner_id);
        let unfinished_range = self
            .chunk_planner
            .mark_failed(runner_id)
            .expect("chunk planner should not fail");
        if connecting {
            self.runner_outcome_sampler
                .record_connect_failed(RunnerFailureKind::Retryable);
        } else {
            self.runner_outcome_sampler
                .record_stream_closed(RunnerFailureKind::Retryable);
        }
        self.release_runner(runner_id);
        trace!("runner {runner_id} lost, released range: {unfinished_range:?}");
        if self.chunk_planner.is_complete() {
            return RunnerTick::finished();
        }
        RunnerTick::with_failure(AttemptFailure {
            runner_id,
            error: TaskError::ChannelClosed,
            retryable: true,
        })
    }

    fn handle_lifecycle_stream_event(&mut self, event: Option<LifecycleStreamEvent>) -> RunnerTick {
        match event {
            Some(LifecycleStreamEvent::Event(tagged)) => self.handle_lifecycle(tagged),
            Some(LifecycleStreamEvent::Closed(runner_id)) => self.handle_runner_lost(runner_id),
            None => RunnerTick::downloading(),
        }
    }

    /// The next rejected spawn, which the lifecycle reports before anything else.
    fn take_spawn_failure(&mut self) -> Option<RunnerTick> {
        let (runner_id, failed) = self.spawn_failures.pop_front()?;
        Some(self.handle_spawn_failed(runner_id, failed))
    }

    /// Handle data frames from runners, returning the chunk ready for writing.
    fn handle_data_frame(&mut self, runner_id: RunnerId, mut data_frame: Bytes) -> DownloadedChunk {
        let bytes_len = data_frame.len();
        *self.meters.entry(runner_id).or_insert(0) += bytes_len;

        let fixed_downloaded_range = self
            .chunk_planner
            .update_progress(runner_id, bytes_len as u64)
            .expect("chunk planner should not fail");
        let picked_size = fixed_downloaded_range.end - fixed_downloaded_range.start;
        data_frame.truncate(picked_size as usize);

        DownloadedChunk {
            range: fixed_downloaded_range,
            bytes: data_frame,
        }
    }

    pub async fn tick(&mut self) -> RunnerTick {
        if let Some(tick) = self.take_spawn_failure() {
            return tick;
        }
        let mut next_lifecycle = pin!(self.lifecycle_aggregator.next().fuse());
        let mut next_data = pin!(self.data_aggregator.next().fuse());
        // Priority: lifecycle > data (lifecycle is control-plane, must not be starved)
        futures::select_biased! {
            event = next_lifecycle => self.handle_lifecycle_stream_event(event),
            data = next_data => {
                if let Some(event) = data {
                    match event {
                        DataStreamEvent::Data(item) => {
                            let chunk = self.handle_data_frame(item.runner_id, item.item.data);
                            return RunnerTick::with_chunk(chunk);
                        }
                        DataStreamEvent::Closed(runner_id) => {
                            trace!("runner {runner_id} data stream closed");
                            self.data_aggregator.unregister(runner_id);
                        }
                    }
                }
                RunnerTick::downloading()
            }
        }
    }

    pub async fn tick_lifecycle_only(&mut self) -> RunnerTick {
        if let Some(tick) = self.take_spawn_failure() {
            return tick;
        }
        let event = self.lifecycle_aggregator.next().await;
        self.handle_lifecycle_stream_event(event)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use async_ringbuf::traits::Producer;

    use super::*;
    use crate::runner::{DATA_FRAME_CHANNEL_CAPACITY, LIFECYCLE_CHANNEL_CAPACITY};

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
        let reg1 = manager.allocate_runner().unwrap();
        assert_eq!(reg1.runner_id, 0);
        assert_eq!(manager.allocated_runner_count(), 1);

        // Allocate the second runner
        let reg2 = manager.allocate_runner().unwrap();
        assert_eq!(reg2.runner_id, 1);
        assert_eq!(manager.allocated_runner_count(), 2);
        assert!(manager.is_full());

        // Cannot allocate more
        assert!(manager.allocate_runner().is_none());

        // Release the first runner
        manager.release_runner(reg1.runner_id);
        assert_eq!(manager.allocated_runner_count(), 1);
        assert!(!manager.is_full());

        // Can allocate again
        let reg3 = manager.allocate_runner().unwrap();
        assert_eq!(reg3.runner_id, 0); // Reuse released ID
    }

    #[test]
    fn test_limit_runner() {
        let mut manager = RunnerManager::new(1000, 2);
        let mut reg = manager.allocate_runner().unwrap();

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
                    source: UnretryableError::ServiceUnavailable {
                        status: "429".into(),
                        description: "Too Many Requests".into(),
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
}
