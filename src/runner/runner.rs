use async_channel::{Receiver, Sender};
use async_std::stream::StreamExt;
use futures::FutureExt;

use crate::{
    adapter::AnyStream,
    manager::{ManagerMessages, ManagerMessagesVariant, TaskId},
};

use super::TaskMessages;

/// runner for each chunk, responsible for downloading each chunk
pub struct TaskRunner {
    /// the id of the task
    task_id: TaskId,
    /// the adapter of the task
    stream: AnyStream<std::io::Result<bytes::Bytes>>,
    /// the receiver of the manager messages
    receiver: Receiver<ManagerMessages>,
    /// the sender of the task messages
    sender: Sender<TaskMessages>,
}

impl TaskRunner {
    pub fn new(
        stream: AnyStream<std::io::Result<bytes::Bytes>>,
        task_id: TaskId,
        receiver: Receiver<ManagerMessages>,
    ) -> (Self, Receiver<TaskMessages>) {
        let (s, r) = async_channel::unbounded();
        (
            TaskRunner {
                stream,
                task_id,
                sender: s,
                receiver,
            },
            r,
        )
    }

    pub async fn run(&mut self) {
        let listen = async {
            while let Ok(msg) = self.receiver.recv().await {
                let ManagerMessages(task_id, variant) = msg;
                if task_id == self.task_id {
                    match variant {
                        // TODO: implement resize task
                        ManagerMessagesVariant::ResizeEnd(range) => unimplemented!(),
                    }
                }
            }
        }
        .fuse();
        let load = async {
            self.sender
                .send(TaskMessages(
                    self.task_id,
                    super::TaskMessagesVariant::Started,
                ))
                .await
                .unwrap();
            let mut err_msg = None;
            while let Some(item) = self.stream.next().await {
                let item = match item {
                    Ok(item) => item,
                    Err(err) => {
                        err_msg = Some(err.to_string());
                        break;
                    }
                };
                // TODO: send the chunk to the write thread
            }

            self.sender
                .send(TaskMessages(
                    self.task_id,
                    super::TaskMessagesVariant::Stopped(err_msg),
                ))
                .await
                .unwrap();
        }
        .fuse();

        futures::pin_mut!(listen, load);
        futures::select! {
            _ = listen => {}
            _ = load => {}
        }
    }

    /// Shutdown the current task runner gracefully
    pub fn shutdown(&mut self) {
        self.sender.close();
        self.receiver.close();
    }
}
