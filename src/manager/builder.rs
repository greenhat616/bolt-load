use async_channel::{unbounded, Sender};
use async_lock::OnceCell;
use std::path::PathBuf;

use super::{
    runner_notification::RunnerNotification, DownloadMode, TaskManager, TaskManagerCommand,
    TaskManagerState,
};
use crate::adapter::{AnyAdapter, BoltLoadAdapterMeta, UnretryableError};

pub struct TaskManagerBuilder {
    meta: OnceCell<BoltLoadAdapterMeta>,
    adapter: Option<AnyAdapter>,
    prefer_mode: Option<DownloadMode>,
    save_path: Option<PathBuf>,
}

impl Default for TaskManagerBuilder {
    fn default() -> Self {
        Self {
            meta: OnceCell::new(),
            adapter: None,
            prefer_mode: None,
            save_path: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TaskManagerBuildError {
    #[error("field validation failed: {0}")]
    FieldValidationFailed(String),
    #[error(transparent)]
    /// The error is unretryable, just returned by the adapter
    UnretryableError(#[from] UnretryableError),
}

async fn try_get_or_init_meta<'a>(
    meta: &'a OnceCell<BoltLoadAdapterMeta>,
    adapter: &'a AnyAdapter,
) -> Result<&'a BoltLoadAdapterMeta, TaskManagerBuildError> {
    let meta = meta
        .get_or_try_init(|| async { adapter.retrieve_meta().await })
        .await?;
    Ok(meta)
}

impl TaskManagerBuilder {
    /// You can call this method to get the meta before build the task manager
    pub async fn retrieve_meta(&mut self) -> Result<&BoltLoadAdapterMeta, TaskManagerBuildError> {
        if self.adapter.is_none() {
            return Err(TaskManagerBuildError::FieldValidationFailed(
                "adapter is not set".to_string(),
            ));
        }
        let adapter = self.adapter.as_ref().unwrap();
        let meta = try_get_or_init_meta(&self.meta, adapter).await?;

        if self.save_path.is_none() {
            if let Some(ref filename) = meta.filename {
                self.save_path = Some(PathBuf::from(filename));
            }
        }

        Ok(meta)
    }

    /// set the adapter
    pub fn adapter(mut self, adapter: AnyAdapter) -> Self {
        self.adapter = Some(adapter);
        self
    }

    /// set the prefer mode
    pub fn prefer_mode(mut self, mode: DownloadMode) -> Self {
        self.prefer_mode = Some(mode);
        self
    }

    /// set the save path
    pub fn save_path(mut self, path: PathBuf) -> Self {
        self.save_path = Some(path);
        self
    }

    fn validate(&self) -> Result<(), TaskManagerBuildError> {
        if self.adapter.is_none() {
            return Err(TaskManagerBuildError::FieldValidationFailed(
                "adapter is not set".to_string(),
            ));
        }
        if self.save_path.is_none() {
            return Err(TaskManagerBuildError::FieldValidationFailed(
                "save path is not set".to_string(),
            ));
        }
        Ok(())
    }

    pub async fn build<'a>(
        mut self,
    ) -> Result<(TaskManager<'a>, Sender<TaskManagerCommand>), TaskManagerBuildError> {
        let _ = self.retrieve_meta().await?;
        self.validate()?;
        let adapter = self.adapter.take().unwrap();
        // TODO: support dynamic check while manager support resumable or persistent
        let mode = if self
            .prefer_mode
            .is_some_and(|m| m == DownloadMode::Singleton)
        {
            DownloadMode::Singleton
        } else if adapter.is_range_stream_available().await {
            DownloadMode::Concurrent
        } else {
            DownloadMode::Singleton
        };
        let (cmd_tx, cmd_rx) = unbounded();
        Ok((
            TaskManager {
                adapter,
                mode,
                save_path: self.save_path.unwrap(),
                state: TaskManagerState::default(),
                meta: self.meta.take().unwrap(),
                runners: vec![],
                runner_control_channel: unbounded(),
                runners_notification: RunnerNotification::default(),
                cmd_rx,
            },
            cmd_tx,
        ))
    }
}
