use crate::manager::TaskId;

mod runner;

/// messages for runner -> manager
pub struct TaskMessages(TaskId, TaskMessagesVariant);
pub enum TaskMessagesVariant {
    Started,
    Stopped,
}
