#![doc = include_str!("../../docs/runner.md")]
use std::{cmp::Ordering, ops::ControlFlow};

use async_ringbuf::{AsyncHeapCons, AsyncHeapProd, traits::*};
use bolt_load_utils::telemetry::*;
use bytes::{Bytes, BytesMut};
use futures::{FutureExt, StreamExt, future::Either};
use smol_cancellation_token::CancellationToken;

use crate::{
    adapter::{AdapterError, AnyBytesStream},
    task::{ControlEvent, RunnerId},
};

/// Type alias for the control signal sender (point-to-point channel)
pub type ControlSignalSender = async_channel::Sender<ControlEvent>;
/// Type alias for the control signal receiver
/// Using async_channel for point-to-point communication instead of broadcast
pub type ControlSignalReceiver = async_channel::Receiver<ControlEvent>;

mod builder;
mod connector;
mod error;
pub use builder::*;
pub use connector::*;
pub use error::*;

// TODO: make it configurable or detect the local disk performance?
const BUFFER_SIZE: usize = 32 * 1024; // 32KB

/// The default period without non-empty upstream data after which a runner
/// fails with a retryable [`TaskError::Timeout`]
pub const DEFAULT_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
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
    /// How long the upstream may yield no non-empty data before failing with `Timeout`.
    idle_timeout: std::time::Duration,
    /// the cancel token
    cancel_token: CancellationToken,
}

/// The step of the event loop
enum EventLoopStep {
    /// The task is cancelled
    Cancelled,
    /// The stream yielded no non-empty data within the idle timeout
    IdleTimeout,
    /// The control signal is received (point-to-point, no need for RunnerId filtering)
    Control(Result<ControlEvent, async_channel::RecvError>),
    /// The download event is received
    Download(Option<Result<Bytes, AdapterError>>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DataDrainOutcome {
    Drained,
    Cancelled,
    TimedOut,
}

impl TaskRunner {
    /// Get the builder of the task runner
    pub fn builder() -> TaskRunnerBuilder {
        TaskRunnerBuilder::default()
    }

    /// run the task runner
    /// This function will block until the task is finished or cancelled
    /// It should be called in a new thread or async spawn context
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip(self), name = "TaskRunner::run", fields(runner_id = self.id))
    )]
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
                    TaskError::ExceededTotalSize
                        | TaskError::StreamError { .. }
                        | TaskError::Timeout,
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

    /// Apply a control signal from the manager to the size limit `total`.
    ///
    /// Since we now use point-to-point channels, the signal is guaranteed to be for this runner.
    fn apply_control(total: &mut Option<u64>, signal: &ControlEvent) -> Result<(), TaskError> {
        match signal {
            ControlEvent::LimitTotal(new_total) => {
                if let Some(current_total) = *total {
                    if current_total > *new_total {
                        trace!("runner: limit total to {new_total}");
                        *total = Some(*new_total);
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

    /// Bytes that may still be buffered before reaching the size limit.
    fn allowance(&self, buff: &BytesMut) -> usize {
        self.total.map_or(usize::MAX, |total| {
            total.saturating_sub(self.downloaded + buff.len() as u64) as usize
        })
    }

    /// Push `data` downstream, applying the control signals that arrive meanwhile.
    ///
    /// Returns `Err` when the push fails or the runner is cancelled, and
    /// `Ok(Some(_))` when the push succeeded but a control signal failed
    /// meanwhile; only the first such error is kept. The stream is not polled
    /// while pushing, so waiting for the consumer does not count as idle.
    async fn emit(&mut self, data: Bytes) -> Result<Option<TaskRunError>, TaskRunError> {
        let len = data.len() as u64;
        let Self {
            total,
            downloaded,
            control_signal,
            data_tx,
            cancel_token,
            ..
        } = self;
        let data_tx = data_tx.as_mut().ok_or(TaskError::ChannelClosed)?;
        let push = data_tx.push(DataFrame { data });
        let cancelled = cancel_token.cancelled().fuse();
        futures::pin_mut!(push, cancelled);
        let mut deferred = None;
        let mut control_open = true;
        loop {
            let control = if control_open {
                Either::Left(control_signal.recv())
            } else {
                Either::Right(futures::future::pending())
            }
            .fuse();
            futures::pin_mut!(control);
            futures::select_biased! {
                _ = cancelled => return Err(TaskRunError::Cancelled),
                signal = control => {
                    let err = match signal {
                        Ok(signal) => Self::apply_control(total, &signal).err().map(Into::into),
                        Err(_) => {
                            // The manager has released this runner
                            control_open = false;
                            Some(TaskError::ChannelClosed.into())
                        }
                    };
                    match err {
                        Some(err) if deferred.is_some() => {
                            warn!("discarding control error during push: {err:?}");
                        }
                        err => deferred = deferred.or(err),
                    }
                }
                res = push => {
                    res.map_err(|_| TaskError::ChannelClosed)?;
                    *downloaded += len;
                    return Ok(deferred);
                }
            }
        }
    }

    /// Push the buffered bytes, if any. See [`Self::emit`] for the result.
    async fn flush(&mut self, buff: &mut BytesMut) -> Result<Option<TaskRunError>, TaskRunError> {
        if buff.is_empty() {
            return Ok(None);
        }
        self.emit(buff.split().freeze()).await
    }

    /// Push the buffered bytes, failing with a control error deferred by the push.
    async fn flush_checked(&mut self, buff: &mut BytesMut) -> Result<(), TaskRunError> {
        match self.flush(buff).await? {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Buffer a stream item, pushing every full buffer downstream.
    ///
    /// The size limit is re-read after each push, since the manager may lower
    /// it meanwhile; bytes already buffered are kept.
    async fn buffer_item(
        &mut self,
        item: Bytes,
        buff: &mut BytesMut,
        idle_timer: &mut async_io::Timer,
    ) -> Result<ControlFlow<()>, TaskRunError> {
        let mut offset = 0;
        loop {
            let allowed = self.allowance(buff);
            if allowed == 0 {
                self.flush_checked(buff).await?;
                return Ok(ControlFlow::Break(()));
            }
            if offset == item.len() {
                return Ok(ControlFlow::Continue(()));
            }
            let writable = (item.len() - offset).min(allowed);
            if buff.len() + writable <= BUFFER_SIZE {
                buff.extend_from_slice(&item[offset..offset + writable]);
                if writable == allowed {
                    self.flush_checked(buff).await?;
                    return Ok(ControlFlow::Break(()));
                }
                return Ok(ControlFlow::Continue(()));
            }
            let take = BUFFER_SIZE - buff.len();
            buff.extend_from_slice(&item[offset..offset + take]);
            offset += take;
            self.flush_checked(buff).await?;
            // Backpressure is not idleness.
            idle_timer.set_after(self.idle_timeout);
        }
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

        let mut buff = BytesMut::with_capacity(BUFFER_SIZE);
        let idle_timeout = self.idle_timeout;
        let mut idle_timer = async_io::Timer::after(idle_timeout);
        loop {
            let step = {
                let cancelled = self.cancel_token.cancelled().fuse();
                let control_signal = self.control_signal.recv().fuse();
                let download = self.stream.next().fuse();
                let idle = FutureExt::fuse(&mut idle_timer);
                futures::pin_mut!(cancelled, control_signal, download, idle);
                futures::select_biased! {
                    _ = cancelled => EventLoopStep::Cancelled,
                    signal = control_signal => EventLoopStep::Control(signal),
                    _ = idle => EventLoopStep::IdleTimeout,
                    result = download => EventLoopStep::Download(result),
                }
            };

            match step {
                EventLoopStep::Cancelled => {
                    trace!("runner: cancelled");
                    return Err(TaskRunError::Cancelled);
                }
                EventLoopStep::Control(Ok(signal)) => {
                    trace!("runner: control signal: {signal:?}");
                    Self::apply_control(&mut self.total, &signal)?;
                }
                EventLoopStep::Control(Err(_)) => {
                    // Control channel closed - manager has released this runner
                    trace!("runner: control signal channel closed");
                    return Err(TaskError::ChannelClosed.into());
                }
                EventLoopStep::IdleTimeout => {
                    warn!("runner: no data received in the last {idle_timeout:?}");
                    self.flush_checked(&mut buff).await?;
                    return Err(TaskError::Timeout.into());
                }
                EventLoopStep::Download(None) => {
                    self.flush_checked(&mut buff).await?;
                    break;
                }
                EventLoopStep::Download(Some(Err(source))) => {
                    // The upstream error wins over a control error deferred by the push.
                    let _ = self.flush(&mut buff).await?;
                    return Err(TaskError::StreamError { source }.into());
                }
                EventLoopStep::Download(Some(Ok(item))) => {
                    if !item.is_empty() {
                        idle_timer.set_after(idle_timeout);
                    }
                    if self
                        .buffer_item(item, &mut buff, &mut idle_timer)
                        .await?
                        .is_break()
                    {
                        break;
                    }
                }
            }
        }

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
            match lifecycle_rx.next().await {
                Some(LifecycleEvent::Started) => {}
                Some(other) => panic!("unexpected lifecycle event before start: {other:?}"),
                None => panic!("lifecycle channel closed before start"),
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
            match lifecycle_rx.next().await {
                Some(LifecycleEvent::Started) => {}
                Some(other) => panic!("unexpected lifecycle event before start: {other:?}"),
                None => panic!("lifecycle channel closed before start"),
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
            while let Some(LifecycleEvent::Started) = lifecycle_rx.next().await {}
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
            if let LifecycleEvent::Stopped(StoppedReason::Failed(e)) = msg
                && e.is_empty()
            {
                got_empty_error = true;
                break;
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
            if let LifecycleEvent::Stopped(StoppedReason::Failed(e)) = msg
                && matches!(e, TaskError::ChannelClosed)
            {
                got_channel_closed = true;
                break;
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
            if let LifecycleEvent::Stopped(StoppedReason::Failed(e)) = msg
                && e.is_stream_error()
            {
                got_stream_error = true;
                break;
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

    struct SpawnedRunner {
        control_tx: async_channel::Sender<ControlEvent>,
        lifecycle_rx: LifecycleReceiver,
        data_rx: DataFrameReceiver,
        handle: tokio::task::JoinHandle<()>,
    }

    async fn spawn_started_runner(stream: AnyBytesStream, total: Option<u64>) -> SpawnedRunner {
        let (control_tx, control_rx) = async_channel::bounded(4);
        let mut builder = TaskRunner::builder()
            .stream(stream)
            .runner_id(1)
            .control_signal(control_rx)
            .cancel_token(CancellationToken::new());
        if let Some(total) = total {
            builder = builder.total(total);
        }
        let (mut runner, mut lifecycle_rx, data_rx) = builder.build().unwrap();
        let handle = tokio::spawn(async move { runner.run().await });
        let started = timeout(Duration::from_secs(2), lifecycle_rx.next())
            .await
            .expect("timed out waiting for runner start");
        assert!(matches!(started, Some(LifecycleEvent::Started)));
        SpawnedRunner {
            control_tx,
            lifecycle_rx,
            data_rx,
            handle,
        }
    }

    /// Drain data frames until the runner stops, returning the received byte
    /// count and the stop reason.
    async fn drain_until_stopped(
        lifecycle_rx: &mut LifecycleReceiver,
        data_rx: &mut DataFrameReceiver,
    ) -> (usize, StoppedReason) {
        let mut received = 0;
        let mut data_done = false;
        let mut reason = None;
        timeout(Duration::from_secs(5), async {
            while !(data_done && reason.is_some()) {
                tokio::select! {
                    frame = data_rx.next(), if !data_done => match frame {
                        Some(frame) => received += frame.data.len(),
                        None => data_done = true,
                    },
                    event = lifecycle_rx.next(), if reason.is_none() => match event {
                        Some(LifecycleEvent::Stopped(r)) => reason = Some(r),
                        Some(LifecycleEvent::Started) => {}
                        None => panic!("lifecycle channel closed before Stopped"),
                    },
                }
            }
        })
        .await
        .expect("timed out draining runner output");
        (received, reason.unwrap())
    }

    /// Yields `frames` full buffers, signals, then yields `tail`.
    ///
    /// The runner forwards a buffer only when the next item arrives, so once
    /// `frames == DATA_FRAME_CHANNEL_CAPACITY + 1` the data channel is full
    /// and handling `tail` blocks on pushing the last buffer. On a
    /// current-thread runtime the runner is parked there before the signal is
    /// observed.
    fn blocking_stream(
        frames: usize,
        signal_tx: async_channel::Sender<()>,
        tail: Result<Bytes, AdapterError>,
    ) -> AnyBytesStream {
        Box::pin(stream! {
            for i in 0..frames {
                yield Ok(Bytes::from(vec![i as u8; BUFFER_SIZE]));
            }
            let _ = signal_tx.try_send(());
            yield tail;
            std::future::pending::<()>().await;
        })
    }

    #[tokio::test]
    async fn test_stream_error_wins_over_deferred_control_error() {
        let frames = DATA_FRAME_CHANNEL_CAPACITY + 1;
        let total = ((frames + 8) * BUFFER_SIZE) as u64;
        let (signal_tx, signal_rx) = async_channel::unbounded();
        let stream = blocking_stream(frames, signal_tx, Err(stream_io_error("boom")));
        let mut runner = spawn_started_runner(stream, Some(total)).await;

        signal_rx.recv().await.unwrap();
        // Not a reduction, so it is rejected; the error is deferred until the push completes.
        runner
            .control_tx
            .send(ControlEvent::LimitTotal(total))
            .await
            .unwrap();
        tokio::task::yield_now().await;

        let (received, reason) =
            drain_until_stopped(&mut runner.lifecycle_rx, &mut runner.data_rx).await;
        runner.handle.await.unwrap();
        assert!(
            matches!(&reason, StoppedReason::Failed(err) if err.is_stream_error()),
            "got {reason:?}"
        );
        assert_eq!(received, frames * BUFFER_SIZE);
    }

    #[tokio::test]
    async fn test_push_failure_wins_over_stream_error() {
        let frames = DATA_FRAME_CHANNEL_CAPACITY + 1;
        let total = ((frames + 8) * BUFFER_SIZE) as u64;
        let (signal_tx, signal_rx) = async_channel::unbounded();
        let stream = blocking_stream(frames, signal_tx, Err(stream_io_error("boom")));
        let SpawnedRunner {
            control_tx,
            mut lifecycle_rx,
            data_rx,
            handle,
        } = spawn_started_runner(stream, Some(total)).await;

        signal_rx.recv().await.unwrap();
        control_tx
            .send(ControlEvent::LimitTotal(total))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        drop(data_rx);

        let stopped = timeout(Duration::from_secs(5), lifecycle_rx.next())
            .await
            .expect("timed out waiting for runner stop");
        handle.await.unwrap();
        assert!(
            matches!(
                stopped,
                Some(LifecycleEvent::Stopped(StoppedReason::Failed(
                    TaskError::ChannelClosed
                )))
            ),
            "got {stopped:?}"
        );
    }

    #[tokio::test]
    async fn test_first_deferred_control_error_is_preserved() {
        let frames = DATA_FRAME_CHANNEL_CAPACITY + 1;
        let total = ((frames + 8) * BUFFER_SIZE) as u64;
        let (signal_tx, signal_rx) = async_channel::unbounded();
        let tail = Ok(Bytes::from(vec![0xff; BUFFER_SIZE]));
        let stream = blocking_stream(frames, signal_tx, tail);
        let SpawnedRunner {
            control_tx,
            mut lifecycle_rx,
            mut data_rx,
            handle,
        } = spawn_started_runner(stream, Some(total)).await;

        signal_rx.recv().await.unwrap();
        control_tx
            .send(ControlEvent::LimitTotal(total))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        // A second error while the push is still blocked must not replace the first.
        drop(control_tx);
        tokio::task::yield_now().await;

        let (received, reason) = drain_until_stopped(&mut lifecycle_rx, &mut data_rx).await;
        handle.await.unwrap();
        assert!(
            matches!(reason, StoppedReason::Failed(TaskError::ExceededTotalSize)),
            "got {reason:?}"
        );
        assert_eq!(received, frames * BUFFER_SIZE);
    }

    #[tokio::test]
    async fn test_limit_total_equal_is_rejected() {
        let stream = Box::pin(futures::stream::pending::<Result<Bytes, AdapterError>>());
        let mut runner = spawn_started_runner(stream, Some(100)).await;

        runner
            .control_tx
            .send(ControlEvent::LimitTotal(100))
            .await
            .unwrap();

        let (received, reason) =
            drain_until_stopped(&mut runner.lifecycle_rx, &mut runner.data_rx).await;
        runner.handle.await.unwrap();
        assert!(
            matches!(reason, StoppedReason::Failed(TaskError::ExceededTotalSize)),
            "got {reason:?}"
        );
        assert_eq!(received, 0);
    }

    #[tokio::test]
    async fn test_limit_total_unknown_is_ignored() {
        let (release_tx, release_rx) = async_channel::bounded::<()>(1);
        let stream = Box::pin(stream! {
            let _ = release_rx.recv().await;
            yield Ok(Bytes::from(vec![7; 20]));
        });
        let mut runner = spawn_started_runner(stream, None).await;

        runner
            .control_tx
            .send(ControlEvent::LimitTotal(10))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        release_tx.close();

        let (received, reason) =
            drain_until_stopped(&mut runner.lifecycle_rx, &mut runner.data_rx).await;
        runner.handle.await.unwrap();
        assert!(matches!(reason, StoppedReason::Finished), "got {reason:?}");
        assert_eq!(received, 20);
    }

    fn idle_runner(
        total: Option<u64>,
    ) -> (
        TaskRunner,
        async_channel::Sender<ControlEvent>,
        CancellationToken,
        DataFrameReceiver,
    ) {
        let (control_tx, control_rx) = async_channel::bounded(4);
        let token = CancellationToken::new();
        let mut builder = TaskRunner::builder()
            .stream(Box::pin(futures::stream::pending()))
            .runner_id(1)
            .control_signal(control_rx)
            .cancel_token(token.clone());
        if let Some(total) = total {
            builder = builder.total(total);
        }
        let (runner, _lifecycle_rx, data_rx) = builder.build().unwrap();
        (runner, control_tx, token, data_rx)
    }

    async fn fill_data_channel(runner: &mut TaskRunner) {
        for _ in 0..DATA_FRAME_CHANNEL_CAPACITY {
            assert!(matches!(
                runner.emit(Bytes::from_static(b"x")).await,
                Ok(None)
            ));
        }
    }

    #[tokio::test]
    async fn test_emit_waits_for_backpressure() {
        let (mut runner, _control_tx, _token, mut data_rx) = idle_runner(None);
        fill_data_channel(&mut runner).await;

        {
            let mut emit = pin!(runner.emit(Bytes::from_static(b"y")));
            assert!(futures::poll!(emit.as_mut()).is_pending());
            assert_eq!(&data_rx.next().await.unwrap().data[..], b"x");
            assert!(matches!(emit.await, Ok(None)));
        }
        assert_eq!(runner.downloaded, DATA_FRAME_CHANNEL_CAPACITY as u64 + 1);

        drop(runner);
        let mut expected = vec![Bytes::from_static(b"x"); DATA_FRAME_CHANNEL_CAPACITY - 1];
        expected.push(Bytes::from_static(b"y"));
        assert_eq!(
            data_rx.map(|frame| frame.data).collect::<Vec<_>>().await,
            expected
        );
    }

    #[tokio::test]
    async fn test_emit_fails_when_receiver_closes() {
        let (mut runner, _control_tx, _token, data_rx) = idle_runner(None);
        fill_data_channel(&mut runner).await;

        {
            let mut emit = pin!(runner.emit(Bytes::from_static(b"y")));
            assert!(futures::poll!(emit.as_mut()).is_pending());
            drop(data_rx);
            assert!(matches!(
                emit.await,
                Err(TaskRunError::Failed(TaskError::ChannelClosed))
            ));
        }
        assert_eq!(runner.downloaded, DATA_FRAME_CHANNEL_CAPACITY as u64);
    }

    #[tokio::test]
    async fn test_cancel_during_blocked_emit() {
        let (mut runner, _control_tx, token, _data_rx) = idle_runner(None);
        fill_data_channel(&mut runner).await;

        let mut emit = pin!(runner.emit(Bytes::from_static(b"y")));
        assert!(futures::poll!(emit.as_mut()).is_pending());
        token.cancel();
        assert!(matches!(emit.await, Err(TaskRunError::Cancelled)));
    }

    #[tokio::test]
    async fn test_limit_reduction_preserves_staged_frame() {
        let frames = DATA_FRAME_CHANNEL_CAPACITY + 1;
        let total = ((frames + 8) * BUFFER_SIZE) as u64;
        let (signal_tx, signal_rx) = async_channel::unbounded();
        let tail = Ok(Bytes::from(vec![0xff; BUFFER_SIZE]));
        let stream = blocking_stream(frames, signal_tx, tail);
        let mut runner = spawn_started_runner(stream, Some(total)).await;

        signal_rx.recv().await.unwrap();
        // Lower the limit below the end of the frame being pushed.
        let limit = ((frames - 1) * BUFFER_SIZE + BUFFER_SIZE / 2) as u64;
        runner
            .control_tx
            .send(ControlEvent::LimitTotal(limit))
            .await
            .unwrap();
        tokio::task::yield_now().await;

        let (received, reason) =
            drain_until_stopped(&mut runner.lifecycle_rx, &mut runner.data_rx).await;
        runner.handle.await.unwrap();
        assert_eq!(received, frames * BUFFER_SIZE, "the staged frame is kept");
        assert!(
            matches!(reason, StoppedReason::Failed(TaskError::ExceededTotalSize)),
            "got {reason:?}"
        );
    }

    #[tokio::test]
    async fn test_idle_timeout_pauses_during_emit() {
        let frames = DATA_FRAME_CHANNEL_CAPACITY + 2;
        let idle_timeout = Duration::from_millis(100);
        let (signal_tx, signal_rx) = async_channel::unbounded();
        let stream = Box::pin(stream! {
            for i in 0..frames {
                if i == frames - 1 {
                    let _ = signal_tx.try_send(());
                }
                yield Ok(Bytes::from(vec![i as u8; BUFFER_SIZE]));
            }
        });
        let (control_tx, control_rx) = async_channel::bounded(1);
        let (mut runner, mut lifecycle_rx, mut data_rx) = TaskRunner::builder()
            .stream(stream)
            .runner_id(1)
            .control_signal(control_rx)
            .cancel_token(CancellationToken::new())
            .idle_timeout(idle_timeout)
            .build()
            .unwrap();
        let handle = tokio::spawn(async move { runner.run().await });

        signal_rx.recv().await.unwrap();
        // Blocked on the full data channel for longer than the idle timeout.
        sleep(idle_timeout * 3).await;

        let (received, reason) = drain_until_stopped(&mut lifecycle_rx, &mut data_rx).await;
        handle.await.unwrap();
        drop(control_tx);
        assert!(matches!(reason, StoppedReason::Finished), "got {reason:?}");
        assert_eq!(received, frames * BUFFER_SIZE);
    }

    #[tokio::test]
    async fn test_empty_frames_do_not_reset_idle_timeout() {
        let stream = Box::pin(stream! {
            yield Ok(Bytes::from_static(b"abc"));
            loop {
                sleep(Duration::from_millis(10)).await;
                yield Ok(Bytes::new());
            }
        });
        let (control_tx, control_rx) = async_channel::bounded(1);
        let (mut runner, mut lifecycle_rx, mut data_rx) = TaskRunner::builder()
            .stream(stream)
            .runner_id(1)
            .control_signal(control_rx)
            .cancel_token(CancellationToken::new())
            .idle_timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let handle = tokio::spawn(async move { runner.run().await });

        let (received, reason) = drain_until_stopped(&mut lifecycle_rx, &mut data_rx).await;
        handle.await.unwrap();
        drop(control_tx);
        assert!(
            matches!(reason, StoppedReason::Failed(TaskError::Timeout)),
            "got {reason:?}"
        );
        assert_eq!(received, 3, "buffered bytes are flushed before failing");
    }
}
