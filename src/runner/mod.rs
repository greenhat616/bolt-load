use crate::manager::TaskId;

mod runner;

/// messages for runner -> manager
pub struct TaskMessages(pub TaskId, pub TaskMessagesVariant);
pub enum TaskMessagesVariant {
    Started,
    /// Stopped with message
    Stopped(Option<String>),
}
