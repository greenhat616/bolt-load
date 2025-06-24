use crate::{
    adapter::{AnyBytesStream, StreamError},
    task::{ManagerMessage, ManagerMessagesVariant, RunnerId},
    utils::ShutdownGuardExt,
};
use async_channel::{Receiver, Sender};
use bytes::Bytes;
use futures::{FutureExt, StreamExt};
use smol_cancellation_token::CancellationToken;

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
    control_signal: Receiver<ManagerMessage>,
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
        receiver: Receiver<ManagerMessage>,
        cancel_token: CancellationToken,
    ) -> (Self, Receiver<RunnerMessage>) {
        let (tx, rx) = async_channel::unbounded();
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
        receiver: Receiver<ManagerMessage>,
        cancel_token: CancellationToken,
        on_channel_created: impl FnOnce(Receiver<RunnerMessage>),
    ) -> Option<Self> {
        let (tx, rx) = async_channel::unbounded();
        on_channel_created(rx);
        let stream = match stream.await {
            Ok(stream) => stream,
            Err(e) => {
                let _ = tx.send(RunnerMessage(
                    runner_id,
                    RunnerMessageKind::Stopped(StoppedReason::Failed(TaskFailedKind::StreamError(
                        e,
                    ))),
                ));
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
            futures::select! {
                signal = control_signal => {
                    // Task layer owned the cancel token guard, ensure cancel token when manager is dropping
                    if let Ok(signal) = signal {
                        let ManagerMessage(_, variant) = signal;
                        match variant {
                            ManagerMessagesVariant::ResizeTotal(total) => {
                                self.total = Some(total);
                            }
                        }
                    }
                }
                item = download => match item {
                    Some(item) => {
                        match item {
                            // TODO: check boundary after
                            Ok(item) => {
                                self.downloaded += item.len() as u64;
                                self.notify
                                    .send(RunnerMessageKind::Downloaded(item))
                                    .await
                                    .map_err(|_| TaskFailedKind::ChannelClosed)?;
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
                _ = cancelled => {
                    break Err(TaskRunError::Cancelled);
                }
            }
        };
        result?;

        // TODO: handle the error
        match self.total {
            Some(total) if total == self.downloaded => {}
            Some(total) => {
                let msg = format!(
                    "runner: downloaded content is not match the total size, total: {}, \
                     downloaded: {}",
                    total, self.downloaded
                );
                log::warn!("{msg}");
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
    use crate::adapter::{StreamError, UnretryableError};

    use super::*;
    use async_stream::stream;
    use pretty_assertions::assert_eq;
    use std::{sync::Arc, time::Duration};
    use test_log::test;
    use tokio::time::sleep;

    #[test(tokio::test)]
    async fn test_normal_download() {
        let (_control_tx, control_rx) = async_channel::unbounded();
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
            log::debug!("msg: {msg:?}");
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

    #[test(tokio::test)]
    async fn test_cancel_download() {
        let (control_tx, control_rx) = async_channel::unbounded();
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

    #[test(tokio::test)]
    async fn test_network_error() {
        let (_control_tx, control_rx) = async_channel::unbounded();
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

    #[test(tokio::test)]
    async fn test_empty_stream() {
        let (_control_tx, control_rx) = async_channel::unbounded();
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
            log::error!("msg: {msg:?}");
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

    #[test(tokio::test)]
    // TODO: add a test for the resize small, and implement the logic
    async fn test_resize_total() {
        let (control_tx, control_rx) = async_channel::unbounded();
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
            Some(10), // Initially wrong size
            Box::pin(test_stream),
            1,
            control_rx,
            cancel_token.clone(),
        );

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        // Send resize message after start
        let mut started = false;
        while let Ok(msg) = msg_rx.recv().await {
            if let RunnerMessage(_, RunnerMessageKind::Started) = msg {
                started = true;
                control_tx
                    .send(ManagerMessage(1, ManagerMessagesVariant::ResizeTotal(60)))
                    .await
                    .unwrap();
                break;
            }
        }

        let mut finished = false;
        while let Ok(msg) = msg_rx.recv().await {
            if let RunnerMessage(_, RunnerMessageKind::Stopped(StoppedReason::Finished)) = msg {
                finished = true;
                break;
            }
        }

        runner_handle.await.unwrap();
        assert!(started);
        assert!(finished);
    }

    #[test(tokio::test)]
    async fn test_size_mismatch() {
        let (_control_tx, control_rx) = async_channel::unbounded();
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

        let mut got_size_mismatch = false;
        while let Ok(msg) = msg_rx.recv().await {
            if let RunnerMessage(
                _,
                RunnerMessageKind::Stopped(StoppedReason::Failed(TaskFailedKind::Other(_))),
            ) = msg
            {
                got_size_mismatch = true;
                break;
            }
        }

        runner_handle.await.unwrap();
        assert!(got_size_mismatch);
    }

    #[test(tokio::test)]
    async fn test_channel_closed() {
        let (control_tx, control_rx) = async_channel::unbounded();
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
}
