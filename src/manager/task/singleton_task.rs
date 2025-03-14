use crate::{
    adapter::{AnyAdapter, BoltLoadAdapterMeta}, manager::Progress, runner::TaskRunner
};

use super::{Result, Task, TaskError, TaskFuture};

pub struct SingletonTask {
    runner: TaskRunner,
}

impl Task for SingletonTask {
    async fn start(&mut self, adapter: &AnyAdapter) -> Result<TaskFuture> {
        // TODO: use backon to retry
        let meta = adapter
            .retrieve_meta()
            .await
            .map_err(TaskError::RetrieveMetaFailed)?;
        Ok(Box::pin(async {
            Ok(())
        }))
    }

    async fn stop(&mut self) -> Result<()> {
        todo!()
    }

    fn inspect_progress(&self) -> Progress {
        todo!()
    }
}
