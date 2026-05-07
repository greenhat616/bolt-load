use std::cmp::Ordering;

use async_ringbuf::{AsyncHeapCons, AsyncHeapProd, traits::*};
use bolt_load_utils::telemetry::*;
use bytes::{Bytes, BytesMut};
use future::FlushBuffFuture;
use futures::{FutureExt, StreamExt};
use smol_cancellation_token::CancellationToken;

use crate::{
    adapter::{AdapterError, AnyBytesStream},
    task::{ControlEvent, RunnerId},
    utils::ShutdownGuardExt,
};

/// Type alias for the control signal receiver
/// Using async_channel for point-to-point communication instead of broadcast
pub type ControlSignalReceiver = async_channel::Receiver<ControlEvent>;

mod builder;
mod connector;
mod error;
mod future;
mod guard;
pub use builder::*;
pub use connector::*;
pub use error::*;
pub use guard::*;

// TODO: make it configurable or detect the local disk performance?
const BUFFER_SIZE: usize = 32 * 1024; // 32KB

/// The timeout for the slow stream
const SLOW_STREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const DATA_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub const LIFECYCLE_CHANNEL_CAPACITY: usize = 2;
pub const DATA_FRAME_CHANNEL_CAPACITY: usize = 32;

#[derive(Debug)]
pub struct DataFrame {
    pub data: Bytes,
}

pub type DataFrameSender = AsyncHeapProd<DataFrame>;
pub type DataFrameReceiver = AsyncHeapCons<DataFrame>;
pub type LifecycleSender = AsyncHeapProd<LifecycleEvent>;
pub type LifecycleReceiver = AsyncHeapCons<LifecycleEvent>;

#[derive(Debug)]
pub enum LifecycleEvent {
    /// The task is started
    Started,
    /// Stopped with message
    Stopped(StoppedReason),
}

impl LifecycleEvent {
    #[inline]
    pub fn failed(e: TaskError) -> Self {
        Self::Stopped(StoppedReason::Failed(e))
    }

    #[inline]
    pub fn finished() -> Self {
        Self::Stopped(StoppedReason::Finished)
    }

    #[inline]
    pub fn started() -> Self {
        Self::Started
    }

    #[inline]
    pub const fn is_finished(&self) -> bool {
        matches!(self, Self::Stopped(StoppedReason::Finished))
    }

    #[inline]
    pub fn is_cancelled(&self) -> bool {
        match self {
            Self::Stopped(StoppedReason::Failed(e)) => e.is_cancelled(),
            _ => false,
        }
    }
}

/// The reason why the task is stopped
#[derive(Debug)]
pub enum StoppedReason {
    /// The task is finished
    Finished,
    /// The task is failed
    Failed(TaskError),
}

#[derive(Debug)]
enum TaskRunError {
    Cancelled,
    Failed(TaskError),
}

impl From<TaskError> for TaskRunError {
    fn from(value: TaskError) -> Self {
        Self::Failed(value)
    }
}

/// runner for each chunk, or single file, responsible for downloading each chunk
#[derive(derive_more::Debug)]
pub struct TaskRunner {
    /// The id of the runner
    id: RunnerId,
    /// The total size of the this chunk or file
    /// possible None if the total size is unknown
    total: Option<u64>,
    /// the downloaded size
    downloaded: u64,
    /// the adapter of the task
    #[debug(skip)]
    stream: AnyBytesStream,
    /// the receiver of the manager messages (point-to-point channel)
    #[debug(skip)]
    control_signal: ControlSignalReceiver,
    /// the sender of the lifecycle events
    #[debug(skip)]
    lifecycle_tx: LifecycleSender,
    /// the sender of the data frames
    #[debug(skip)]
    data_tx: Option<DataFrameSender>,
    /// How long to wait for data frames to be consumed after the stream stops.
    data_drain_timeout: std::time::Duration,
    /// the cancel token
    cancel_token: CancellationToken,
    /// the shutdown signal, used for ensure the task runner is stopped
    shutdown_rx: Option<oneshot::Receiver<()>>,
}

/// The step of the event loop
enum EventLoopStep {
    /// The task is cancelled
    Cancelled,
    /// The stream is too slow, and transfer 0 bytes in the last SLOW_STREAM_TIMEOUT
    SlowTransfer,
    /// The control signal is received (point-to-point, no need for RunnerId filtering)
    Control(Result<ControlEvent, async_channel::RecvError>),
    /// The download event is received
    Download(Option<Result<Bytes, AdapterError>>),
    /// The pending flush operation completed
    PendingComplete(Result<DataFrameSender, future::ChannelClosed>),
}

/// Bundles a flush future with the byte count being flushed
struct FlushRequest {
    future: FlushBuffFuture,
    flushed_bytes: u64,
}

/// The operation of the stream event
enum StreamEventOperation {
    /// Flush the buffer to the data channel
    FlushBuff {
        pending: FlushRequest,
        post_action: PendingPostAction,
    },
    /// The stream is errored, and we need to flush the remaining bytes in the buffer
    StreamError {
        error: AdapterError,
        /// flush remaining bytes in the buffer, and break the loop
        pending: Option<FlushRequest>,
    },
    /// Stream ended with an empty buffer — nothing to flush
    FinishWithoutFlush,
}

/// What to do after a pending flush future completes
enum PendingPostAction {
    /// Resume normal event loop (buffer was full, stream continues)
    Resume,
    /// Continue processing the remainder of a chunk that didn't fit in the buffer
    ContinueItem { item: Bytes, offset: usize },
    /// Stream is finished, break with Ok(())
    FinishSuccess,
    /// Stream had an error, propagate it after flush
    PropagateError(AdapterError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DataDrainOutcome {
    Drained,
    Cancelled,
    TimedOut,
}

/// Bundles a pending flush future with its post-completion action
struct PendingFlushState {
    future: FlushBuffFuture,
    flushed_bytes: u64,
    post_action: PendingPostAction,
}

impl TaskRunner {
    /// Get the builder of the task runner
    pub fn builder() -> TaskRunnerBuilder {
        TaskRunnerBuilder::default()
    }

    /// run the task runner
    /// This function will block until the task is finished or cancelled
    /// It should be called in a new thread or async spawn context
    #[cfg(feature = "tracing")]
    #[tracing::instrument(skip(self), name = "TaskRunner::run", fields(runner_id = self.id))]
    pub async fn run(&mut self) {
        let mut result = self.run_inner().await;
        if Self::should_wait_data_drain(&result) {
            match self.wait_data_drained().await {
                DataDrainOutcome::Drained => {}
                DataDrainOutcome::Cancelled => result = Err(TaskRunError::Cancelled),
                DataDrainOutcome::TimedOut => {
                    warn!(
                        "data drain timed out; reporting as timeout so undrained bytes are retried"
                    );
                    result = Err(TaskRunError::Failed(TaskError::Timeout));
                }
            }
        }

        match result {
            Ok(_) => {
                let _ = self.lifecycle_tx.push(LifecycleEvent::finished()).await;
            }
            Err(err) => match err {
                TaskRunError::Cancelled => {
                    let _ = self
                        .lifecycle_tx
                        .push(LifecycleEvent::failed(TaskError::Cancelled))
                        .await;
                }
                TaskRunError::Failed(task_err) => {
                    let _ = self
                        .lifecycle_tx
                        .push(LifecycleEvent::failed(task_err))
                        .await;
                }
            },
        }
        self.data_tx.take();
    }

    fn should_wait_data_drain(result: &Result<(), TaskRunError>) -> bool {
        matches!(
            result,
            Ok(())
                | Err(TaskRunError::Failed(
                    TaskError::ExceededTotalSize | TaskError::StreamError { .. },
                ))
        )
    }

    async fn wait_data_drained(&mut self) -> DataDrainOutcome {
        let Some(data_tx) = self.data_tx.as_mut() else {
            return DataDrainOutcome::Drained;
        };

        if self.cancel_token.is_cancelled() {
            return DataDrainOutcome::Cancelled;
        }
        if data_tx.is_closed() {
            return DataDrainOutcome::Drained;
        }

        let timeout = async_io::Timer::after(self.data_drain_timeout);
        let drained = data_tx.wait_vacant(DATA_FRAME_CHANNEL_CAPACITY).fuse();
        let cancelled = self.cancel_token.cancelled().fuse();
        let timeout = futures::FutureExt::fuse(timeout);
        futures::pin_mut!(drained, cancelled, timeout);

        futures::select_biased! {
            _ = cancelled => DataDrainOutcome::Cancelled,
            _ = drained => DataDrainOutcome::Drained,
            _ = timeout => DataDrainOutcome::TimedOut,
        }
    }

    /// Handle the control signal from the manager
    ///
    /// Since we now use point-to-point channels, the signal is guaranteed to be for this runner.
    #[cfg_attr(feature = "tracing", tracing::instrument(
        skip(self),
        name = "TaskRunner::handle_control_signal",
        fields(runner_id = self.id)
    ))]
    fn handle_control_signal(&mut self, signal: &ControlEvent) -> Result<(), TaskError> {
        match signal {
            ControlEvent::LimitTotal(new_total) => {
                if let Some(current_total) = self.total {
                    if current_total > *new_total {
                        trace!("runner: limit total to {new_total}");
                        self.total = Some(*new_total);
                    } else {
                        warn!(
                            "runner: limit total is larger than current total,
                            limit: {new_total}, current: {current_total}"
                        );
                        Err(TaskError::ExceededTotalSize)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn flush_buff(&mut self, buff: &mut BytesMut) -> Result<Option<FlushRequest>, TaskRunError> {
        if !buff.is_empty() {
            let data_tx = self.data_tx.take().ok_or(TaskError::ChannelClosed)?;
            let flushed_bytes = buff.len() as u64;
            let chunk = buff.split().freeze();
            return Ok(Some(FlushRequest {
                future: FlushBuffFuture::new(data_tx, DataFrame { data: chunk }),
                flushed_bytes,
            }));
        }
        Ok(None)
    }

    /// Handle the stream event
    ///
    /// This function will handle the stream event, and flush the buffer if needed
    /// It will also check if the stream is finished, and set the is_finished flag if needed
    ///
    /// # Arguments
    /// * `event` - The stream event
    #[cfg_attr(feature = "tracing", tracing::instrument(
        skip_all,
        name = "TaskRunner::handle_stream_event",
        fields(runner_id = self.id)
    ))]
    fn handle_stream_event(
        &mut self,
        event: Option<Result<Bytes, AdapterError>>,
        buff: &mut BytesMut,
    ) -> Result<Option<StreamEventOperation>, TaskRunError> {
        Ok(match event {
            Some(Ok(item)) => {
                if let Some(total) = self.total {
                    let current_downloaded = self.downloaded + buff.len() as u64;
                    if current_downloaded >= total {
                        return Ok(Some(match self.flush_buff(buff)? {
                            Some(flush) => StreamEventOperation::FlushBuff {
                                pending: flush,
                                post_action: PendingPostAction::FinishSuccess,
                            },
                            None => StreamEventOperation::FinishWithoutFlush,
                        }));
                    }
                }

                if item.is_empty() {
                    return Ok(None);
                }

                let bytes_to_write = if let Some(total) = self.total {
                    let current_downloaded = self.downloaded + buff.len() as u64;
                    let remaining = total.saturating_sub(current_downloaded);
                    item.len().min(remaining as usize)
                } else {
                    item.len()
                };

                if bytes_to_write == 0 {
                    return Ok(Some(match self.flush_buff(buff)? {
                        Some(flush) => StreamEventOperation::FlushBuff {
                            pending: flush,
                            post_action: PendingPostAction::FinishSuccess,
                        },
                        None => StreamEventOperation::FinishWithoutFlush,
                    }));
                }

                if buff.len() + bytes_to_write > BUFFER_SIZE {
                    let take = BUFFER_SIZE.saturating_sub(buff.len()).min(bytes_to_write);
                    if take > 0 {
                        buff.extend_from_slice(&item[..take]);
                    }
                    let Some(flush) = self.flush_buff(buff)? else {
                        return Err(TaskError::Other {
                            message: "buffer unexpectedly empty after fill".to_string(),
                        }
                        .into());
                    };
                    let remaining = bytes_to_write - take;
                    return Ok(Some(StreamEventOperation::FlushBuff {
                        pending: flush,
                        post_action: if remaining > 0 {
                            PendingPostAction::ContinueItem { item, offset: take }
                        } else {
                            PendingPostAction::Resume
                        },
                    }));
                }

                buff.extend_from_slice(&item[..bytes_to_write]);

                if let Some(total) = self.total {
                    let current_downloaded = self.downloaded + buff.len() as u64;
                    if current_downloaded >= total {
                        return Ok(Some(match self.flush_buff(buff)? {
                            Some(flush) => StreamEventOperation::FlushBuff {
                                pending: flush,
                                post_action: PendingPostAction::FinishSuccess,
                            },
                            None => StreamEventOperation::FinishWithoutFlush,
                        }));
                    }
                }
                None
            }
            Some(Err(err)) => Some(StreamEventOperation::StreamError {
                error: err,
                pending: self.flush_buff(buff)?,
            }),
            None => Some(match self.flush_buff(buff)? {
                Some(flush) => StreamEventOperation::FlushBuff {
                    pending: flush,
                    post_action: PendingPostAction::FinishSuccess,
                },
                None => StreamEventOperation::FinishWithoutFlush,
            }),
        })
    }

    /// The inner logic of the task runner
    /// Just wrap a Result<(), TaskFailedKind> to return the error kind
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip(self), name = "TaskRunner::run_inner", fields(runner_id = self.id))
    )]
    async fn run_inner(&mut self) -> Result<(), TaskRunError> {
        trace!("runner: started");
        self.lifecycle_tx
            .push(LifecycleEvent::Started)
            .await
            .map_err(|_| TaskRunError::Failed(TaskError::ChannelClosed))?;

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let _guard = shutdown_tx.shutdown_guard();
        self.shutdown_rx = Some(shutdown_rx);
        let mut buff = BytesMut::with_capacity(BUFFER_SIZE);
        let mut slow_transfer_timer = async_io::Timer::after(SLOW_STREAM_TIMEOUT);
        let mut pending_state: Option<PendingFlushState> = None;
        let mut deferred_control_error: Option<TaskRunError> = None;
        let mut control_channel_closed = false;
        let result = loop {
            let step: EventLoopStep = async {
                let cancelled = self.cancel_token.cancelled().fuse();
                futures::pin_mut!(cancelled);

                if let Some(state) = pending_state.as_mut() {
                    // PENDING MODE: only cancel + control + pending flush
                    let pending_flush = (&mut state.future).fuse();
                    futures::pin_mut!(pending_flush);
                    if control_channel_closed {
                        futures::select_biased! {
                            _ = cancelled => EventLoopStep::Cancelled,
                            result = pending_flush => EventLoopStep::PendingComplete(result),
                        }
                    } else {
                        let control_signal = self.control_signal.recv().fuse();
                        futures::pin_mut!(control_signal);
                        futures::select_biased! {
                            _ = cancelled => EventLoopStep::Cancelled,
                            signal = control_signal => EventLoopStep::Control(signal),
                            result = pending_flush => EventLoopStep::PendingComplete(result),
                        }
                    }
                } else {
                    // NORMAL MODE: full event loop
                    let control_signal = self.control_signal.recv().fuse();
                    let download = self.stream.next().fuse();
                    let slow_transfer = slow_transfer_timer.next().fuse();
                    futures::pin_mut!(control_signal, download, slow_transfer);
                    futures::select_biased! {
                        _ = cancelled => EventLoopStep::Cancelled,
                        signal = control_signal => EventLoopStep::Control(signal),
                        _ = slow_transfer => {
                            slow_transfer_timer.set_after(SLOW_STREAM_TIMEOUT);
                            EventLoopStep::SlowTransfer
                        }
                        result = download => {
                            slow_transfer_timer.set_after(SLOW_STREAM_TIMEOUT);
                            EventLoopStep::Download(result)
                        },
                    }
                }
            }
            .await;

            match step {
                EventLoopStep::Cancelled => {
                    trace!("runner: cancelled");
                    break Err(TaskRunError::Cancelled);
                }
                EventLoopStep::Control(signal) => match signal {
                    Ok(variant) => {
                        trace!("runner: control signal: {variant:?}");
                        if let Err(err) = self.handle_control_signal(&variant) {
                            let err = err.into();
                            if pending_state.is_some() {
                                if deferred_control_error.is_none() {
                                    deferred_control_error = Some(err);
                                } else {
                                    warn!("discarding control error during pending flush: {err:?}");
                                }
                                continue;
                            }
                            break Err(err);
                        }
                        trace!(
                            "current downloaded: {}",
                            self.downloaded as usize + buff.len()
                        );
                    }
                    Err(_) => {
                        // Control channel closed - manager has released this runner
                        trace!("runner: control signal channel closed");
                        let err = TaskError::ChannelClosed.into();
                        if pending_state.is_some() {
                            control_channel_closed = true;
                            if deferred_control_error.is_none() {
                                deferred_control_error = Some(err);
                            } else {
                                warn!("discarding control error during pending flush: {err:?}");
                            }
                            continue;
                        }
                        break Err(err);
                    }
                },
                EventLoopStep::SlowTransfer => {
                    warn!(
                        "runner: very slow stream, transfer 0 bytes in the last {} seconds",
                        SLOW_STREAM_TIMEOUT.as_secs()
                    );
                }
                EventLoopStep::Download(item) => match self.handle_stream_event(item, &mut buff) {
                    Ok(Some(StreamEventOperation::FlushBuff {
                        pending,
                        post_action,
                    })) => {
                        pending_state = Some(PendingFlushState {
                            future: pending.future,
                            flushed_bytes: pending.flushed_bytes,
                            post_action,
                        });
                    }
                    Ok(Some(StreamEventOperation::StreamError { error, pending })) => match pending
                    {
                        Some(flush) => {
                            pending_state = Some(PendingFlushState {
                                future: flush.future,
                                flushed_bytes: flush.flushed_bytes,
                                post_action: PendingPostAction::PropagateError(error),
                            });
                        }
                        None => {
                            break Err(TaskError::StreamError { source: error }.into());
                        }
                    },
                    Ok(Some(StreamEventOperation::FinishWithoutFlush)) => {
                        break Ok(());
                    }
                    Ok(None) => {}
                    Err(err) => break Err(err),
                },
                EventLoopStep::PendingComplete(flush_result) => {
                    let Some(state) = pending_state.take() else {
                        break Err(TaskError::Other {
                            message: "pending flush completed without pending state".to_string(),
                        }
                        .into());
                    };

                    let data_tx = match flush_result {
                        Ok(sender) => sender,
                        Err(_) => break Err(TaskError::ChannelClosed.into()),
                    };
                    self.data_tx = Some(data_tx);
                    self.downloaded += state.flushed_bytes;

                    if let PendingPostAction::PropagateError(error) = state.post_action {
                        break Err(TaskError::StreamError { source: error }.into());
                    }

                    if let Some(err) = deferred_control_error.take() {
                        break Err(err);
                    }

                    match state.post_action {
                        PendingPostAction::Resume => {
                            slow_transfer_timer.set_after(SLOW_STREAM_TIMEOUT);
                        }
                        PendingPostAction::ContinueItem { item, offset } => {
                            let remaining_in_item = item.len().saturating_sub(offset);
                            let allowed = self
                                .total
                                .map(|t| {
                                    t.saturating_sub(self.downloaded + buff.len() as u64) as usize
                                })
                                .unwrap_or(usize::MAX);
                            let writable = remaining_in_item.min(allowed);
                            let take = writable.min(BUFFER_SIZE.saturating_sub(buff.len()));

                            if take == 0 || allowed == 0 {
                                if let Some(flush) = self.flush_buff(&mut buff)? {
                                    pending_state = Some(PendingFlushState {
                                        future: flush.future,
                                        flushed_bytes: flush.flushed_bytes,
                                        post_action: PendingPostAction::FinishSuccess,
                                    });
                                    continue;
                                }
                                break Ok(());
                            }

                            buff.extend_from_slice(&item[offset..offset + take]);
                            let next_offset = offset + take;
                            if let Some(total) = self.total {
                                if self.downloaded + buff.len() as u64 >= total {
                                    let Some(flush) = self.flush_buff(&mut buff)? else {
                                        break Err(TaskError::Other {
                                            message: "buffer unexpectedly empty after continuing \
                                                      item"
                                                .to_string(),
                                        }
                                        .into());
                                    };
                                    pending_state = Some(PendingFlushState {
                                        future: flush.future,
                                        flushed_bytes: flush.flushed_bytes,
                                        post_action: PendingPostAction::FinishSuccess,
                                    });
                                    continue;
                                }
                            }

                            if next_offset < item.len() && take < writable {
                                let Some(flush) = self.flush_buff(&mut buff)? else {
                                    break Err(TaskError::Other {
                                        message: "buffer unexpectedly empty after continuing item"
                                            .to_string(),
                                    }
                                    .into());
                                };
                                pending_state = Some(PendingFlushState {
                                    future: flush.future,
                                    flushed_bytes: flush.flushed_bytes,
                                    post_action: PendingPostAction::ContinueItem {
                                        item,
                                        offset: next_offset,
                                    },
                                });
                                continue;
                            }
                            slow_transfer_timer.set_after(SLOW_STREAM_TIMEOUT);
                        }
                        PendingPostAction::FinishSuccess => break Ok(()),
                        PendingPostAction::PropagateError(error) => {
                            break Err(TaskError::StreamError { source: error }.into());
                        }
                    }
                }
            }
        };
        result?;

        // TODO: handle the error
        match self.total {
            Some(total) => match total.cmp(&self.downloaded) {
                Ordering::Equal => {}
                Ordering::Greater => {
                    warn!(
                        "runner: downloaded content is smaller than the total size, total: {}, \
                         downloaded: {}",
                        total, self.downloaded
                    );
                    return Err(TaskError::SmallerThanTotalSize.into());
                }
                Ordering::Less => {
                    warn!(
                        "runner: downloaded content is larger than the total size, total: {}, \
                         downloaded: {}",
                        total, self.downloaded
                    );
                    return Err(TaskError::ExceededTotalSize.into());
                }
            },
            None if self.downloaded > 0 => {}
            None => {
                return Err(TaskError::Empty.into());
            }
        }
        Ok(())
    }

    /// Cancel the current task runner,
    pub async fn cancel(&mut self) {
        self.cancel_token.cancel();
        if let Some(shutdown_rx) = self.shutdown_rx.take() {
            let _ = shutdown_rx.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{pin::pin, sync::Arc, time::Duration};

    use async_stream::stream;
    use pretty_assertions::assert_eq;
    use tokio::time::{sleep, timeout};

    use super::*;
    use crate::adapter::{AdapterError, UnretryableError};

    fn patterned_frame(len: usize) -> Bytes {
        Bytes::from((0..len).map(|idx| (idx % 251) as u8).collect::<Vec<_>>())
    }

    fn stream_io_error(message: &'static str) -> AdapterError {
        AdapterError::Unretryable {
            source: UnretryableError::Io {
                source: Arc::new(std::io::Error::other(message)),
            },
        }
    }

    async fn run_runner_and_collect(
        stream: AnyBytesStream,
        total: Option<u64>,
    ) -> (Vec<u8>, bool, Option<TaskError>) {
        let (_control_tx, control_rx) = async_channel::bounded(1);
        let token = CancellationToken::new();
        let mut builder = TaskRunner::builder()
            .stream(stream)
            .runner_id(1)
            .control_signal(control_rx)
            .cancel_token(token);
        if let Some(total) = total {
            builder = builder.total(total);
        }

        let (mut runner, lifecycle_rx, data_rx) = builder.build().unwrap();
        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        let mut lifecycle_rx = pin!(lifecycle_rx);
        let mut data_rx = pin!(data_rx);
        let mut data_done = false;
        let mut stopped = false;
        let mut finished = false;
        let mut failed = None;
        let mut received = Vec::new();

        timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    msg = lifecycle_rx.next(), if !stopped => {
                        match msg {
                            Some(LifecycleEvent::Started) => {}
                            Some(LifecycleEvent::Stopped(StoppedReason::Finished)) => {
                                stopped = true;
                                finished = true;
                            }
                            Some(LifecycleEvent::Stopped(StoppedReason::Failed(err))) => {
                                stopped = true;
                                failed = Some(err);
                            }
                            None => {
                                stopped = true;
                            }
                        }
                    }
                    frame = data_rx.next(), if !data_done => {
                        match frame {
                            Some(frame) => received.extend_from_slice(&frame.data),
                            None => data_done = true,
                        }
                    }
                }

                if stopped && data_done {
                    break;
                }
            }
        })
        .await
        .expect("timed out collecting runner output");

        runner_handle.await.unwrap();
        (received, finished, failed)
    }

    #[tokio::test]
    async fn test_drain_zero_timeout_empty_channel_returns_drained() {
        let (_control_tx, control_rx) = async_channel::bounded(1);
        let token = CancellationToken::new();
        let test_stream = futures::stream::pending::<Result<Bytes, AdapterError>>();

        let (mut runner, _lifecycle_rx, _data_rx) = TaskRunner::builder()
            .stream(Box::pin(test_stream))
            .runner_id(1)
            .control_signal(control_rx)
            .cancel_token(token)
            .data_drain_timeout(Duration::ZERO)
            .build()
            .unwrap();

        assert_eq!(runner.wait_data_drained().await, DataDrainOutcome::Drained);
    }

    #[tokio::test]
    async fn test_normal_download() {
        let (_control_tx, control_rx) = async_channel::bounded(1);
        let token = CancellationToken::new();

        // Create a stream that emits 3 chunks
        let test_stream = stream! {
            for i in 0..3 {
                yield Ok(Bytes::from(vec![i as u8; 10]));
            }
        };

        let (mut runner, lifecycle_rx, data_rx) = TaskRunner::builder()
            .total(30) // Total size: 3 chunks * 10 bytes
            .stream(Box::pin(test_stream))
            .runner_id(1)
            .control_signal(control_rx)
            .cancel_token(token)
            .build()
            .unwrap();

        // Spawn the runner
        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        // Collect all messages
        let mut downloaded_size = 0;
        let mut started = false;
        let mut finished = false;

        let mut lifecycle_rx = pin!(lifecycle_rx);
        let mut data_rx = pin!(data_rx);
        let mut data_done = false;
        loop {
            tokio::select! {
                msg = lifecycle_rx.next() => {
                    match msg {
                        Some(LifecycleEvent::Started) => {
                            started = true;
                        }
                        Some(LifecycleEvent::Stopped(StoppedReason::Finished)) => {
                            finished = true;
                        }
                        Some(other) => panic!("Unexpected lifecycle event: {other:?}"),
                        None => break,
                    }
                }
                frame = data_rx.next(), if !data_done => {
                    match frame {
                        Some(frame) => {
                            downloaded_size += frame.data.len();
                        }
                        None => { data_done = true; }
                    }
                }
            }

            if finished && data_done {
                break;
            }
        }

        runner_handle.await.unwrap();
        assert!(started);
        assert!(finished);
        assert_eq!(downloaded_size, 30);
    }

    #[tokio::test]
    async fn test_single_large_frame_known_total_preserves_all_bytes() {
        let frame = patterned_frame(BUFFER_SIZE * 3 + 123);
        let expected = frame.to_vec();
        let stream = stream! {
            yield Ok(frame);
        };

        let (received, finished, failed) =
            run_runner_and_collect(Box::pin(stream), Some(expected.len() as u64)).await;

        assert!(finished);
        assert!(failed.is_none());
        assert_eq!(received, expected);
    }

    #[tokio::test]
    async fn test_single_large_frame_unknown_total_preserves_all_bytes() {
        let frame = patterned_frame(BUFFER_SIZE * 3 + 123);
        let expected = frame.to_vec();
        let stream = stream! {
            yield Ok(frame);
        };

        let (received, finished, failed) = run_runner_and_collect(Box::pin(stream), None).await;

        assert!(finished);
        assert!(failed.is_none());
        assert_eq!(received, expected);
    }

    #[tokio::test]
    async fn test_single_frame_exactly_two_buffers_preserves_all_bytes() {
        let frame = patterned_frame(BUFFER_SIZE * 2);
        let expected = frame.to_vec();
        let stream = stream! {
            yield Ok(frame);
        };

        let (received, finished, failed) =
            run_runner_and_collect(Box::pin(stream), Some(expected.len() as u64)).await;

        assert!(finished);
        assert!(failed.is_none());
        assert_eq!(received, expected);
    }

    #[tokio::test]
    async fn test_pending_flush_survives_control_error() {
        let (control_tx, control_rx) = async_channel::bounded(4);
        let cancel_token = CancellationToken::new();
        let frame_count = DATA_FRAME_CHANNEL_CAPACITY + 4;
        let total = (frame_count * BUFFER_SIZE) as u64;
        let test_stream = stream! {
            for i in 0..frame_count {
                yield Ok(Bytes::from(vec![i as u8; BUFFER_SIZE]));
            }
            std::future::pending::<()>().await;
        };

        let (mut runner, lifecycle_rx, data_rx) = TaskRunner::builder()
            .total(total)
            .stream(Box::pin(test_stream))
            .runner_id(1)
            .control_signal(control_rx)
            .cancel_token(cancel_token)
            .build()
            .unwrap();

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        let mut lifecycle_rx = pin!(lifecycle_rx);
        let mut data_rx = pin!(data_rx);

        timeout(Duration::from_secs(2), async {
            while let Some(event) = lifecycle_rx.next().await {
                if matches!(event, LifecycleEvent::Started) {
                    break;
                }
            }
        })
        .await
        .expect("timed out waiting for runner start");

        sleep(Duration::from_millis(100)).await;
        control_tx
            .send(ControlEvent::LimitTotal(total + BUFFER_SIZE as u64))
            .await
            .unwrap();

        let mut received = 0;
        let mut data_done = false;
        let mut failed_exceeded_total = false;
        timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    frame = data_rx.next(), if !data_done => {
                        match frame {
                            Some(frame) => received += frame.data.len(),
                            None => data_done = true,
                        }
                    }
                    event = lifecycle_rx.next(), if !failed_exceeded_total => {
                        match event {
                            Some(LifecycleEvent::Stopped(StoppedReason::Failed(TaskError::ExceededTotalSize))) => {
                                failed_exceeded_total = true;
                            }
                            Some(LifecycleEvent::Stopped(other)) => {
                                panic!("expected ExceededTotalSize, got {other:?}");
                            }
                            Some(LifecycleEvent::Started) => {}
                            None => break,
                        }
                    }
                }

                if data_done && failed_exceeded_total {
                    break;
                }
            }
        })
        .await
        .expect("timed out collecting deferred control error output");

        runner_handle.await.unwrap();
        assert!(failed_exceeded_total);
        assert!(
            received > DATA_FRAME_CHANNEL_CAPACITY * BUFFER_SIZE,
            "pending frame was dropped: received {received} bytes"
        );
    }

    #[tokio::test]
    async fn test_control_drop_during_pending_flush_no_livelock() {
        let (control_tx, control_rx) = async_channel::bounded(1);
        let cancel_token = CancellationToken::new();
        let frame = patterned_frame((DATA_FRAME_CHANNEL_CAPACITY + 2) * BUFFER_SIZE);
        let test_stream = stream! {
            yield Ok(frame);
            std::future::pending::<()>().await;
        };

        let (mut runner, lifecycle_rx, data_rx) = TaskRunner::builder()
            .stream(Box::pin(test_stream))
            .runner_id(1)
            .control_signal(control_rx)
            .cancel_token(cancel_token)
            .build()
            .unwrap();

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        let mut lifecycle_rx = pin!(lifecycle_rx);
        let data_rx = data_rx;

        timeout(Duration::from_secs(2), async {
            loop {
                match lifecycle_rx.next().await {
                    Some(LifecycleEvent::Started) => break,
                    Some(other) => panic!("unexpected lifecycle event before start: {other:?}"),
                    None => panic!("lifecycle channel closed before start"),
                }
            }
        })
        .await
        .expect("timed out waiting for runner start");

        timeout(Duration::from_secs(2), async {
            while data_rx.occupied_len() < DATA_FRAME_CHANNEL_CAPACITY {
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("timed out waiting for pending flush backpressure");

        sleep(Duration::from_millis(20)).await;
        drop(control_tx);

        let mut data_rx = pin!(data_rx);
        let mut received = 0;
        let mut data_done = false;
        let mut got_channel_closed = false;
        timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    frame = data_rx.next(), if !data_done => {
                        match frame {
                            Some(frame) => received += frame.data.len(),
                            None => data_done = true,
                        }
                    }
                    event = lifecycle_rx.next(), if !got_channel_closed => {
                        match event {
                            Some(LifecycleEvent::Stopped(StoppedReason::Failed(
                                TaskError::ChannelClosed,
                            ))) => {
                                got_channel_closed = true;
                            }
                            Some(LifecycleEvent::Started) => {}
                            Some(other) => panic!("expected ChannelClosed, got {other:?}"),
                            None => panic!("lifecycle channel closed before ChannelClosed"),
                        }
                    }
                }

                if data_done && got_channel_closed {
                    break;
                }
            }
        })
        .await
        .expect("timed out collecting control-drop output");

        runner_handle.await.unwrap();
        assert!(got_channel_closed);
        assert!(
            received > DATA_FRAME_CHANNEL_CAPACITY * BUFFER_SIZE,
            "pending frame was not flushed: received {received} bytes"
        );
    }

    #[tokio::test]
    async fn test_cancel_download() {
        let (_control_tx, control_rx) = async_channel::bounded(1);
        let cancel_token = CancellationToken::new();

        // Create an infinite stream that we'll cancel
        let test_stream = stream! {
            loop {
                sleep(Duration::from_millis(10)).await;
                yield Ok(Bytes::from(vec![1; 10]));
            }
        };

        let (mut runner, lifecycle_rx, _data_rx) = TaskRunner::builder()
            .stream(Box::pin(test_stream))
            .runner_id(1)
            .control_signal(control_rx)
            .cancel_token(cancel_token.clone())
            .build()
            .unwrap();

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        // Wait for the Started message
        let mut started = false;
        let mut lifecycle_rx = pin!(lifecycle_rx);
        while let Some(msg) = lifecycle_rx.next().await {
            if let LifecycleEvent::Started = msg {
                started = true;
                break;
            }
        }

        // Send cancel signal
        cancel_token.cancel();

        // Wait for cancelled message
        let mut cancelled = false;
        while let Some(msg) = lifecycle_rx.next().await {
            if msg.is_cancelled() {
                cancelled = true;
                break;
            }
        }

        runner_handle.await.unwrap();
        assert!(started);
        assert!(cancelled);
    }

    #[tokio::test]
    async fn test_network_error() {
        // Create a stream that yields an error
        let test_stream = stream! {
            yield Ok(Bytes::from(vec![1; 10]));
            yield Err(stream_io_error("Network error"));
        };

        let (received, finished, failed) =
            run_runner_and_collect(Box::pin(test_stream), Some(20)).await;

        assert!(!finished);
        assert_eq!(received, vec![1; 10]);
        assert!(matches!(failed, Some(err) if err.is_stream_error()));
    }

    #[tokio::test]
    async fn test_stream_error_after_data_flush_waits_drain() {
        let (_control_tx, control_rx) = async_channel::bounded(1);
        let cancel_token = CancellationToken::new();
        let test_stream = stream! {
            yield Ok(Bytes::from_static(b"abc"));
            yield Err(stream_io_error("Network error"));
        };

        let (mut runner, lifecycle_rx, data_rx) = TaskRunner::builder()
            .stream(Box::pin(test_stream))
            .runner_id(1)
            .control_signal(control_rx)
            .cancel_token(cancel_token)
            .build()
            .unwrap();

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        let mut lifecycle_rx = pin!(lifecycle_rx);
        let data_rx = data_rx;

        timeout(Duration::from_secs(2), async {
            loop {
                match lifecycle_rx.next().await {
                    Some(LifecycleEvent::Started) => break,
                    Some(other) => panic!("unexpected lifecycle event before start: {other:?}"),
                    None => panic!("lifecycle channel closed before start"),
                }
            }
        })
        .await
        .expect("timed out waiting for runner start");

        timeout(Duration::from_secs(2), async {
            while data_rx.occupied_len() == 0 {
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("timed out waiting for flushed data");

        let stopped_before_drain = timeout(Duration::from_millis(100), async {
            loop {
                match lifecycle_rx.next().await {
                    Some(LifecycleEvent::Started) => {}
                    Some(LifecycleEvent::Stopped(_)) | None => break,
                }
            }
        })
        .await;
        assert!(
            stopped_before_drain.is_err(),
            "runner reported stopped before flushed data was drained"
        );

        let mut data_rx = pin!(data_rx);
        let frame = data_rx.next().await.expect("expected flushed data frame");
        assert_eq!(frame.data, Bytes::from_static(b"abc"));

        let mut got_stream_error = false;
        timeout(Duration::from_secs(2), async {
            while let Some(msg) = lifecycle_rx.next().await {
                match msg {
                    LifecycleEvent::Stopped(StoppedReason::Failed(err)) => {
                        assert!(err.is_stream_error());
                        got_stream_error = true;
                        break;
                    }
                    LifecycleEvent::Started => {}
                    other => panic!("unexpected lifecycle event: {other:?}"),
                }
            }
        })
        .await
        .expect("timed out waiting for stream error");

        runner_handle.await.unwrap();
        assert!(got_stream_error);
    }

    #[tokio::test]
    async fn test_empty_frames_mid_stream_are_skipped() {
        let test_stream = stream! {
            yield Ok(Bytes::new());
            yield Ok(Bytes::from_static(b"abc"));
            yield Ok(Bytes::new());
            yield Ok(Bytes::from_static(b"def"));
        };

        let (received, finished, failed) =
            run_runner_and_collect(Box::pin(test_stream), None).await;

        assert!(finished);
        assert!(failed.is_none());
        assert_eq!(received, b"abcdef");
    }

    #[tokio::test]
    async fn test_empty_stream() {
        let (_control_tx, control_rx) = async_channel::bounded(1);
        let cancel_token = CancellationToken::new();
        // Create an empty stream
        let test_stream = stream! {
            yield Ok(Bytes::from(vec![]));
        };

        let (mut runner, lifecycle_rx, _data_rx) = TaskRunner::builder()
            .stream(Box::pin(test_stream))
            .runner_id(1)
            .control_signal(control_rx)
            .cancel_token(cancel_token.clone())
            .build()
            .unwrap();

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        let mut got_empty_error = false;
        let mut lifecycle_rx = pin!(lifecycle_rx);
        while let Some(msg) = lifecycle_rx.next().await {
            error!("msg: {msg:?}");
            if let LifecycleEvent::Stopped(StoppedReason::Failed(e)) = msg {
                if e.is_empty() {
                    got_empty_error = true;
                    break;
                }
            }
        }

        runner_handle.await.unwrap();
        assert!(got_empty_error);
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_resize_total_larger() {
        let runner_id = 1;
        let (control_tx, control_rx) = async_channel::bounded(1);
        let cancel_token = CancellationToken::new();
        // Create a stream with known size
        let test_stream = stream! {
            yield Ok(Bytes::from(vec![1; BUFFER_SIZE]));
            sleep(Duration::from_millis(10)).await;
            yield Ok(Bytes::from(vec![2; BUFFER_SIZE]));
            sleep(Duration::from_millis(10)).await;
            yield Ok(Bytes::from(vec![3; BUFFER_SIZE]));
            sleep(Duration::from_millis(10)).await;
            yield Ok(Bytes::from(vec![4; BUFFER_SIZE]));
            sleep(Duration::from_millis(10)).await;
            yield Ok(Bytes::from(vec![5; BUFFER_SIZE]));
            sleep(Duration::from_millis(10)).await;
            yield Ok(Bytes::from(vec![6; BUFFER_SIZE]));
        };

        let expected_total = (BUFFER_SIZE as f64 * 5.5) as u64;

        let (mut runner, lifecycle_rx, data_rx) = TaskRunner::builder()
            .total(3 * BUFFER_SIZE as u64) // Initially larger than first chunk to avoid early termination
            .stream(Box::pin(test_stream))
            .runner_id(runner_id)
            .control_signal(control_rx)
            .cancel_token(cancel_token.clone())
            .build()
            .unwrap();

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        // Send resize message after first download to avoid early termination
        let mut started = false;
        let mut finished = false;
        let mut resize_sent = false;

        let mut lifecycle_rx = pin!(lifecycle_rx);
        let mut data_rx = pin!(data_rx);
        let mut data_done = false;
        loop {
            tokio::select! {
                msg = lifecycle_rx.next() => {
                    match msg {
                        Some(LifecycleEvent::Started) => {
                            started = true;
                        }
                        Some(LifecycleEvent::Stopped(StoppedReason::Finished)) => {
                            finished = true;
                            break;
                        }
                        Some(LifecycleEvent::Stopped(StoppedReason::Failed(e))) => {
                            error!("runner: stopped with error: {e:?}");
                            break;
                        }
                        None => break,
                    }
                }
                frame = data_rx.next(), if !data_done => {
                    match frame {
                        Some(_) => {
                            // Send resize message immediately after first download (only once)
                            if !resize_sent {
                                control_tx
                                    .send(ControlEvent::LimitTotal(expected_total))
                                    .await
                                    .unwrap();
                                resize_sent = true;
                            }
                        }
                        None => { data_done = true; }
                    }
                }
            }
        }

        runner_handle.await.unwrap();
        assert!(started);
        assert!(!finished, "runner should failed");
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_resize_total_smaller() {
        let (control_tx, control_rx) = async_channel::bounded(1);
        let cancel_token = CancellationToken::new();

        // Create a stream with multiple chunks that would normally total 6 chunks
        let test_stream = stream! {
            yield Ok(Bytes::from(vec![1; BUFFER_SIZE]));
            sleep(Duration::from_millis(10)).await;
            yield Ok(Bytes::from(vec![2; BUFFER_SIZE]));
            sleep(Duration::from_millis(10)).await;
            yield Ok(Bytes::from(vec![3; BUFFER_SIZE]));
            sleep(Duration::from_millis(10)).await;
            yield Ok(Bytes::from(vec![4; BUFFER_SIZE]));
            sleep(Duration::from_millis(10)).await;
            yield Ok(Bytes::from(vec![5; BUFFER_SIZE]));
            sleep(Duration::from_millis(10)).await;
            yield Ok(Bytes::from(vec![6; BUFFER_SIZE]));
        };

        let (mut runner, lifecycle_rx, data_rx) = TaskRunner::builder()
            .total(6 * BUFFER_SIZE as u64) // Initially larger size
            .stream(Box::pin(test_stream))
            .runner_id(1)
            .control_signal(control_rx)
            .cancel_token(cancel_token.clone())
            .build()
            .unwrap();

        let expected_total = (BUFFER_SIZE as f64 * 4.5) as u64;

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        // Wait for start and collect some download messages
        let mut started = false;
        let mut finished = false;
        let mut download_count = 0;
        let mut total_downloaded = 0;

        let mut lifecycle_rx = pin!(lifecycle_rx);
        let mut data_rx = pin!(data_rx);
        let mut data_done = false;
        loop {
            tokio::select! {
                msg = lifecycle_rx.next() => {
                    match msg {
                        Some(LifecycleEvent::Started) => {
                            trace!("task started");
                            started = true;
                        }
                        Some(LifecycleEvent::Stopped(StoppedReason::Finished)) => {
                            trace!("task finished");
                            finished = true;
                        }
                        Some(other) => panic!("Unexpected lifecycle event: {other:?}"),
                        None => break,
                    }
                }
                frame = data_rx.next(), if !data_done => {
                    match frame {
                        Some(frame) => {
                            download_count += 1;
                            total_downloaded += frame.data.len();
                            // After downloading 2 chunks (64 KB), resize to 4.5 chunks
                            if download_count == 2 {
                                trace!("runner: resize total to {expected_total}");
                                control_tx
                                    .send(ControlEvent::LimitTotal(expected_total))
                                    .await
                                    .unwrap();
                            }
                        }
                        None => { data_done = true; }
                    }
                }
            }

            if finished && data_done {
                break;
            }
        }

        runner_handle.await.unwrap();
        assert!(started);
        // Should have downloaded exactly 4.5 chunks
        assert_eq!(total_downloaded, expected_total as usize);
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_resize_total_smaller_with_small_chunks() {
        let (control_tx, control_rx) = async_channel::bounded(1);
        let cancel_token = CancellationToken::new();

        // Create a stream with small chunks (50 bytes each) - these will accumulate in buffer
        let test_stream = stream! {
            for i in 0..20 {
                sleep(Duration::from_millis(10)).await;
                yield Ok(Bytes::from(vec![i as u8; 50])); // 50 bytes per chunk
            }
        };

        let (mut runner, lifecycle_rx, data_rx) = TaskRunner::builder()
            .total(1000) // Initially allow 1000 bytes (20 chunks)
            .stream(Box::pin(test_stream))
            .runner_id(1)
            .control_signal(control_rx)
            .cancel_token(cancel_token.clone())
            .build()
            .unwrap();

        // Pre-schedule the control signal to be sent after 80ms
        // This should happen while the runner is actively downloading
        tokio::spawn({
            let control_tx = control_tx.clone();
            async move {
                sleep(Duration::from_millis(80)).await;
                let new_limit = 200; // 4 chunks worth
                trace!("sending limit signal to reduce total to {new_limit} bytes after 80ms");
                let _ = control_tx.send(ControlEvent::LimitTotal(new_limit)).await;
            }
        });

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        let mut started = false;
        let mut stopped = false;
        let mut total_downloaded = 0;

        let mut lifecycle_rx = pin!(lifecycle_rx);
        let mut data_rx = pin!(data_rx);
        let mut data_done = false;
        loop {
            tokio::select! {
                msg = lifecycle_rx.next() => {
                    match msg {
                        Some(LifecycleEvent::Started) => {
                            trace!("task started");
                            started = true;
                        }
                        Some(LifecycleEvent::Stopped(StoppedReason::Finished)) => {
                            trace!("task finished normally");
                            stopped = true;
                        }
                        Some(LifecycleEvent::Stopped(StoppedReason::Failed(err))) => {
                            trace!("task failed with error: {err:?}");
                            stopped = true;
                        }
                        None => break,
                    }
                }
                frame = data_rx.next(), if !data_done => {
                    match frame {
                        Some(frame) => {
                            total_downloaded += frame.data.len();
                            trace!("downloaded batch, total: {total_downloaded} bytes");
                        }
                        None => { data_done = true; }
                    }
                }
            }

            if stopped && data_done {
                break;
            }
        }

        runner_handle.await.unwrap();
        assert!(started);

        // With small chunks and buffer mechanism, we expect:
        // - Some data should be downloaded (at least a few chunks)
        // - Should not download the entire 1000 bytes due to limit signal
        // - Allow for timing variations in signal processing
        let min_expected = 100; // At least 2 chunks should be downloaded
        let max_expected = 800; // Should not download most of the stream

        assert!(
            total_downloaded >= min_expected,
            "Should have downloaded at least {min_expected} bytes, got {total_downloaded}"
        );
        assert!(
            total_downloaded <= max_expected,
            "Should not have downloaded too much after limit signal, got {total_downloaded} bytes \
             (limit was 200)"
        );

        println!("✓ Runner correctly handled resize to smaller limit with small chunks");
        println!(
            "  - Downloaded: {total_downloaded} bytes (limit was reduced to 200 bytes after 80ms)"
        );
        println!("  - Expected range: {min_expected}-{max_expected} bytes");
    }

    #[tokio::test]
    async fn test_size_mismatch() {
        let (_control_tx, control_rx) = async_channel::bounded(1);
        let cancel_token = CancellationToken::new();

        // Create a stream that produces more data than expected
        let test_stream = stream! {
            yield Ok(Bytes::from(vec![1; 10]));
            yield Ok(Bytes::from(vec![2; 10]));
        };

        let (mut runner, lifecycle_rx, data_rx) = TaskRunner::builder()
            .total(10) // Expect only 10 bytes but will receive 20
            .stream(Box::pin(test_stream))
            .runner_id(1)
            .control_signal(control_rx)
            .cancel_token(cancel_token.clone())
            .build()
            .unwrap();

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        let mut started = false;
        let mut finished = false;
        let mut total_downloaded = 0;

        let mut lifecycle_rx = pin!(lifecycle_rx);
        let mut data_rx = pin!(data_rx);
        let mut data_done = false;
        loop {
            tokio::select! {
                msg = lifecycle_rx.next() => {
                    match msg {
                        Some(LifecycleEvent::Started) => {
                            started = true;
                        }
                        Some(LifecycleEvent::Stopped(StoppedReason::Finished)) => {
                            finished = true;
                        }
                        Some(other) => panic!("Unexpected lifecycle event: {other:?}"),
                        None => break,
                    }
                }
                frame = data_rx.next(), if !data_done => {
                    match frame {
                        Some(frame) => {
                            total_downloaded += frame.data.len();
                        }
                        None => { data_done = true; }
                    }
                }
            }

            if finished && data_done {
                break;
            }
        }

        runner_handle.await.unwrap();
        assert!(started);
        assert!(finished);
        // Should have downloaded exactly 10 bytes (truncated the excess)
        assert_eq!(total_downloaded, 10);
    }

    #[tokio::test]
    async fn test_channel_closed() {
        let (control_tx, control_rx) = async_channel::bounded(1);
        let cancel_token = CancellationToken::new();

        // Create a stream that will never complete
        let test_stream = stream! {
            loop {
                sleep(Duration::from_millis(10)).await;
                yield Ok(Bytes::from(vec![1; 10]));
            }
        };

        let (mut runner, lifecycle_rx, _data_rx) = TaskRunner::builder()
            .stream(Box::pin(test_stream))
            .runner_id(1)
            .control_signal(control_rx)
            .cancel_token(cancel_token.clone())
            .build()
            .unwrap();

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        // Wait for start then drop the control channel
        let mut lifecycle_rx = pin!(lifecycle_rx);
        while let Some(msg) = lifecycle_rx.next().await {
            if let LifecycleEvent::Started = msg {
                drop(control_tx);
                break;
            }
        }

        let mut got_channel_closed = false;
        while let Some(msg) = lifecycle_rx.next().await {
            if let LifecycleEvent::Stopped(StoppedReason::Failed(e)) = msg {
                if matches!(e, TaskError::ChannelClosed) {
                    got_channel_closed = true;
                    break;
                }
            }
        }

        runner_handle.await.unwrap();
        assert!(got_channel_closed);
    }

    #[tokio::test]
    async fn test_new_with_async_and_callback_success() {
        let (_control_tx, control_rx) = async_channel::bounded(1);
        let cancel_token = CancellationToken::new();
        let runner_id = 42;

        // Create the stream directly (builder pattern replaces the async callback API)
        let test_stream = stream! {
            yield Ok(Bytes::from(vec![1; 10]));
            yield Ok(Bytes::from(vec![2; 10]));
        };

        let (mut runner, lifecycle_rx, data_rx) = TaskRunner::builder()
            .total(20)
            .stream(Box::pin(test_stream))
            .runner_id(runner_id)
            .control_signal(control_rx)
            .cancel_token(cancel_token)
            .build()
            .unwrap();

        // Spawn the runner to test it works
        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        // Verify messages work correctly
        let mut started = false;
        let mut finished = false;
        let mut total_downloaded = 0;

        let mut lifecycle_rx = pin!(lifecycle_rx);
        let mut data_rx = pin!(data_rx);
        let mut data_done = false;
        loop {
            tokio::select! {
                msg = lifecycle_rx.next() => {
                    match msg {
                        Some(LifecycleEvent::Started) => {
                            started = true;
                        }
                        Some(LifecycleEvent::Stopped(StoppedReason::Finished)) => {
                            finished = true;
                        }
                        Some(other) => panic!("Unexpected lifecycle event: {other:?}"),
                        None => break,
                    }
                }
                frame = data_rx.next(), if !data_done => {
                    match frame {
                        Some(frame) => {
                            total_downloaded += frame.data.len();
                        }
                        None => { data_done = true; }
                    }
                }
            }

            if finished && data_done {
                break;
            }
        }

        runner_handle.await.unwrap();
        assert!(started);
        assert!(finished);
        assert_eq!(total_downloaded, 20);
    }

    #[tokio::test]
    async fn test_new_with_async_and_callback_stream_failure() {
        let (_control_tx, control_rx) = async_channel::bounded(1);
        let cancel_token = CancellationToken::new();
        let runner_id = 42;

        // Create a stream that immediately errors (simulates stream creation failure)
        let test_stream = stream! {
            yield Err::<Bytes, AdapterError>(AdapterError::Unretryable {
                source: UnretryableError::Io {
                    source: Arc::new(std::io::Error::other("Mock network error")),
                },
            });
        };

        let (mut runner, lifecycle_rx, _data_rx) = TaskRunner::builder()
            .total(20)
            .stream(Box::pin(test_stream))
            .runner_id(runner_id)
            .control_signal(control_rx)
            .cancel_token(cancel_token)
            .build()
            .unwrap();

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        // Should receive a stopped message with stream error
        let mut lifecycle_rx = pin!(lifecycle_rx);
        let mut got_stream_error = false;
        while let Some(msg) = lifecycle_rx.next().await {
            if let LifecycleEvent::Stopped(StoppedReason::Failed(e)) = msg {
                if e.is_stream_error() {
                    got_stream_error = true;
                    break;
                }
            }
        }

        runner_handle.await.unwrap();
        assert!(got_stream_error);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_multiple_runners_with_independent_channels() {
        // Each runner now has its own independent control channel (point-to-point)
        let (control_tx1, control_rx1) = async_channel::bounded(10);
        let (control_tx2, control_rx2) = async_channel::bounded(10);
        let (control_tx3, control_rx3) = async_channel::bounded(10);

        let cancel_token = CancellationToken::new();

        // Create streams for all runners - each will yield 6 chunks
        let create_stream = || {
            stream! {
                for i in 0..6 {
                    sleep(Duration::from_millis(10)).await;
                    yield Ok(Bytes::from(vec![i as u8; BUFFER_SIZE]));
                }
            }
        };

        // Create three runners with different IDs
        let runner_id1 = 1;
        let runner_id2 = 2;
        let runner_id3 = 3;

        let (mut runner1, lifecycle_rx1, data_rx1) = TaskRunner::builder()
            .total(6 * BUFFER_SIZE as u64) // Total 6 chunks
            .stream(Box::pin(create_stream()))
            .runner_id(runner_id1)
            .control_signal(control_rx1)
            .cancel_token(cancel_token.clone())
            .build()
            .unwrap();

        let (mut runner2, lifecycle_rx2, data_rx2) = TaskRunner::builder()
            .total(6 * BUFFER_SIZE as u64) // Total 6 chunks
            .stream(Box::pin(create_stream()))
            .runner_id(runner_id2)
            .control_signal(control_rx2)
            .cancel_token(cancel_token.clone())
            .build()
            .unwrap();

        let (mut runner3, lifecycle_rx3, data_rx3) = TaskRunner::builder()
            .total(6 * BUFFER_SIZE as u64) // Total 6 chunks
            .stream(Box::pin(create_stream()))
            .runner_id(runner_id3)
            .control_signal(control_rx3)
            .cancel_token(cancel_token.clone())
            .build()
            .unwrap();

        // Spawn all runners
        let runner1_handle = tokio::spawn(async move {
            runner1.run().await;
        });

        let runner2_handle = tokio::spawn(async move {
            runner2.run().await;
        });

        let runner3_handle = tokio::spawn(async move {
            runner3.run().await;
        });

        // Track the state of each runner
        let mut runner1_started = false;
        let mut runner2_started = false;
        let mut runner3_started = false;

        let mut runner1_downloaded = 0;
        let mut runner2_downloaded = 0;
        let mut runner3_downloaded = 0;

        let mut runner1_finished = false;
        let mut runner2_finished = false;
        let mut runner3_finished = false;

        let mut limit_sent = false;

        // Pin the consumers for use with tokio::select!
        let mut lifecycle_rx1 = pin!(lifecycle_rx1);
        let mut lifecycle_rx2 = pin!(lifecycle_rx2);
        let mut lifecycle_rx3 = pin!(lifecycle_rx3);
        let mut data_rx1 = pin!(data_rx1);
        let mut data_rx2 = pin!(data_rx2);
        let mut data_rx3 = pin!(data_rx3);
        let mut data1_done = false;
        let mut data2_done = false;
        let mut data3_done = false;

        // Use timeout to prevent infinite waiting
        let timeout_duration = Duration::from_secs(10);
        let result = tokio::time::timeout(timeout_duration, async {
            // Use select to handle messages from all runners
            loop {
                tokio::select! {
                    msg = lifecycle_rx1.next(), if !runner1_finished => {
                        match msg {
                            Some(LifecycleEvent::Started) => {
                                runner1_started = true;
                            }
                            Some(LifecycleEvent::Stopped(reason)) => {
                                assert!(matches!(reason, StoppedReason::Finished));
                                runner1_finished = true;
                            }
                            None => {
                                runner1_finished = true;
                            }
                            _ => {}
                        }
                    }
                    frame = data_rx1.next(), if !data1_done => {
                        match frame {
                            Some(frame) => {
                                runner1_downloaded += frame.data.len();

                                // Send limit message ONLY to runner2's channel after some downloads
                                if !limit_sent && runner1_downloaded >= 20 && runner2_downloaded >= 20 {
                                    // Limit runner2 to 3 chunks - message goes directly to runner2
                                    control_tx2
                                        .send(ControlEvent::LimitTotal(3 * BUFFER_SIZE as u64))
                                        .await
                                        .unwrap();
                                    limit_sent = true;
                                }
                            }
                            None => { data1_done = true; }
                        }
                    }
                    msg = lifecycle_rx2.next(), if !runner2_finished => {
                        match msg {
                            Some(LifecycleEvent::Started) => {
                                runner2_started = true;
                            }
                            Some(LifecycleEvent::Stopped(reason)) => {
                                assert!(matches!(reason, StoppedReason::Finished));
                                runner2_finished = true;
                            }
                            None => {
                                runner2_finished = true;
                            }
                            _ => {}
                        }
                    }
                    frame = data_rx2.next(), if !data2_done => {
                        match frame {
                            Some(frame) => {
                                runner2_downloaded += frame.data.len();
                            }
                            None => { data2_done = true; }
                        }
                    }
                    msg = lifecycle_rx3.next(), if !runner3_finished => {
                        match msg {
                            Some(LifecycleEvent::Started) => {
                                runner3_started = true;
                            }
                            Some(LifecycleEvent::Stopped(reason)) => {
                                assert!(matches!(reason, StoppedReason::Finished));
                                runner3_finished = true;
                            }
                            None => {
                                runner3_finished = true;
                            }
                            _ => {}
                        }
                    }
                    frame = data_rx3.next(), if !data3_done => {
                        match frame {
                            Some(frame) => {
                                runner3_downloaded += frame.data.len();
                            }
                            None => { data3_done = true; }
                        }
                    }
                }

                // Break when all runners have stopped and their data streams are drained.
                if runner1_finished
                    && runner2_finished
                    && runner3_finished
                    && data1_done
                    && data2_done
                    && data3_done
                {
                    break;
                }
            }
        })
        .await;

        // Check if test timed out
        if result.is_err() {
            panic!(
                "Test timed out after {} seconds",
                timeout_duration.as_secs()
            );
        }

        // Wait for all runners to complete
        runner1_handle.await.unwrap();
        runner2_handle.await.unwrap();
        runner3_handle.await.unwrap();

        // Keep control channels alive until runners complete
        drop(control_tx1);
        drop(control_tx3);

        // Verify results
        assert!(runner1_started && runner2_started && runner3_started);
        assert!(runner1_finished && runner2_finished && runner3_finished);
        assert!(limit_sent);

        // Runner1 and Runner3 should have downloaded the full 6 chunks
        assert_eq!(runner1_downloaded, 6 * BUFFER_SIZE);
        assert_eq!(runner3_downloaded, 6 * BUFFER_SIZE);

        // Runner2 should have been limited to 3 chunks
        assert_eq!(runner2_downloaded, 3 * BUFFER_SIZE);

        println!("✓ Multiple runners with independent channels work correctly");
        println!("  - Runner1 downloaded: {runner1_downloaded} bytes (expected: 6 chunks)");
        println!(
            "  - Runner2 downloaded: {runner2_downloaded} bytes (expected: 3 chunks, limited)"
        );
        println!("  - Runner3 downloaded: {runner3_downloaded} bytes (expected: 6 chunks)");
    }
}
