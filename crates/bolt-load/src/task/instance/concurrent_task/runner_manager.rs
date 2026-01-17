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
use pending::{
    PendingRunnerContext, PendingRunnerGroup, PendingRunnerOutput, PendingRunnerReceiver,
};

use super::{ChunkPlanner, PlannerGuard, chunk_planner::ChunkState};
use crate::{
    runner::{
        RunnerMessage, RunnerMessageConsumer, RunnerMessageKind, StoppedReason, TaskFailedKind,
    },
    task::{ControlEvent, RunnerId, instance::Generator},
};

mod notification;
mod pending;

pub use notification::RunnerNotification;

/// Default capacity for control channels
const DEFAULT_CONTROL_CHANNEL_CAPACITY: usize = 8;

/// Type alias for control message sender
pub type ControlSender = Sender<ControlEvent>;

/// Type alias for control message receiver
pub type ControlReceiver = Receiver<ControlEvent>;

/// Errors that may occur when sending messages
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("Runner {0} not found")]
    RunnerNotFound(RunnerId),
    #[error("Control channel is full")]
    ChannelFull,
    #[error("Control channel is closed")]
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
            Err(SendError::RunnerNotFound(runner_id))
        }
    }
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
    /// Runner notification aggregator for receiving messages from runners
    runner_notification: RunnerNotification,
    /// Control channel mapping for sending messages to specific runners
    control_channels: HashMap<RunnerId, ControlSender>,
    /// Pending runners for creating runners
    pending_runners: PendingRunnerGroup,
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
            runner_notification: RunnerNotification::with_capacity(max_concurrency),
            control_channels: HashMap::with_capacity(max_concurrency),
            pending_runners: PendingRunnerGroup::new(),
        }
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
    /// - Remove from RunnerNotification
    /// - Recycle the runner ID
    pub fn release_runner(&mut self, runner_id: RunnerId) {
        // Close control channel (receiver will get closed signal after sender is dropped)
        self.control_channels.remove(&runner_id);
        self.runner_notification.remove(runner_id);
        self.id_generator.release(runner_id);
    }

    /// Checks if the specified ID is allocated
    #[inline]
    pub fn is_runner_allocated(&self, runner_id: RunnerId) -> bool {
        self.id_generator.is_allocated(runner_id)
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

    /// Gets the control channel sender for the specified runner (cloned)
    ///
    /// This method is used for scenarios that need to hold a sender reference
    pub fn get_control_sender(&self, runner_id: RunnerId) -> Option<ControlSender> {
        self.control_channels.get(&runner_id).cloned()
    }

    // ==================== Runner Notification Related Methods ====================

    /// Registers a message consumer for a runner
    pub fn register_notification(&mut self, runner_id: RunnerId, consumer: RunnerMessageConsumer) {
        self.runner_notification.add(runner_id, consumer);
    }

    /// Gets a mutable reference to RunnerNotification
    ///
    /// Used for polling messages in the event loop
    #[inline]
    pub fn notification_mut(&mut self) -> &mut RunnerNotification {
        &mut self.runner_notification
    }

    /// Closes RunnerNotification
    #[inline]
    pub fn close_notification(&mut self) {
        self.runner_notification.close();
    }

    /// Checks if RunnerNotification is closed
    #[inline]
    pub fn is_notification_closed(&self) -> bool {
        self.runner_notification.is_closed()
    }

    // ==================== Chunk Planner Related Methods ====================

    /// Gets an immutable reference to ChunkPlanner
    #[inline]
    pub fn chunk_planner(&self) -> &ChunkPlanner {
        &self.chunk_planner
    }

    /// Gets a mutable reference to ChunkPlanner
    #[inline]
    pub fn chunk_planner_mut(&mut self) -> &mut ChunkPlanner {
        &mut self.chunk_planner
    }

    /// Creates a PlannerGuard for transactional operations
    #[inline]
    pub fn planner_guard(&mut self) -> PlannerGuard<'_> {
        PlannerGuard::new(&mut self.chunk_planner)
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

    /// Checks if the download is complete
    #[inline]
    pub fn is_complete(&self) -> bool {
        self.chunk_planner.is_complete()
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

    /// Attempts to arrange a chunk by length
    #[inline]
    pub fn try_arrange_chunk_by_length(&self, length: u64) -> Option<Range<u64>> {
        self.chunk_planner.try_arrange_chunk_by_length(length)
    }

    /// Gets incomplete states
    #[inline]
    pub fn get_incomplete_states(
        &self,
        min_chunk_size: Option<u64>,
    ) -> Vec<(RunnerId, Range<u64>)> {
        self.chunk_planner.get_incomplete_states(min_chunk_size)
    }

    /// Splits a chunk
    #[inline]
    pub fn split_chunk(
        &mut self,
        runner_id: RunnerId,
        split_pos: u64,
        new_runner_id: RunnerId,
    ) -> Result<Range<u64>, super::chunk_planner::Error> {
        self.chunk_planner
            .split_chunk(runner_id, split_pos, new_runner_id)
    }

    /// Resizes the runner state
    #[inline]
    pub fn resize_runner_state(
        &mut self,
        runner_id: RunnerId,
        new_size: u64,
    ) -> Result<u64, super::chunk_planner::Error> {
        self.chunk_planner.resize_runner_state(runner_id, new_size)
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

    #[inline]
    pub fn resize_runner(&mut self, runner_id: RunnerId, new_size: u64) {
        let Self {
            chunk_planner,
            control_channels,
            ..
        } = self;
        let mut guard = PlannerGuard::new(chunk_planner);
        // Limit the total size of the runner
        let downloaded_size = guard
            .planner()
            .resize_runner_state(runner_id, new_size)
            .expect("chunk planner should not fail");

        control_channels
            .send_message(
                runner_id,
                ControlEvent::LimitTotal(downloaded_size + new_size),
            )
            .expect("Manager control channel should never full or closed");

        guard.commit();
    }

    fn handle_notification(
        &mut self,
        msg: RunnerMessage,
        meters: &mut HashMap<RunnerId, usize>,
        on_downloaded: impl FnOnce(Range<u64>, Bytes),
    ) -> bool {
        let RunnerMessage(runner_id, msg) = msg;

        match msg {
            RunnerMessageKind::Stopped(reason) => {
                meters.remove(&runner_id);
                // Remove control channel when runner stops
                self.release_runner(runner_id);
                match reason {
                    StoppedReason::Finished => {
                        trace!("runner {runner_id} finished");
                        self.chunk_planner
                            .mark_finished(runner_id)
                            .expect("chunk planner should not fail");

                        if self.chunk_planner.is_complete() {
                            trace!("[TASK] all chunks finished");
                            return true;
                        }
                    }
                    StoppedReason::Failed(kind) => {
                        error!("runner {runner_id} failed: {kind:?}");
                        match kind {
                            TaskFailedKind::ExceededTotalSize => {
                                self.chunk_planner
                                    .mark_finished(runner_id)
                                    .expect("chunk planner should not fail");
                            }
                            _ => {
                                let unfinished_range = self
                                    .chunk_planner
                                    .mark_failed(runner_id)
                                    .expect("chunk planner should not fail");
                                trace!(
                                    "runner {runner_id} failed, released range: \
                                     {unfinished_range:?}"
                                );
                            }
                        }
                    }
                }
            }
            RunnerMessageKind::Downloaded(mut bytes) => {
                let bytes_len = bytes.len();
                *meters.entry(runner_id).or_insert(0) += bytes_len;

                // update the chunk planner progress
                let fixed_downloaded_range = self
                    .chunk_planner
                    .update_progress(runner_id, bytes_len as u64)
                    .expect("chunk planner should not fail");
                let picked_size = fixed_downloaded_range.end - fixed_downloaded_range.start;
                bytes.truncate(picked_size as usize);

                on_downloaded(fixed_downloaded_range, bytes);
            }
            RunnerMessageKind::Started => {
                trace!("runner {runner_id} started");
            }
        }
        false
    }

    fn handle_pending(&mut self, pending: PendingRunnerOutput) {
        match pending.result {
            Ok(consumer) => {
                self.chunk_planner
                    .mark_running(pending.context.runner_id)
                    .expect("chunk planner should not fail");
                self.register_notification(pending.context.runner_id, consumer);
            }
            Err(error) => {
                error!("failed to create runner: {error:?}");
                self.chunk_planner
                    .mark_failed(pending.context.runner_id)
                    .expect("chunk planner should not fail");
            }
        }
    }

    pub async fn tick(
        &mut self,
        meters: &mut HashMap<RunnerId, usize>,
        on_downloaded: impl FnOnce(Range<u64>, Bytes),
    ) -> TaskState {
        let next_notification = self.runner_notification.next().fuse();
        let pending = self.pending_runners.next().fuse();
        futures::pin_mut!(next_notification, pending);
        futures::select! {
            msg = next_notification => {
                match msg {
                    Some(msg) => {
                        if self.handle_notification(msg, meters, on_downloaded) {
                            return TaskState::Finished;
                        }
                    }
                    None => {
                        assert!(self.runner_notification.is_closed());
                        return TaskState::Finished;
                    }
                }
            }
            pending = pending => {
                match pending {
                    Some(pending) => {
                        self.handle_pending(pending);
                    }
                    None => {
                        assert!(self.chunk_planner.is_complete());
                        return TaskState::Finished;
                    }
                }
            }
        }
        TaskState::Downloading
    }
}

#[cfg(test)]
mod tests {
    use super::{super::chunk_planner::ChunkStatus, *};

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
        assert!(matches!(result, Err(SendError::RunnerNotFound(999))));
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
        let mut manager = RunnerManager::new(1000, 2);

        // Allocate pending runner with chunk
        let result =
            manager.allocate_pending_runner_with_chunk(0..500, |runner_id, _control_rx| {
                assert_eq!(runner_id, 0);
                // Return a receiver that will resolve later
                let (_, rx) =
                    oneshot::channel::<Result<RunnerMessageConsumer, RunnerConnectorError>>();
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
        let mut manager = RunnerManager::new(1000, 4);

        // Allocate multiple pending runners
        for i in 0..4 {
            let start = (i * 250) as u64;
            let end = ((i + 1) * 250) as u64;
            let result = manager.allocate_pending_runner_with_chunk(start..end, |_runner_id, _| {
                let (_, rx) =
                    oneshot::channel::<Result<RunnerMessageConsumer, RunnerConnectorError>>();
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

        use crate::{
            DEFAULT_EVENT_CHANNEL_CAPACITY,
            runner::{RunnerMessage, RunnerMessageKind},
        };

        let mut manager = RunnerManager::new(1000, 2);

        // Create a channel to send the consumer
        let (tx, rx) = oneshot::channel();

        // Allocate pending runner
        manager.allocate_pending_runner_with_chunk(0..500, |runner_id, _control_rx| {
            assert_eq!(runner_id, 0);
            rx
        });

        // Verify initial state is pending
        let state = manager.get_runner_state(0).unwrap();
        assert_eq!(state.status, ChunkStatus::Pending);

        // Create a mock consumer
        let rb = AsyncHeapRb::<RunnerMessage>::new(DEFAULT_EVENT_CHANNEL_CAPACITY);
        let (_prod, cons) = rb.split();

        // Send the consumer through the channel
        tx.send(Ok(cons)).unwrap();

        // Tick to process the pending runner
        let mut meters = HashMap::new();
        let state = manager.tick(&mut meters, |_, _| {}).await;

        assert_eq!(state, TaskState::Downloading);

        // Verify state is now running
        let chunk_state = manager.get_runner_state(0).unwrap();
        assert_eq!(chunk_state.status, ChunkStatus::Running);
    }

    #[tokio::test]
    async fn test_pending_connection_failure_via_tick() {
        use crate::runner::RunnerConnectorError;

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
        tx.send(Err(RunnerConnectorError::Build {
            source: crate::runner::TaskRunnerBuilderError::StreamNotSet,
        }))
        .unwrap();

        // Tick to process the pending runner
        let mut meters = HashMap::new();
        let state = manager.tick(&mut meters, |_, _| {}).await;

        assert_eq!(state, TaskState::Downloading);

        // Verify runner state is removed (failed)
        assert!(manager.get_runner_state(0).is_none());
    }
}
