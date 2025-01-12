use crate::{
    adapter::AnyBytesStream,
    manager::{ManagerMessage, ManagerMessagesVariant, RunnerId},
};
use async_channel::{Receiver, Sender};
use bytes::Bytes;
use futures::{FutureExt, StreamExt};

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
#[derive(Debug)]
pub enum StoppedReason {
    /// The task is finished
    Finished,
    /// The task is failed
    Failed(TaskFailedKind),
    /// The task is cancelled
    Cancelled,
}

/// The kind of the task failed
#[derive(Debug)]
pub enum TaskFailedKind {
    /// The task is timeout, only happen when a stream is not sent in a period
    Timeout,
    /// The task is empty
    Empty,
    /// The channel is closed
    ChannelClosed,
    /// The network error
    NetworkError(String),
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
    /// the sender of the shutdown signal
    shutdown_signal: Option<Sender<Result<(), TaskRunError>>>,
}

impl TaskRunner {
    pub fn new(
        total: Option<u64>,
        stream: AnyBytesStream,
        task_id: RunnerId,
        receiver: Receiver<ManagerMessage>,
    ) -> (Self, Receiver<RunnerMessage>) {
        let (tx, rx) = async_channel::unbounded();
        (
            TaskRunner {
                total,
                downloaded: 0,
                stream,
                notify: RunnerMessageSender::new(task_id, tx),
                control_signal: receiver,
                shutdown_signal: None,
            },
            rx,
        )
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
                        .send(RunnerMessageKind::Stopped(StoppedReason::Cancelled))
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

        let (shutdown_tx, shutdown_rx) = async_channel::unbounded();
        self.shutdown_signal = Some(shutdown_tx.clone());

        loop {
            let control_signal = async { self.control_signal.recv().await }.fuse();
            let download = async { self.stream.next().await }.fuse();
            let shutdown_signal = async { shutdown_rx.recv().await }.fuse();

            futures::pin_mut!(control_signal, download, shutdown_signal);
            futures::select! {
                signal = control_signal => {
                    match signal {
                        Ok(signal) => {
                            let ManagerMessage(_, variant) = signal;
                            match variant {
                                ManagerMessagesVariant::ResizeTotal(total) => {
                                    self.total = Some(total);
                                }
                                ManagerMessagesVariant::Cancel => {
                                    return Err(TaskRunError::Cancelled);
                                }
                            }
                        }
                        // In this case, the control signal is closed, which means the client or manager is released
                        // We should just call the shutdown function
                        Err(e) => {
                            log::debug!("control signal is closed, shutdown the task runner: {:?}", e);
                            let _ = shutdown_tx
                                .send(Err(TaskFailedKind::ChannelClosed.into()))
                                .await;
                        }
                    }
                }
                item = download => match item {
                    Some(item) => {
                        match item {
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
                            Err(err) => {
                                let _ = shutdown_tx
                                    .send(Err(TaskFailedKind::NetworkError(err.to_string()).into()))
                                    .await;
                            }
                        }
                    },
                    // In this case, the download is closed, which means the stream is finished
                    None => {
                        let _ = shutdown_tx.send(Ok(())).await;
                    }
                },
                signal = shutdown_signal => {
                    match signal {
                        Ok(Ok(())) => {
                            break;
                        }
                        Ok(Err(err)) => {
                            return Err(err);
                        }
                        Err(_) => {
                            log::debug!("shutdown signal is closed, shutdown the task runner");
                            return Err(TaskFailedKind::ChannelClosed.into());
                        }
                    }
                }
            }
        }

        // TODO: handle the error
        match self.total {
            Some(total) if total == self.downloaded => {}
            Some(total) => {
                let msg = format!(
                    "runner: downloaded content is not match the total size, total: {}, \
                     downloaded: {}",
                    total, self.downloaded
                );
                log::warn!("{}", msg);
                return Err(TaskFailedKind::Other(msg).into());
            }
            None if self.downloaded > 0 => {}
            None => {
                return Err(TaskFailedKind::Empty.into());
            }
        }
        Ok(())
    }

    /// Shutdown the current task runner gracefully
    pub fn shutdown(&mut self) {
        let _ = self
            .shutdown_signal
            .take()
            .unwrap()
            .try_send(Err(TaskRunError::Cancelled));
    }
}

#[cfg(test)]
mod tests {
    use crate::adapter::{StreamError, UnretryableError};

    use super::*;
    use async_stream::stream;
    use pretty_assertions::assert_eq;
    use std::time::Duration;
    use test_log::test;
    use tokio::time::sleep;

    #[test(tokio::test)]
    async fn test_normal_download() {
        let (_control_tx, control_rx) = async_channel::unbounded();

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
            log::debug!("msg: {:?}", msg);
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

        // Create an infinite stream that we'll cancel
        let test_stream = stream! {
            loop {
                sleep(Duration::from_millis(10)).await;
                yield Ok(Bytes::from(vec![1; 10]));
            }
        };

        let (mut runner, msg_rx) = TaskRunner::new(None, Box::pin(test_stream), 1, control_rx);

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
        control_tx
            .send(ManagerMessage(1, ManagerMessagesVariant::Cancel))
            .await
            .unwrap();

        // Wait for cancelled message
        let mut cancelled = false;
        while let Ok(msg) = msg_rx.recv().await {
            if let RunnerMessage(_, RunnerMessageKind::Stopped(StoppedReason::Cancelled)) = msg {
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

        // Create a stream that yields an error
        let test_stream = stream! {
            yield Ok(Bytes::from(vec![1; 10]));
            yield Err(StreamError::Unretryable(UnretryableError::Other(
                std::io::Error::new(std::io::ErrorKind::Other, "Network error"),
            )));
        };

        let (mut runner, msg_rx) = TaskRunner::new(Some(20), Box::pin(test_stream), 1, control_rx);

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        let mut got_error = false;
        while let Ok(msg) = msg_rx.recv().await {
            if let RunnerMessage(
                _,
                RunnerMessageKind::Stopped(StoppedReason::Failed(TaskFailedKind::NetworkError(_))),
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

        // Create an empty stream
        let test_stream = stream! {
            yield Ok(Bytes::from(vec![]));
        };

        let (mut runner, msg_rx) = TaskRunner::new(None, Box::pin(test_stream), 1, control_rx);

        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        let mut got_empty_error = false;
        while let Ok(msg) = msg_rx.recv().await {
            log::error!("msg: {:?}", msg);
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

        // Create a stream that will never complete
        let test_stream = stream! {
            loop {
                sleep(Duration::from_millis(10)).await;
                yield Ok(Bytes::from(vec![1; 10]));
            }
        };

        let (mut runner, msg_rx) = TaskRunner::new(None, Box::pin(test_stream), 1, control_rx);

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
