use async_broadcast::Receiver as BroadcastReceiver;
use async_channel::{Receiver, Sender};
use bytes::Bytes;
use futures::{FutureExt, StreamExt};
use smol_cancellation_token::CancellationToken;

use crate::{
    DEFAULT_EVENT_CHANNEL_CAPACITY,
    adapter::{AnyBytesStream, StreamError},
    task::{ManagerMessage, ManagerMessagesVariant, RunnerId},
    utils::{ShutdownGuardExt, logging::*},
};

mod guard;
pub use guard::*;

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
    StreamError(StreamError),
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
        stream: impl Future<Output = Result<AnyBytesStream, StreamError>>,
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
                                error!(
                                    "runner: limit total is smaller than current total,
                                    limit: {new_total}, current: {current_total}"
                                );
                                Err(TaskFailedKind::Other(format!(
                                    "limit total is smaller than current total, limit: \
                                     {new_total}, current: {current_total}"
                                )))?;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The inner logic of the task runner
    /// Just wrap a Result<(), TaskFailedKind> to return the error kind
    async fn run_inner(&mut self) -> Result<(), TaskRunError> {
        self.notify
            .send(RunnerMessageKind::Started)
            .await
            .map_err(|_| TaskRunError::Failed(TaskFailedKind::ChannelClosed))?;

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let _guard = shutdown_tx.shutdown_guard();
        self.shutdown_rx = Some(shutdown_rx);

        let result = loop {
            let control_signal = self.control_signal.recv().fuse();
            let download = self.stream.next().fuse();
            let cancelled = self.cancel_token.cancelled().fuse();

            futures::pin_mut!(control_signal, download, cancelled);
            futures::select_biased! {
                _ = cancelled => {
                    break Err(TaskRunError::Cancelled);
                }
                signal = control_signal => {
                    match signal {
                        Ok(signal) => {
                            if let Err(err) = self.handle_control_signal(&signal) {
                                break Err(err.into());
                            }
                        }
                        Err(_) => {
                            break Err(TaskFailedKind::ChannelClosed.into());
                        }
                    }
                }
                item = download => match item {
                    Some(item) => {
                        match item {
                            // TODO: check boundary after
                            Ok(item) => {
                                let size = match self.total {
                                    Some(total) => item.len().min(total as usize - self.downloaded as usize),
                                    None => item.len(),
                                };
                                self.downloaded += size as u64;
                                self.notify
                                    .send(RunnerMessageKind::Downloaded(item.slice(..size)))
                                    .await
                                    .map_err(|_| TaskFailedKind::ChannelClosed)?;

                                // Check if we've reached the total size limit
                                if let Some(total) = self.total {
                                    if self.downloaded >= total {
                                        break Ok(());
                                    }
                                }
                            }
                            // TODO: add a retry logic?
                            // First, we have to clarify whether this error is recoverable
                            // If it is, we can retry it
                            // If it is not, we should just return the error, and terminate the task
                            Err(err) => break Err(TaskFailedKind::StreamError(err).into()),
                        }
                    },
                    // In this case, the download is closed, which means the stream is finished
                    None => {
                        break Ok(());
                    }
                },
            }
        };
        result?;

        // TODO: handle the error
        match self.total {
            Some(total) if total == self.downloaded => {}
            Some(total) if self.downloaded > total => {
                warn!(
                    "runner: downloaded content is larger than the total size, total: {}, \
                     downloaded: {}",
                    total, self.downloaded
                );
            }
            Some(total) => {
                let msg = format!(
                    "runner: downloaded content is smaller than the total size, total: {}, \
                     downloaded: {}",
                    total, self.downloaded
                );
                warn!("{msg}");
                return Err(TaskFailedKind::Other(msg).into());
            }
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
    use crate::adapter::{StreamError, UnretryableError};

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
            yield Err(StreamError::Unretryable(UnretryableError::Io(
                Arc::new(std::io::Error::other("Network error")),
            )));
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
    #[tracing_test::traced_test]
    async fn test_resize_total_larger() {
        let runner_id = 1;
        let (control_tx, control_rx) = async_broadcast::broadcast(1);
        let cancel_token = CancellationToken::new();
        // Create a stream with known size
        let test_stream = stream! {
            yield Ok(Bytes::from(vec![1; 10]));
            sleep(Duration::from_millis(10)).await;
            yield Ok(Bytes::from(vec![2; 10]));
            sleep(Duration::from_millis(10)).await;
            yield Ok(Bytes::from(vec![3; 10]));
            sleep(Duration::from_millis(10)).await;
            yield Ok(Bytes::from(vec![4; 10]));
            sleep(Duration::from_millis(10)).await;
            yield Ok(Bytes::from(vec![5; 10]));
            sleep(Duration::from_millis(10)).await;
            yield Ok(Bytes::from(vec![6; 10]));
        };

        let (mut runner, msg_rx) = TaskRunner::new(
            Some(15), // Initially larger than first chunk to avoid early termination
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
                                ManagerMessagesVariant::LimitTotal(60),
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
    async fn test_resize_total_smaller() {
        let (control_tx, control_rx) = async_broadcast::broadcast(1);
        let cancel_token = CancellationToken::new();

        // Create a stream with multiple chunks that would normally total 60 bytes
        let test_stream = stream! {
            yield Ok(Bytes::from(vec![1; 10]));
            sleep(Duration::from_millis(10)).await;
            yield Ok(Bytes::from(vec![2; 10]));
            sleep(Duration::from_millis(10)).await;
            yield Ok(Bytes::from(vec![3; 10]));
            sleep(Duration::from_millis(10)).await;
            yield Ok(Bytes::from(vec![4; 10]));
            sleep(Duration::from_millis(10)).await;
            yield Ok(Bytes::from(vec![5; 10]));
            sleep(Duration::from_millis(10)).await;
            yield Ok(Bytes::from(vec![6; 10]));
        };

        let (mut runner, msg_rx) = TaskRunner::new(
            Some(60), // Initially larger size
            Box::pin(test_stream),
            1,
            control_rx,
            cancel_token.clone(),
        );

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
                    started = true;
                }
                RunnerMessage(_, RunnerMessageKind::Downloaded(bytes)) => {
                    download_count += 1;
                    total_downloaded += bytes.len();
                    // After downloading 2 chunks (20 bytes), resize to 25 bytes
                    if download_count == 2 {
                        control_tx
                            .broadcast_direct(ManagerMessage(
                                1,
                                ManagerMessagesVariant::LimitTotal(25),
                            ))
                            .await
                            .unwrap();
                    }
                }
                RunnerMessage(_, RunnerMessageKind::Stopped(StoppedReason::Finished)) => {
                    break;
                }
                _ => panic!("Unexpected message: {msg:?}"),
            }
        }

        runner_handle.await.unwrap();
        assert!(started);
        // Should have downloaded exactly 25 bytes (2 full chunks + 5 bytes from 3rd chunk)
        assert_eq!(total_downloaded, 25);
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
            Ok::<AnyBytesStream, StreamError>(Box::pin(test_stream))
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
            Err::<AnyBytesStream, StreamError>(StreamError::Unretryable(UnretryableError::Io(
                Arc::new(std::io::Error::other("Mock network error")),
            )))
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
    #[tracing_test::traced_test]
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
                    yield Ok(Bytes::from(vec![i as u8; 10]));
                }
            }
        };

        // Create three runners with different IDs
        let runner_id1 = 1;
        let runner_id2 = 2;
        let runner_id3 = 3;

        let (mut runner1, msg_rx1) = TaskRunner::new(
            Some(60), // Total 60 bytes
            Box::pin(create_stream()),
            runner_id1,
            control_rx1,
            cancel_token.clone(),
        );

        let (mut runner2, msg_rx2) = TaskRunner::new(
            Some(60), // Total 60 bytes
            Box::pin(create_stream()),
            runner_id2,
            control_rx2,
            cancel_token.clone(),
        );

        let (mut runner3, msg_rx3) = TaskRunner::new(
            Some(60), // Total 60 bytes
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
                                            ManagerMessagesVariant::LimitTotal(35),
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

        // Runner1 and Runner3 should have downloaded the full 60 bytes
        assert_eq!(runner1_downloaded, 60);
        assert_eq!(runner3_downloaded, 60);

        // Runner2 should have been limited to 35 bytes
        assert_eq!(runner2_downloaded, 35);

        println!("✓ Multiple runners correctly handled their own messages");
        println!("  - Runner1 downloaded: {runner1_downloaded} bytes (expected: 60)");
        println!("  - Runner2 downloaded: {runner2_downloaded} bytes (expected: 35, limited)");
        println!("  - Runner3 downloaded: {runner3_downloaded} bytes (expected: 60)");
    }
}
