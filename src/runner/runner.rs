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
    adapter: Box<
        dyn BoltLoadAdapter<
            Item = Result<bytes::Bytes, std::io::Error>,
            Stream = AnyStream<std::io::Result<bytes::Bytes>>,
        >,
    >,
    receiver: Receiver<ManagerMessages>,
    sender: Sender<TaskMessages>,
}

impl TaskRunner {
    pub fn new<A>(
        adapter: A,
        task_id: TaskId,
        receiver: Receiver<ManagerMessages>,
    ) -> (Self, Receiver<TaskMessages>)
    where
        A: BoltLoadAdapter<
                Item = Result<bytes::Bytes, std::io::Error>,
                Stream = AnyStream<std::io::Result<bytes::Bytes>>,
            > + Sync
            + 'static,
    {
        let (s, r) = async_channel::unbounded();
        (
            TaskRunner {
                adapter: Box::new(adapter),
                task_id,
                sender: s,
                receiver,
            },
            r,
        )
    }
}
