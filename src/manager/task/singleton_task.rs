use crate::runner::TaskRunner;

pub struct SingletonTask {
    runner: TaskRunner,
}

impl SingletonTask {
    pub fn load(&mut self) -> Result<(), String> {
        todo!()
    }
}
