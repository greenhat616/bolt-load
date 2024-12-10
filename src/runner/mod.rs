use crate::manager::TaskId;

mod runner;


/// messages for runner -> manager
pub enum TaskMessages {
    Started(TaskId),
    Finished(TaskId),
    // TODO: change the error message to a custom type
    Failed(TaskId, String),
}
