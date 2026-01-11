use std::cmp::Ordering;

use async_broadcast::Receiver as BroadcastReceiver;
use async_channel::{Receiver, Sender};
use bolt_load_utils::telemetry::*;
use bytes::{Bytes, BytesMut};
use futures::{FutureExt, StreamExt};
use smol_cancellation_token::CancellationToken;

use crate::{
    DEFAULT_EVENT_CHANNEL_CAPACITY,
    adapter::{AdapterError, AnyBytesStream},
    task::{ManagerMessage, ManagerMessagesVariant, RunnerId},
    utils::ShutdownGuardExt,
};

mod guard;
pub use guard::*;

// TODO: make it configurable or detect the local disk performance?
const BUFFER_SIZE: usize = 32 * 1024; // 32KB

/// The timeout for the slow stream
const SLOW_STREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// messages for runner -> manager
#[derive(Debug)]
pub struct RunnerMessage(pub RunnerId, pub RunnerMessageKind);

#[derive(Debug)]
pub enum RunnerMessageKind {
    /// The task is started
    Started,
    /// Stopped with message
    Stopped(StoppedReason),
    /// Downloaded a chunk
    Downloaded(Bytes),
}

/// The reason why the task is stopped
#[derive(Debug, Clone)]
pub enum StoppedReason {
    /// The task is finished
    Finished,
    /// The task is failed
    Failed(TaskFailedKind),
}

/// The kind of the task failed
#[derive(Debug, Clone)]
pub enum TaskFailedKind {
    /// The task is cancelled
    Cancelled,
    /// The task is timeout, only happen when a stream is not sent in a period
    Timeout,
    /// The task is empty
    Empty,
    /// The channel is closed
    ChannelClosed,
    /// The task is exceeded the total size
    ///
    /// Possible reason:
    /// - The total sized while the downloaded chunk is larger than the total size
    ExceededTotalSize,
    /// The task is smaller than the total size
    ///
    /// Possible reason:
    /// - The total sized while the downloaded chunk is smaller than the total size
    SmallerThanTotalSize,
    StreamError(AdapterError),
    /// The other error
    Other(String),
}

#[derive(Clone)]
/// a wrapper of the task message sender
struct RunnerMessageSender(RunnerId, Sender<RunnerMessage>);

impl RunnerMessageSender {
    pub fn new(task_id: RunnerId, sender: Sender<RunnerMessage>) -> Self {
        Self(task_id, sender)
    }

    fn runner_id(&self) -> RunnerId {
        self.0
    }

    pub async fn send(
        &self,
        message: RunnerMessageKind,
    ) -> Result<(), async_channel::SendError<RunnerMessage>> {
        self.1.send(RunnerMessage(self.0, message)).await
    }
}

#[derive(Debug)]
enum TaskRunError {
    Cancelled,
    Failed(TaskFailedKind),
}

impl From<TaskFailedKind> for TaskRunError {
    fn from(value: TaskFailedKind) -> Self {
        Self::Failed(value)
    }
}

/// runner for each chunk, or single file, responsible for downloading each chunk
pub struct TaskRunner {
    /// The total size of the this chunk or file
    /// possible None if the total size is unknown
    total: Option<u64>,
    /// the downloaded size
    downloaded: u64,
    /// the adapter of the task
    stream: AnyBytesStream,
    /// the receiver of the manager messages
    control_signal: BroadcastReceiver<ManagerMessage>,
    /// the sender of the task messages
    notify: RunnerMessageSender,
    /// the cancel token
    cancel_token: CancellationToken,
    /// the shutdown signal, used for ensure the task runner is stopped
    shutdown_rx: Option<oneshot::Receiver<()>>,
}

enum Event {
    /// The task is cancelled
    Cancelled,
    /// The stream is too slow, and transfer 0 bytes in the last SLOW_STREAM_TIMEOUT
    SlowTransfer,
    /// The control signal is received
    Control(Result<ManagerMessage, async_broadcast::RecvError>),
    /// The download event is received
    Download(Option<Result<Bytes, AdapterError>>),
}

impl TaskRunner {
    pub fn new(
        total: Option<u64>,
        stream: AnyBytesStream,
        runner_id: RunnerId,
        receiver: BroadcastReceiver<ManagerMessage>,
        cancel_token: CancellationToken,
    ) -> (Self, Receiver<RunnerMessage>) {
        let (tx, rx) = async_channel::bounded(DEFAULT_EVENT_CHANNEL_CAPACITY);
        (
            TaskRunner {
                total,
                downloaded: 0,
                stream,
                notify: RunnerMessageSender::new(runner_id, tx),
                control_signal: receiver,
                cancel_token,
                shutdown_rx: None,
            },
            rx,
        )
    }

    pub async fn new_with_async_and_callback(
        total: Option<u64>,
        stream: impl Future<Output = Result<AnyBytesStream, AdapterError>>,
        runner_id: RunnerId,
        receiver: BroadcastReceiver<ManagerMessage>,
        cancel_token: CancellationToken,
        on_channel_created: impl FnOnce(Receiver<RunnerMessage>),
    ) -> Option<Self> {
        let (tx, rx) = async_channel::bounded(DEFAULT_EVENT_CHANNEL_CAPACITY);
        on_channel_created(rx);
        let stream = match stream.await {
            Ok(stream) => stream,
            Err(e) => {
                let _ = tx
                    .send(RunnerMessage(
                        runner_id,
                        RunnerMessageKind::Stopped(StoppedReason::Failed(
                            TaskFailedKind::StreamError(e),
                        )),
                    ))
                    .await;
                return None;
            }
        };
        Some(TaskRunner {
            total,
            downloaded: 0,
            stream,
            notify: RunnerMessageSender::new(runner_id, tx),
            control_signal: receiver,
            cancel_token,
            shutdown_rx: None,
        })
    }

    /// run the task runner
    /// This function will block until the task is finished or cancelled
    /// It should be called in a new thread or async spawn context
    #[cfg(feature = "tracing")]
    #[tracing::instrument(skip(self), name = "TaskRunner::run", fields(runner_id = self.notify.runner_id()))]
    pub async fn run(&mut self) {
        match self.run_inner().await {
            Ok(_) => {
                let _ = self
                    .notify
                    .send(RunnerMessageKind::Stopped(StoppedReason::Finished))
                    .await;
            }
            Err(err) => match err {
                TaskRunError::Cancelled => {
                    let _ = self
                        .notify
                        .send(RunnerMessageKind::Stopped(StoppedReason::Failed(
                            TaskFailedKind::Cancelled,
                        )))
                        .await;
                }
                TaskRunError::Failed(failed_kind) => {
                    let _ = self
                        .notify
                        .send(RunnerMessageKind::Stopped(StoppedReason::Failed(
                            failed_kind,
                        )))
                        .await;
                }
            },
        }
    }

    /// Handle the control signal from the manager
    #[cfg_attr(feature = "tracing", tracing::instrument(
        skip(self),
        name = "TaskRunner::handle_control_signal",
        fields(runner_id = self.notify.runner_id())
    ))]
    fn handle_control_signal(&mut self, signal: &ManagerMessage) -> Result<(), TaskFailedKind> {
        // Task layer owned the cancel token guard, ensure cancel token when manager is dropping
        match signal {
            ManagerMessage(runner_id, variant) if *runner_id == self.notify.runner_id() => {
                match variant {
                    ManagerMessagesVariant::LimitTotal(new_total) => {
                        if let Some(current_total) = self.total {
                            if current_total > *new_total {
                                trace!("runner: limit total to {new_total}");
                                self.total = Some(*new_total);
                            } else {
                                warn!(
                                    "runner: limit total is larger than current total,
                                    limit: {new_total}, current: {current_total}"
                                );
                                Err(TaskFailedKind::ExceededTotalSize)?;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn flush_buff(&mut self, buff: &mut BytesMut) -> Result<(), TaskFailedKind> {
        if !buff.is_empty() {
            self.downloaded += buff.len() as u64;
            let chunk = buff.split().freeze();
            self.notify
                .send(RunnerMessageKind::Downloaded(chunk))
                .await
                .map_err(|_| TaskFailedKind::ChannelClosed)?;
            buff.clear();
        }
        Ok(())
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
        fields(runner_id = self.notify.runner_id())
    ))]
    async fn handle_stream_event(
        &mut self,
        event: Option<Result<Bytes, AdapterError>>,
        buff: &mut BytesMut,
        is_finished: &mut bool,
    ) -> Result<(), TaskFailedKind> {
        match event {
            // TODO: check boundary after
            Some(Ok(item)) => {
                if let Some(total) = self.total {
                    let current_downloaded = self.downloaded + buff.len() as u64;
                    if current_downloaded >= total {
                        self.flush_buff(buff).await?;
                        *is_finished = true;
                        return Ok(());
                    }
                }

                let bytes_to_write = if let Some(total) = self.total {
                    let current_downloaded = self.downloaded + buff.len() as u64;
                    let remaining = total.saturating_sub(current_downloaded);
                    item.len().min(remaining as usize)
                } else {
                    item.len()
                };

                if bytes_to_write == 0 {
                    self.flush_buff(buff).await?;
                    *is_finished = true;
                    return Ok(());
                }

                if buff.len() + bytes_to_write > BUFFER_SIZE {
                    self.flush_buff(buff).await?;
                }

                buff.extend_from_slice(&item[..bytes_to_write]);

                if let Some(total) = self.total {
                    let current_downloaded = self.downloaded + buff.len() as u64;
                    if current_downloaded >= total {
                        self.flush_buff(buff).await?;
                        *is_finished = true;
                    }
                }
            }
            // TODO: add a retry logic?
            // First, we have to clarify whether this error is recoverable
            // If it is, we can retry it
            // If it is not, we should just return the error, and terminate the task
            Some(Err(err)) => {
                self.flush_buff(buff).await?;
                return Err(TaskFailedKind::StreamError(err));
            }
            // In this case, the download is closed, which means the stream is finished
            None => {
                self.flush_buff(buff).await?;
                *is_finished = true;
            }
        }
        Ok(())
    }

    /// The inner logic of the task runner
    /// Just wrap a Result<(), TaskFailedKind> to return the error kind
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip(self), name = "TaskRunner::run_inner", fields(runner_id = self.notify.runner_id()))
    )]
    async fn run_inner(&mut self) -> Result<(), TaskRunError> {
        trace!("runner: started");
        self.notify
            .send(RunnerMessageKind::Started)
            .await
            .map_err(|_| TaskRunError::Failed(TaskFailedKind::ChannelClosed))?;

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let _guard = shutdown_tx.shutdown_guard();
        self.shutdown_rx = Some(shutdown_rx);
        let mut buff = BytesMut::with_capacity(BUFFER_SIZE);
        let mut is_finished = false;
        let mut timer = async_io::Timer::after(SLOW_STREAM_TIMEOUT);
        let mut now = std::time::Instant::now();
        let result = loop {
            let step: Event = async {
                let control_signal = self.control_signal.recv().fuse();
                let download = self.stream.next().fuse();
                let cancelled = self.cancel_token.cancelled().fuse();
                let slow_transfer = timer.next().fuse();
                futures::pin_mut!(control_signal, download, cancelled, slow_transfer);
                futures::select_biased! {
                    _ = cancelled => {
                        Event::Cancelled
                    }
                    signal = control_signal => {
                        Event::Control(signal)
                    }
                    _ = slow_transfer => {
                        timer.set_after(SLOW_STREAM_TIMEOUT);
                        Event::SlowTransfer
                    }
                    result = download => {
                        timer.set_after(SLOW_STREAM_TIMEOUT);
                        Event::Download(result)
                    },
                }
            }
            .await;

            match step {
                Event::Cancelled => {
                    trace!("runner: cancelled");
                    break Err(TaskRunError::Cancelled);
                }
                Event::Control(signal) => match signal {
                    Ok(signal) => {
                        trace!("runner: control signal: {signal:?}");
                        if let Err(err) = self.handle_control_signal(&signal) {
                            break Err(err.into());
                        }
                        trace!(
                            "current downloaded: {}",
                            self.downloaded as usize + buff.len()
                        );
                    }
                    Err(_) => {
                        trace!("runner: control signal channel closed");
                        break Err(TaskFailedKind::ChannelClosed.into());
                    }
                },
                Event::SlowTransfer => {
                    warn!(
                        "runner: very slow stream, transfer 0 bytes in the last {} seconds",
                        SLOW_STREAM_TIMEOUT.as_secs()
                    );
                }
                Event::Download(item) => {
                    match self
                        .handle_stream_event(item, &mut buff, &mut is_finished)
                        .await
                    {
                        Ok(()) => {
                            if is_finished {
                                trace!("runner: finished");
                                break Ok(());
                            }
                        }
                        Err(err) => {
                            break Err(err.into());
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
                    return Err(TaskFailedKind::SmallerThanTotalSize.into());
                }
                Ordering::Less => {
                    warn!(
                        "runner: downloaded content is larger than the total size, total: {}, \
                         downloaded: {}",
                        total, self.downloaded
                    );
                    return Err(TaskFailedKind::ExceededTotalSize.into());
                }
            },
            None if self.downloaded > 0 => {}
            None => {
                return Err(TaskFailedKind::Empty.into());
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
    use std::{sync::Arc, time::Duration};

    use async_stream::stream;
    use oneshot;
    use pretty_assertions::assert_eq;
    use tokio::time::sleep;

    use super::*;
    use crate::adapter::{AdapterError, UnretryableError};

    #[tokio::test]
    async fn test_normal_download() {
        let (_control_tx, control_rx) = async_broadcast::broadcast(1);
        let token = CancellationToken::new();

        // Create a stream that emits 3 chunks
        let test_stream = stream! {
            for i in 0..3 {
                yield Ok(Bytes::from(vec![i as u8; 10]));
            }
        };

        let (mut runner, msg_rx) = TaskRunner::new(
            Some(30), // Total size: 3 chunks * 10 bytes
            Box::pin(test_stream),
            1,
            control_rx,
            token,
        );

        // Spawn the runner
        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        // Collect all messages
        let mut downloaded_size = 0;
        let mut started = false;
        let mut finished = false;

        while let Ok(msg) = msg_rx.recv().await {
            debug!("msg: {msg:?}");
            match msg {
                RunnerMessage(_, RunnerMessageKind::Started) => {
                    started = true;
                }
                RunnerMessage(_, RunnerMessageKind::Downloaded(bytes)) => {
                    downloaded_size += bytes.len();
                }
                RunnerMessage(_, RunnerMessageKind::Stopped(StoppedReason::Finished)) => {
                    finished = true;
                    break;
                }
                _ => panic!("Unexpected message"),
            }
        }

        runner_handle.await.unwrap();
        assert!(started);
        assert!(finished);
        assert_eq!(downloaded_size, 30);
    }

    #[tokio::test]
    async fn test_cancel_download() {
        let (_control_tx, control_rx) = async_broadcast::broadcast(1);
        let cancel_token = CancellationToken::new();

        // Create an infinite stream that we'll cancel
        let test_stream = stream! {
            loop {
                sleep(Duration::from_millis(10)).await;
                yield Ok(Bytes::from(vec![1; 10]));
            }
        };

        let (mut runner, msg_rx) = TaskRunner::new(
            None,
            Box::pin(test_stream),
            1,
            control_rx,
            cancel_token.clone(),
        );

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        // Wait for the Started message
        let mut started = false;
        while let Ok(msg) = msg_rx.recv().await {
            if let RunnerMessage(_, RunnerMessageKind::Started) = msg {
                started = true;
                break;
            }
        }

        // Send cancel signal
        cancel_token.cancel();

        // Wait for cancelled message
        let mut cancelled = false;
        while let Ok(msg) = msg_rx.recv().await {
            if let RunnerMessage(
                _,
                RunnerMessageKind::Stopped(StoppedReason::Failed(TaskFailedKind::Cancelled)),
            ) = msg
            {
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
        let (_control_tx, control_rx) = async_broadcast::broadcast(1);
        let cancel_token = CancellationToken::new();
        // Create a stream that yields an error
        let test_stream = stream! {
            yield Ok(Bytes::from(vec![1; 10]));
            yield Err(AdapterError::Unretryable{
                source: UnretryableError::Io {
                    source: Arc::new(std::io::Error::other("Network error")),
                },
            });
        };

        let (mut runner, msg_rx) = TaskRunner::new(
            Some(20),
            Box::pin(test_stream),
            1,
            control_rx,
            cancel_token.clone(),
        );

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        let mut got_error = false;
        while let Ok(msg) = msg_rx.recv().await {
            if let RunnerMessage(
                _,
                RunnerMessageKind::Stopped(StoppedReason::Failed(TaskFailedKind::StreamError(_))),
            ) = msg
            {
                got_error = true;
                break;
            }
        }

        runner_handle.await.unwrap();
        assert!(got_error);
    }

    #[tokio::test]
    async fn test_empty_stream() {
        let (_control_tx, control_rx) = async_broadcast::broadcast(1);
        let cancel_token = CancellationToken::new();
        // Create an empty stream
        let test_stream = stream! {
            yield Ok(Bytes::from(vec![]));
        };

        let (mut runner, msg_rx) = TaskRunner::new(
            None,
            Box::pin(test_stream),
            1,
            control_rx,
            cancel_token.clone(),
        );

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        let mut got_empty_error = false;
        while let Ok(msg) = msg_rx.recv().await {
            error!("msg: {msg:?}");
            if let RunnerMessage(
                _,
                RunnerMessageKind::Stopped(StoppedReason::Failed(TaskFailedKind::Empty)),
            ) = msg
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
        let (control_tx, control_rx) = async_broadcast::broadcast(1);
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

        let (mut runner, msg_rx) = TaskRunner::new(
            Some(3 * BUFFER_SIZE as u64), // Initially larger than first chunk to avoid early termination
            Box::pin(test_stream),
            runner_id,
            control_rx,
            cancel_token.clone(),
        );

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        // Send resize message after first download to avoid early termination
        let mut started = false;
        let mut finished = false;
        let mut resize_sent = false;

        while let Ok(msg) = msg_rx.recv().await {
            match msg {
                RunnerMessage(_, RunnerMessageKind::Started) => {
                    started = true;
                }
                RunnerMessage(_, RunnerMessageKind::Downloaded(_)) => {
                    // Send resize message immediately after first download (only once)
                    if !resize_sent {
                        control_tx
                            .broadcast_direct(ManagerMessage(
                                runner_id,
                                ManagerMessagesVariant::LimitTotal(expected_total),
                            ))
                            .await
                            .unwrap();
                        resize_sent = true;
                    }
                }
                RunnerMessage(_, RunnerMessageKind::Stopped(StoppedReason::Finished)) => {
                    finished = true;
                    break;
                }
                RunnerMessage(_, RunnerMessageKind::Stopped(StoppedReason::Failed(e))) => {
                    error!("runner: stopped with error: {e:?}");
                    break;
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
        let (control_tx, control_rx) = async_broadcast::broadcast(1);
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

        let (mut runner, msg_rx) = TaskRunner::new(
            Some(6 * BUFFER_SIZE as u64), // Initially larger size
            Box::pin(test_stream),
            1,
            control_rx,
            cancel_token.clone(),
        );

        let expected_total = (BUFFER_SIZE as f64 * 4.5) as u64;

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        // Wait for start and collect some download messages
        let mut started = false;
        let mut download_count = 0;
        let mut total_downloaded = 0;

        while let Ok(msg) = msg_rx.recv().await {
            match msg {
                RunnerMessage(_, RunnerMessageKind::Started) => {
                    trace!("task started");
                    started = true;
                }
                RunnerMessage(_, RunnerMessageKind::Downloaded(bytes)) => {
                    download_count += 1;
                    total_downloaded += bytes.len();
                    // After downloading 2 chunks (64 KB), resize to 4.5 chunks
                    if download_count == 2 {
                        trace!("runner: resize total to {expected_total}");
                        control_tx
                            .broadcast_direct(ManagerMessage(
                                1,
                                ManagerMessagesVariant::LimitTotal(expected_total),
                            ))
                            .await
                            .unwrap();
                    }
                }
                RunnerMessage(_, RunnerMessageKind::Stopped(StoppedReason::Finished)) => {
                    trace!("task finished");
                    break;
                }
                _ => panic!("Unexpected message: {msg:?}"),
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
        let (control_tx, control_rx) = async_broadcast::broadcast(1);
        let cancel_token = CancellationToken::new();

        // Create a stream with small chunks (50 bytes each) - these will accumulate in buffer
        let test_stream = stream! {
            for i in 0..20 {
                sleep(Duration::from_millis(10)).await;
                yield Ok(Bytes::from(vec![i as u8; 50])); // 50 bytes per chunk
            }
        };

        let (mut runner, msg_rx) = TaskRunner::new(
            Some(1000), // Initially allow 1000 bytes (20 chunks)
            Box::pin(test_stream),
            1,
            control_rx,
            cancel_token.clone(),
        );

        // Pre-schedule the control signal to be sent after 80ms
        // This should happen while the runner is actively downloading
        tokio::spawn({
            let control_tx = control_tx.clone();
            async move {
                sleep(Duration::from_millis(80)).await;
                let new_limit = 200; // 4 chunks worth
                trace!("sending limit signal to reduce total to {new_limit} bytes after 80ms");
                let _ = control_tx
                    .broadcast_direct(ManagerMessage(
                        1,
                        ManagerMessagesVariant::LimitTotal(new_limit),
                    ))
                    .await;
            }
        });

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        let mut started = false;
        let mut total_downloaded = 0;

        while let Ok(msg) = msg_rx.recv().await {
            match msg {
                RunnerMessage(_, RunnerMessageKind::Started) => {
                    trace!("task started");
                    started = true;
                }
                RunnerMessage(_, RunnerMessageKind::Downloaded(bytes)) => {
                    total_downloaded += bytes.len();
                    trace!("downloaded batch, total: {total_downloaded} bytes");
                }
                RunnerMessage(_, RunnerMessageKind::Stopped(StoppedReason::Finished)) => {
                    trace!("task finished normally");
                    break;
                }
                RunnerMessage(_, RunnerMessageKind::Stopped(StoppedReason::Failed(err))) => {
                    trace!("task failed with error: {err:?}");
                    break;
                }
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
        let (_control_tx, control_rx) = async_broadcast::broadcast(1);
        let cancel_token = CancellationToken::new();

        // Create a stream that produces more data than expected
        let test_stream = stream! {
            yield Ok(Bytes::from(vec![1; 10]));
            yield Ok(Bytes::from(vec![2; 10]));
        };

        let (mut runner, msg_rx) = TaskRunner::new(
            Some(10), // Expect only 10 bytes but will receive 20
            Box::pin(test_stream),
            1,
            control_rx,
            cancel_token.clone(),
        );

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        let mut started = false;
        let mut finished = false;
        let mut total_downloaded = 0;

        while let Ok(msg) = msg_rx.recv().await {
            match msg {
                RunnerMessage(_, RunnerMessageKind::Started) => {
                    started = true;
                }
                RunnerMessage(_, RunnerMessageKind::Downloaded(bytes)) => {
                    total_downloaded += bytes.len();
                }
                RunnerMessage(_, RunnerMessageKind::Stopped(StoppedReason::Finished)) => {
                    finished = true;
                    break;
                }
                _ => panic!("Unexpected message: {msg:?}"),
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
        let (control_tx, control_rx) = async_broadcast::broadcast(1);
        let cancel_token = CancellationToken::new();

        // Create a stream that will never complete
        let test_stream = stream! {
            loop {
                sleep(Duration::from_millis(10)).await;
                yield Ok(Bytes::from(vec![1; 10]));
            }
        };

        let (mut runner, msg_rx) = TaskRunner::new(
            None,
            Box::pin(test_stream),
            1,
            control_rx,
            cancel_token.clone(),
        );

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        // Wait for start then drop the control channel
        while let Ok(msg) = msg_rx.recv().await {
            if let RunnerMessage(_, RunnerMessageKind::Started) = msg {
                drop(control_tx);
                break;
            }
        }

        let mut got_channel_closed = false;
        while let Ok(msg) = msg_rx.recv().await {
            if let RunnerMessage(
                _,
                RunnerMessageKind::Stopped(StoppedReason::Failed(TaskFailedKind::ChannelClosed)),
            ) = msg
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
        let (_control_tx, control_rx) = async_broadcast::broadcast(1);
        let cancel_token = CancellationToken::new();
        let runner_id = 42;

        // Create a successful stream future
        let stream_future = async {
            let test_stream = stream! {
                yield Ok(Bytes::from(vec![1; 10]));
                yield Ok(Bytes::from(vec![2; 10]));
            };
            Ok::<AnyBytesStream, AdapterError>(Box::pin(test_stream))
        };

        // Capture the receiver from the callback
        let (callback_tx, callback_rx) = oneshot::channel();
        let on_channel_created = move |rx: Receiver<RunnerMessage>| {
            let _ = callback_tx.send(rx);
        };

        // Test the function
        let result = TaskRunner::new_with_async_and_callback(
            Some(20),
            stream_future,
            runner_id,
            control_rx,
            cancel_token,
            on_channel_created,
        )
        .await;

        // Should return Some(TaskRunner)
        assert!(result.is_some());
        let mut runner = result.unwrap();

        // The callback should have been called with a receiver
        let msg_rx = callback_rx.await.unwrap();

        // Spawn the runner to test it works
        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        // Verify messages work correctly
        let mut started = false;
        let mut finished = false;
        let mut total_downloaded = 0;

        while let Ok(msg) = msg_rx.recv().await {
            match msg {
                RunnerMessage(id, RunnerMessageKind::Started) => {
                    assert_eq!(id, runner_id);
                    started = true;
                }
                RunnerMessage(id, RunnerMessageKind::Downloaded(bytes)) => {
                    assert_eq!(id, runner_id);
                    total_downloaded += bytes.len();
                }
                RunnerMessage(id, RunnerMessageKind::Stopped(StoppedReason::Finished)) => {
                    assert_eq!(id, runner_id);
                    finished = true;
                    break;
                }
                _ => panic!("Unexpected message: {msg:?}"),
            }
        }

        runner_handle.await.unwrap();
        assert!(started);
        assert!(finished);
        assert_eq!(total_downloaded, 20);
    }

    #[tokio::test]
    async fn test_new_with_async_and_callback_stream_failure() {
        let (_control_tx, control_rx) = async_broadcast::broadcast(1);
        let cancel_token = CancellationToken::new();
        let runner_id = 42;

        // Create a failing stream future
        let stream_future = async {
            Err::<AnyBytesStream, AdapterError>(AdapterError::Unretryable {
                source: UnretryableError::Io {
                    source: Arc::new(std::io::Error::other("Mock network error")),
                },
            })
        };

        // Capture the receiver from the callback
        let (callback_tx, callback_rx) = oneshot::channel();
        let on_channel_created = move |rx: Receiver<RunnerMessage>| {
            let _ = callback_tx.send(rx);
        };

        // Test the function
        let result = TaskRunner::new_with_async_and_callback(
            Some(20),
            stream_future,
            runner_id,
            control_rx,
            cancel_token,
            on_channel_created,
        )
        .await;

        // Should return None due to stream failure
        assert!(result.is_none());

        // The callback should still have been called with a receiver
        let msg_rx = callback_rx.await.unwrap();

        // Should receive a stopped message with stream error
        let msg = msg_rx.recv().await.unwrap();
        match msg {
            RunnerMessage(
                id,
                RunnerMessageKind::Stopped(StoppedReason::Failed(TaskFailedKind::StreamError(_))),
            ) => {
                assert_eq!(id, runner_id);
            }
            _ => panic!("Expected stopped message with stream error, got: {msg:?}"),
        }

        // Channel should be closed after the error message
        assert!(msg_rx.recv().await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_multiple_runners_handle_own_messages() {
        let (control_tx, control_rx1) = async_broadcast::broadcast(10);
        let control_rx2 = control_tx.new_receiver();
        let control_rx3 = control_tx.new_receiver();

        let cancel_token = CancellationToken::new();

        // Create streams for all runners - each will yield 6 chunks of 10 bytes
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

        let (mut runner1, msg_rx1) = TaskRunner::new(
            Some(6 * BUFFER_SIZE as u64), // Total 6 chunks
            Box::pin(create_stream()),
            runner_id1,
            control_rx1,
            cancel_token.clone(),
        );

        let (mut runner2, msg_rx2) = TaskRunner::new(
            Some(6 * BUFFER_SIZE as u64), // Total 6 chunks
            Box::pin(create_stream()),
            runner_id2,
            control_rx2,
            cancel_token.clone(),
        );

        let (mut runner3, msg_rx3) = TaskRunner::new(
            Some(6 * BUFFER_SIZE as u64), // Total 6 chunks
            Box::pin(create_stream()),
            runner_id3,
            control_rx3,
            cancel_token.clone(),
        );

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

        // Use timeout to prevent infinite waiting
        let timeout_duration = Duration::from_secs(10);
        let result = tokio::time::timeout(timeout_duration, async {
            // Use select to handle messages from all runners
            loop {
                tokio::select! {
                    msg = msg_rx1.recv(), if !runner1_finished => {
                        match msg {
                            Ok(RunnerMessage(id, RunnerMessageKind::Started)) => {
                                assert_eq!(id, runner_id1);
                                runner1_started = true;
                            }
                            Ok(RunnerMessage(id, RunnerMessageKind::Downloaded(bytes))) => {
                                assert_eq!(id, runner_id1);
                                runner1_downloaded += bytes.len();

                                // Send limit message to runner2 only after some downloads
                                if !limit_sent && runner1_downloaded >= 20 && runner2_downloaded >= 20 {
                                    // Limit runner2 to 35 bytes (should stop after 3.5 chunks)
                                    control_tx
                                        .broadcast_direct(ManagerMessage(
                                            runner_id2,
                                            ManagerMessagesVariant::LimitTotal(3 * BUFFER_SIZE as u64),
                                        ))
                                        .await
                                        .unwrap();
                                    limit_sent = true;
                                }
                            }
                            Ok(RunnerMessage(id, RunnerMessageKind::Stopped(reason))) => {
                                assert_eq!(id, runner_id1);
                                assert!(matches!(reason, StoppedReason::Finished));
                                runner1_finished = true;
                            }
                            Err(_) => {
                                // Channel closed, treat as finished
                                runner1_finished = true;
                            }
                        }
                    }
                    msg = msg_rx2.recv(), if !runner2_finished => {
                        match msg {
                            Ok(RunnerMessage(id, RunnerMessageKind::Started)) => {
                                assert_eq!(id, runner_id2);
                                runner2_started = true;
                            }
                            Ok(RunnerMessage(id, RunnerMessageKind::Downloaded(bytes))) => {
                                assert_eq!(id, runner_id2);
                                runner2_downloaded += bytes.len();
                            }
                            Ok(RunnerMessage(id, RunnerMessageKind::Stopped(reason))) => {
                                assert_eq!(id, runner_id2);
                                assert!(matches!(reason, StoppedReason::Finished));
                                runner2_finished = true;
                            }
                            Err(_) => {
                                // Channel closed, treat as finished
                                runner2_finished = true;
                            }
                        }
                    }
                    msg = msg_rx3.recv(), if !runner3_finished => {
                        match msg {
                            Ok(RunnerMessage(id, RunnerMessageKind::Started)) => {
                                assert_eq!(id, runner_id3);
                                runner3_started = true;
                            }
                            Ok(RunnerMessage(id, RunnerMessageKind::Downloaded(bytes))) => {
                                assert_eq!(id, runner_id3);
                                runner3_downloaded += bytes.len();
                            }
                            Ok(RunnerMessage(id, RunnerMessageKind::Stopped(reason))) => {
                                assert_eq!(id, runner_id3);
                                assert!(matches!(reason, StoppedReason::Finished));
                                runner3_finished = true;
                            }
                            Err(_) => {
                                // Channel closed, treat as finished
                                runner3_finished = true;
                            }
                        }
                    }
                }

                // Break when all runners are finished
                if runner1_finished && runner2_finished && runner3_finished {
                    break;
                }
            }
        }).await;

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

        // Verify results
        assert!(runner1_started && runner2_started && runner3_started);
        assert!(runner1_finished && runner2_finished && runner3_finished);
        assert!(limit_sent);

        // Runner1 and Runner3 should have downloaded the full 6 chunks
        assert_eq!(runner1_downloaded, 6 * BUFFER_SIZE);
        assert_eq!(runner3_downloaded, 6 * BUFFER_SIZE);

        // Runner2 should have been limited to 3 chunks
        assert_eq!(runner2_downloaded, 3 * BUFFER_SIZE);

        println!("✓ Multiple runners correctly handled their own messages");
        println!("  - Runner1 downloaded: {runner1_downloaded} bytes (expected: 6 chunks)");
        println!(
            "  - Runner2 downloaded: {runner2_downloaded} bytes (expected: 3 chunks, limited)"
        );
        println!("  - Runner3 downloaded: {runner3_downloaded} bytes (expected: 6 chunks)");
    }
}
