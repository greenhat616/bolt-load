use async_channel::{Receiver, Sender};

use crate::{
    adapter::{AnyStream, BoltLoadAdapter},
    manager::{ManagerMessages, TaskId},
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
}
