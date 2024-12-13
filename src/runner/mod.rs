use crate::{
    adapter::AnyBytesStream,
    manager::{ManagerMessage, ManagerMessagesVariant, TaskId},
};
use async_channel::{Receiver, Sender};
use bytes::Bytes;
use futures::FutureExt;
use futures::StreamExt;

/// messages for runner -> manager
pub struct TaskMessage(pub TaskId, pub TaskMessageKind);
pub enum TaskMessageKind {
    /// The task is started
    Started,
    /// Stopped with message
    Stopped(StoppedReason),
    /// Downloaded a chunk
    Downloaded(Bytes),
}

/// The reason why the task is stopped
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
struct TaskMessageSender(TaskId, Sender<TaskMessage>);

impl TaskMessageSender {
    pub fn new(task_id: TaskId, sender: Sender<TaskMessage>) -> Self {
        Self(task_id, sender)
    }

    pub async fn send(
        &self,
        message: TaskMessageKind,
    ) -> Result<(), async_channel::SendError<TaskMessage>> {
        self.1.send(TaskMessage(self.0, message)).await
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
    notify: TaskMessageSender,
    /// the sender of the shutdown signal
    shutdown_signal: Option<Sender<Result<(), TaskRunError>>>,
}

impl TaskRunner {
    pub fn new(
        total: Option<u64>,
        stream: AnyBytesStream,
        task_id: TaskId,
        receiver: Receiver<ManagerMessage>,
    ) -> (Self, Receiver<TaskMessage>) {
        let (tx, rx) = async_channel::unbounded();
        (
            TaskRunner {
                total,
                downloaded: 0,
                stream,
                notify: TaskMessageSender::new(task_id, tx),
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
                    .send(TaskMessageKind::Stopped(StoppedReason::Finished))
                    .await;
            }
            Err(err) => match err {
                TaskRunError::Cancelled => {
                    let _ = self
                        .notify
                        .send(TaskMessageKind::Stopped(StoppedReason::Cancelled))
                        .await;
                }
                TaskRunError::Failed(failed_kind) => {
                    let _ = self
                        .notify
                        .send(TaskMessageKind::Stopped(StoppedReason::Failed(
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
            .send(TaskMessageKind::Started)
            .await
            .unwrap();

        let (shutdown_tx, shutdown_rx) = async_channel::bounded(1);
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
                        Err(_) => {
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
                                    .send(TaskMessageKind::Downloaded(item))
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
                    "runner: downloaded content is not match the total size, total: {}, downloaded: {}",
                    total,
                    self.downloaded
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
