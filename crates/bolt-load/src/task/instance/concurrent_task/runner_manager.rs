//! Runner Manager - Unified management of runner lifecycle and communication
//!
//! This module provides a unified interface to manage:
//! - Runner ID allocation and recycling (IdGenerator)
//! - Chunk allocation and progress tracking (ChunkPlanner)
//! - Aggregated reception of runner messages (RunnerNotification)
//! - Channels for sending control messages to specific runners

use std::{collections::HashMap, ops::Range};

use async_channel::{Receiver, Sender, TrySendError};
use bolt_load_utils::telemetry::*;
use bytes::Bytes;
use futures::{FutureExt, StreamExt};
use pending::{PendingRunnerContext, PendingRunnerError, PendingRunnerGroup, PendingRunnerOutput};
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
    chunk_planner::{ChunkState, Resize},
    strategy::RunnerOutcomeSampler,
};
use crate::{
    runner::{DataFrameReceiver, LifecycleEvent, LifecycleReceiver, StoppedReason, TaskError},
    task::{
        ControlEvent, RunnerId,
        instance::{Generator, concurrent_task::strategy::RunnerFailureKind},
    },
};

mod data;
mod lifecycle;
pub mod pending;
mod stream;

pub use data::{DataAggregator, DataStreamEvent};
pub use lifecycle::{LifecycleAggregator, LifecycleStreamEvent};
pub use pending::PendingRunnerReceiver;

/// Default capacity for control channels
pub(super) const DEFAULT_CONTROL_CHANNEL_CAPACITY: usize = 8;

/// Type alias for control message sender
pub type ControlSender = Sender<ControlEvent>;

/// Type alias for control message receiver
pub type ControlReceiver = Receiver<ControlEvent>;

/// Errors that may occur when sending messages
#[derive(Debug, snafu::Snafu)]
pub enum SendError {
    #[snafu(display("Runner {runner_id} not found"))]
    RunnerNotFound { runner_id: RunnerId },
    #[snafu(display("Control channel is full"))]
    ChannelFull,
    #[snafu(display("Control channel is closed"))]
    ChannelClosed,
}

trait ManagerControlMapExt {
    fn send_message(&self, runner_id: RunnerId, message: ControlEvent) -> Result<(), SendError>;
}

impl ManagerControlMapExt for HashMap<RunnerId, ControlSender> {
    fn send_message(&self, runner_id: RunnerId, message: ControlEvent) -> Result<(), SendError> {
        if let Some(tx) = self.get(&runner_id) {
            tx.try_send(message)?;
            Ok(())
        } else {
            Err(SendError::RunnerNotFound { runner_id })
        }
    }
}

/// Errors that may occur when resizing a runner's chunk.
#[derive(Debug, snafu::Snafu)]
pub enum ResizeError {
    #[snafu(transparent)]
    Planner { source: super::chunk_planner::Error },
    #[snafu(display("Failed to limit runner {runner_id}: {source}"))]
    Limit {
        runner_id: RunnerId,
        source: SendError,
    },
}

impl<T> From<TrySendError<T>> for SendError {
    fn from(err: TrySendError<T>) -> Self {
        match err {
            TrySendError::Full(_) => SendError::ChannelFull,
            TrySendError::Closed(_) => SendError::ChannelClosed,
        }
    }
}

/// Runner registration information
pub struct RunnerRegistration {
    /// Runner ID
    pub runner_id: RunnerId,
    /// Control message receiver
    pub control_rx: ControlReceiver,
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

    /// Control channel mapping for sending messages to specific runners
    control_channels: HashMap<RunnerId, ControlSender>,
    /// Pending runners for creating runners
    pending_runners: PendingRunnerGroup,
    /// Sampler for tracking the outcome of the runners
    runner_outcome_sampler: RunnerOutcomeSampler,
    /// Lifecycle aggregator for runners' lifecycle events
    lifecycle_aggregator: LifecycleAggregator,
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
            control_channels: HashMap::with_capacity(max_concurrency),
            pending_runners: PendingRunnerGroup::new(),
            runner_outcome_sampler: RunnerOutcomeSampler::with_defaults(),
            lifecycle_aggregator: LifecycleAggregator::with_capacity(max_concurrency),
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

    /// Allocates a new runner ID and corresponding control channel
    ///
    /// # Returns
    /// * `Some(RunnerRegistration)` - If allocation is successful
    /// * `None` - If maximum concurrency has been reached
    pub fn allocate_runner(&mut self) -> Option<RunnerRegistration> {
        let runner_id = self.id_generator.next()?;
        let (tx, rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        self.control_channels.insert(runner_id, tx);
        Some(RunnerRegistration {
            runner_id,
            control_rx: rx,
        })
    }

    /// Releases a runner ID and its associated resources
    ///
    /// This will:
    /// - Close and remove the control channel
    /// - Remove from RunnerNotification (legacy) and aggregators (new)
    /// - Recycle the runner ID
    pub fn release_runner(&mut self, runner_id: RunnerId) {
        // Close control channel (receiver will get closed signal after sender is dropped)
        self.control_channels.remove(&runner_id);
        self.lifecycle_aggregator.remove(runner_id);
        self.data_aggregator.unregister(runner_id);
        self.id_generator.release(runner_id);
    }

    /// Gets the number of currently allocated runners
    #[inline]
    pub fn allocated_runner_count(&self) -> usize {
        self.id_generator.allocated_count()
    }

    /// Checks if maximum concurrency has been reached
    #[inline]
    pub fn is_full(&self) -> bool {
        self.id_generator.is_full()
    }

    /// Checks if there are no runners
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.id_generator.is_empty()
    }

    // ==================== Control Channel Related Methods ====================

    /// Sends a control message to the specified runner
    ///
    /// # Arguments
    /// * `runner_id` - ID of the target runner
    /// * `message` - Message to send
    ///
    /// # Returns
    /// * `Ok(())` - If the message is sent successfully
    /// * `Err(SendError)` - If sending fails
    pub fn send_message(
        &self,
        runner_id: RunnerId,
        message: ControlEvent,
    ) -> Result<(), SendError> {
        self.control_channels.send_message(runner_id, message)
    }

    /// Control messages waiting for the runner to receive them.
    #[cfg(test)]
    pub(super) fn queued_controls(&self, runner_id: RunnerId) -> usize {
        self.control_channels.get(&runner_id).map_or(0, Sender::len)
    }

    // ==================== Runner Notification Related Methods ====================

    /// Registers a lifecycle receiver for a runner
    #[expect(
        dead_code,
        reason = "registration without a pending runner is not used yet"
    )]
    fn register_lifecycle(&mut self, runner_id: RunnerId, receiver: LifecycleReceiver) {
        self.lifecycle_aggregator.add(runner_id, receiver);
    }

    /// Registers a data receiver for a runner
    /// Called when the runner is successfully created
    #[expect(
        dead_code,
        reason = "registration without a pending runner is not used yet"
    )]
    fn register_data(&mut self, runner_id: RunnerId, receiver: DataFrameReceiver) {
        self.data_aggregator.register(runner_id, receiver);
    }

    /// Registers both lifecycle and data receivers for a runner.
    pub fn register_runner_channels(
        &mut self,
        runner_id: RunnerId,
        lifecycle_rx: LifecycleReceiver,
        data_rx: DataFrameReceiver,
    ) {
        self.lifecycle_aggregator.add(runner_id, lifecycle_rx);
        self.data_aggregator.register(runner_id, data_rx);
    }

    /// Returns a reference to the data aggregator for consuming data
    pub fn data_aggregator(&mut self) -> &mut DataAggregator {
        &mut self.data_aggregator
    }

    /// Gets the total file size
    #[inline]
    pub fn total(&self) -> u64 {
        self.chunk_planner.total
    }

    /// Allocates a chunk to the specified runner
    #[inline]
    pub fn allocate_chunk(&mut self, range: Range<u64>, runner_id: RunnerId) -> bool {
        self.chunk_planner.allocate_chunk(range, Some(runner_id))
    }

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

    /// Marks a runner as finished
    #[inline]
    pub fn mark_finished(
        &mut self,
        runner_id: RunnerId,
    ) -> Result<(), super::chunk_planner::Error> {
        self.chunk_planner.mark_finished(runner_id)
    }

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

    /// Allocates a new runner and assigns a chunk
    ///
    /// This is a convenience method that combines `allocate_runner` and `allocate_chunk`
    ///
    /// # Arguments
    /// * `range` - The chunk range to allocate
    ///
    /// # Returns
    /// * `Some((RunnerRegistration, bool))` - Runner registration info and whether chunk was successfully allocated
    /// * `None` - If unable to allocate a runner ID
    pub fn allocate_runner_with_chunk(&mut self, range: Range<u64>) -> Option<RunnerRegistration> {
        let registration = self.allocate_runner()?;
        if !self.allocate_chunk(range, registration.runner_id) {
            self.release_runner(registration.runner_id);
            return None;
        }
        Some(registration)
    }

    /// Allocates a pending runner with chunk using legacy architecture
    pub fn allocate_pending_runner_with_chunk(
        &mut self,
        range: Range<u64>,
        f: impl FnOnce(RunnerId, ControlReceiver) -> PendingRunnerReceiver,
    ) -> Option<()> {
        let registration = self.allocate_runner()?;
        if !self
            .chunk_planner
            .allocate_pending_chunk(range.clone(), registration.runner_id)
        {
            self.release_runner(registration.runner_id);
            return None;
        }
        let rx = f(registration.runner_id, registration.control_rx);
        let context = PendingRunnerContext::new(registration.runner_id, range);
        self.pending_runners.insert(context, rx);
        Some(())
    }

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
    /// cannot be sent the limit.
    #[inline]
    pub fn resize_runner(&mut self, runner_id: RunnerId, new_size: u64) -> Result<(), ResizeError> {
        let Self {
            chunk_planner,
            control_channels,
            ..
        } = self;
        let mut guard = PlannerGuard::new(chunk_planner);
        let limit = match guard.planner().resize_runner_state(runner_id, new_size)? {
            Resize::Unchanged => return Ok(()),
            Resize::Shrunk { limit } => limit,
        };

        // Limit the total size of the runner
        control_channels
            .send_message(runner_id, ControlEvent::LimitTotal(limit))
            .map_err(|source| ResizeError::Limit { runner_id, source })?;

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

    fn handle_lifecycle(&mut self, event: RunnerTaggedStreamItem<LifecycleEvent>) -> RunnerTick {
        let runner_id = event.runner_id;
        let reason = match event.item {
            LifecycleEvent::Started => {
                trace!("runner {runner_id} started");
                return RunnerTick::downloading();
            }
            LifecycleEvent::Stopped(reason) => reason,
        };
        self.meters.remove(&runner_id);
        // Remove control channel when runner stops
        self.release_runner(runner_id);
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

    /// Handle pending runners
    fn handle_pending(&mut self, pending: PendingRunnerOutput) -> RunnerTick {
        let runner_id = pending.context.runner_id;
        let error = match pending.result {
            Ok(output) => {
                self.chunk_planner
                    .mark_running(runner_id)
                    .expect("chunk planner should not fail");
                self.register_runner_channels(runner_id, output.lifecycle_rx, output.data_rx);
                return RunnerTick::downloading();
            }
            Err(error) => error,
        };
        error!("failed to create runner: {error:?}");
        // Update planner state first for stronger atomicity
        self.chunk_planner
            .mark_failed(runner_id)
            .expect("chunk planner should not fail");
        // Release runner slot after planner state transition
        // Note: release_runner() is no-op for unregistered runners (safe)
        self.release_runner(runner_id);
        let (error, retryable) = match error {
            PendingRunnerError::ReceiverClosed => (TaskError::ChannelClosed, true),
            PendingRunnerError::Connection { source } => {
                let retryable = source.is_retryable();
                (
                    TaskError::StreamError {
                        source: source.source,
                    },
                    retryable,
                )
            }
            PendingRunnerError::Spawn { message } => (TaskError::Other { message }, false),
        };
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

    /// Handle a runner whose stream closed without sending `Stopped`.
    fn handle_runner_lost(&mut self, runner_id: RunnerId) -> RunnerTick {
        warn!("runner {runner_id} lifecycle stream closed unexpectedly");
        self.meters.remove(&runner_id);
        let unfinished_range = self
            .chunk_planner
            .mark_failed(runner_id)
            .expect("chunk planner should not fail");
        self.runner_outcome_sampler
            .record_stream_closed(RunnerFailureKind::Retryable);
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

    fn handle_pending_stream_event(&mut self, pending: Option<PendingRunnerOutput>) -> RunnerTick {
        match pending {
            Some(pending) => self.handle_pending(pending),
            None if self.chunk_planner.is_complete() => RunnerTick::finished(),
            None => RunnerTick::downloading(),
        }
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

    #[expect(
        private_interfaces,
        reason = "`RunnerTick` is internal to the concurrent task"
    )]
    pub async fn tick(&mut self) -> RunnerTick {
        let next_lifecycle = self.lifecycle_aggregator.next().fuse();
        let next_data = self.data_aggregator.next().fuse();
        let pending = self.pending_runners.next().fuse();
        futures::pin_mut!(next_lifecycle, next_data, pending);
        // Priority: lifecycle > pending > data (lifecycle is control-plane, must not be starved)
        futures::select_biased! {
            event = next_lifecycle => self.handle_lifecycle_stream_event(event),
            pending = pending => self.handle_pending_stream_event(pending),
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

    #[expect(
        private_interfaces,
        reason = "`RunnerTick` is internal to the concurrent task"
    )]
    pub async fn tick_lifecycle_only(&mut self) -> RunnerTick {
        let next_lifecycle = self.lifecycle_aggregator.next().fuse();
        let pending = self.pending_runners.next().fuse();
        futures::pin_mut!(next_lifecycle, pending);
        futures::select_biased! {
            event = next_lifecycle => self.handle_lifecycle_stream_event(event),
            pending = pending => self.handle_pending_stream_event(pending),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{super::chunk_planner::ChunkStatus, *};
    use crate::runner::{ConnectionError, DATA_FRAME_CHANNEL_CAPACITY, LIFECYCLE_CHANNEL_CAPACITY};

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
    fn test_send_message() {
        let mut manager = RunnerManager::new(1000, 2);

        // Allocate runner
        let reg = manager.allocate_runner().unwrap();

        // Send message
        let result = manager.send_message(reg.runner_id, ControlEvent::LimitTotal(500));
        assert!(result.is_ok());

        // Receive message
        let msg = reg.control_rx.try_recv().unwrap();
        assert!(matches!(msg, ControlEvent::LimitTotal(500)));

        // Send message to non-existent runner
        let result = manager.send_message(999, ControlEvent::LimitTotal(500));
        assert!(matches!(
            result,
            Err(SendError::RunnerNotFound { runner_id: 999 })
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
        let reg = manager.allocate_runner_with_chunk(0..500).unwrap();
        manager.update_progress(reg.runner_id, 100).unwrap();

        manager.resize_runner(reg.runner_id, 150).unwrap();
        assert_eq!(
            manager.get_runner_state(reg.runner_id).unwrap().allocated,
            0..250
        );
        assert!(matches!(
            reg.control_rx.try_recv(),
            Ok(ControlEvent::LimitTotal(250))
        ));
    }

    #[test]
    fn test_resize_runner_rejected_or_unchanged_sends_nothing() {
        let mut manager = RunnerManager::new(1000, 2);
        let reg = manager.allocate_runner_with_chunk(0..500).unwrap();
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
        assert!(reg.control_rx.try_recv().is_err());
    }

    #[test]
    fn test_resize_runner_rolls_back_when_the_limit_cannot_be_sent() {
        let mut manager = RunnerManager::new(1000, 2);
        let reg = manager.allocate_runner_with_chunk(0..512).unwrap();
        for _ in 0..DEFAULT_CONTROL_CHANNEL_CAPACITY {
            let incomplete = manager.get_runner_state(0).unwrap().incomplete_range();
            manager.resize_runner(0, incomplete.end / 2).unwrap();
        }
        let full = manager.get_runner_state(0).unwrap().allocated.clone();
        assert!(matches!(
            manager.resize_runner(0, full.end / 2),
            Err(ResizeError::Limit {
                runner_id: 0,
                source: SendError::ChannelFull
            })
        ));
        assert_eq!(manager.get_runner_state(0).unwrap().allocated, full);

        drop(reg.control_rx);
        assert!(matches!(
            manager.resize_runner(0, full.end / 2),
            Err(ResizeError::Limit {
                runner_id: 0,
                source: SendError::ChannelClosed
            })
        ));
        assert_eq!(manager.get_runner_state(0).unwrap().allocated, full);
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

    #[test]
    fn test_allocate_pending_runner_with_chunk() {
        use pending::{PendingRunnerError, RunnerBuilderOutput};

        let mut manager = RunnerManager::new(1000, 2);

        // Allocate pending runner with chunk
        let result =
            manager.allocate_pending_runner_with_chunk(0..500, |runner_id, _control_rx| {
                assert_eq!(runner_id, 0);
                // Return a receiver that will resolve later
                let (_, rx) = oneshot::channel::<Result<RunnerBuilderOutput, PendingRunnerError>>();
                rx
            });

        assert!(result.is_some());
        assert_eq!(manager.allocated_runner_count(), 1);

        // Verify chunk is allocated with pending status
        let state = manager.get_runner_state(0).unwrap();
        assert_eq!(state.allocated, 0..500);
        assert_eq!(state.status, ChunkStatus::Pending);
    }

    #[test]
    fn test_multiple_pending_runners() {
        use pending::{PendingRunnerError, RunnerBuilderOutput};

        let mut manager = RunnerManager::new(1000, 4);

        // Allocate multiple pending runners
        for i in 0..4 {
            let start = (i * 250) as u64;
            let end = ((i + 1) * 250) as u64;
            let result = manager.allocate_pending_runner_with_chunk(start..end, |_runner_id, _| {
                let (_, rx) = oneshot::channel::<Result<RunnerBuilderOutput, PendingRunnerError>>();
                rx
            });
            assert!(result.is_some());
        }

        assert_eq!(manager.allocated_runner_count(), 4);
        assert!(manager.is_full());

        // Cannot allocate more
        let result = manager.allocate_pending_runner_with_chunk(0..100, |_, _| {
            let (_, rx) = oneshot::channel();
            rx
        });
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_pending_to_running_via_tick() {
        use async_ringbuf::{AsyncHeapRb, traits::Split};

        use crate::runner::{DataFrame, LifecycleEvent};

        let mut manager = RunnerManager::new(1000, 2);

        // Create a channel to send the builder output
        let (tx, rx) = oneshot::channel();

        // Allocate pending runner
        manager.allocate_pending_runner_with_chunk(0..500, |runner_id, _control_rx| {
            assert_eq!(runner_id, 0);
            rx
        });

        // Verify initial state is pending
        let state = manager.get_runner_state(0).unwrap();
        assert_eq!(state.status, ChunkStatus::Pending);

        // Create mock channels for the new architecture
        let data_rb = AsyncHeapRb::<DataFrame>::new(DATA_FRAME_CHANNEL_CAPACITY);
        let (_data_prod, data_cons) = data_rb.split();

        let lifecycle_rb = AsyncHeapRb::<LifecycleEvent>::new(LIFECYCLE_CHANNEL_CAPACITY);
        let (_lifecycle_prod, lifecycle_cons) = lifecycle_rb.split();

        // Send the channels through the pending receiver
        tx.send(Ok(pending::RunnerBuilderOutput {
            data_rx: data_cons,
            lifecycle_rx: lifecycle_cons,
        }))
        .unwrap();

        // Tick to process the pending runner
        let tick = manager.tick().await;

        assert_eq!(tick.state, TaskState::Downloading);
        assert!(tick.downloaded.is_none());

        // Verify state is now running
        let chunk_state = manager.get_runner_state(0).unwrap();
        assert_eq!(chunk_state.status, ChunkStatus::Running);
    }

    #[tokio::test]
    async fn test_pending_connection_failure_via_tick() {
        use bolt_load_core::adapter::AdapterError;

        use crate::adapter::UnretryableError;

        let mut manager = RunnerManager::new(1000, 2);

        // Create a channel to send the error
        let (tx, rx) = oneshot::channel();

        // Allocate pending runner
        manager.allocate_pending_runner_with_chunk(0..500, |runner_id, _control_rx| {
            assert_eq!(runner_id, 0);
            rx
        });

        // Verify initial state is pending
        let state = manager.get_runner_state(0).unwrap();
        assert_eq!(state.status, ChunkStatus::Pending);

        // Send connection error
        tx.send(Err(pending::PendingRunnerError::Connection {
            source: ConnectionError {
                source: AdapterError::Unretryable {
                    source: UnretryableError::Whatever {
                        message: "test connection error".to_string(),
                        source: None,
                    },
                },
            },
        }))
        .unwrap();

        // Tick to process the pending runner
        let tick = manager.tick().await;

        assert_eq!(tick.state, TaskState::Downloading);
        assert!(tick.downloaded.is_none());

        // Verify runner state is removed (failed)
        assert!(manager.get_runner_state(0).is_none());
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
        let registration = manager.allocate_runner_with_chunk(0..64).unwrap();
        let runner_id = registration.runner_id;
        let cancel_token = CancellationToken::new();
        let test_stream = stream! {
            yield Ok(Bytes::from(vec![7; 64]));
        };

        let (mut runner, lifecycle_rx, data_rx) = TaskRunner::builder()
            .total(64)
            .stream(Box::pin(test_stream))
            .runner_id(runner_id)
            .control_signal(registration.control_rx)
            .cancel_token(cancel_token)
            .data_drain_timeout(Duration::from_millis(50))
            .build()
            .unwrap();

        manager.register_runner_channels(runner_id, lifecycle_rx, data_rx);

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
