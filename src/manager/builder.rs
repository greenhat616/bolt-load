use async_channel::{Sender, unbounded};
use async_fs::OpenOptions;
use async_lock::OnceCell;
use futures::FutureExt;
use smol_cancellation_token::CancellationToken;
use std::{collections::HashMap, path::PathBuf};

use super::{
    DownloadMode, TaskImpl, TaskManager, TaskManagerCommand, TaskManagerState,
    TaskManagerStateControl, runner_notification::RunnerNotification,
};
use crate::{
    adapter::{AnyAdapter, BoltLoadAdapterMeta, UnretryableError},
    runtime::Runtime,
};

pub struct TaskManagerBuilder {
    cancel_token: Option<CancellationToken>,
    runtime: Option<Runtime>,
    meta: OnceCell<BoltLoadAdapterMeta>,
    adapter: Option<AnyAdapter>,
    prefer_mode: Option<DownloadMode>,
    save_path: Option<PathBuf>,
    /// a directory to save the file, It is used to save the file, prefer the filename retrieved from the adapter
    save_dir: Option<PathBuf>,
}

impl Default for TaskManagerBuilder {
    fn default() -> Self {
        Self {
            meta: OnceCell::new(),
            adapter: None,
            prefer_mode: None,
            save_path: None,
            save_dir: None,
            runtime: None,
            cancel_token: None,
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
    #[error(transparent)]
    IOError(#[from] std::io::Error),
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
                if let Some(ref save_dir) = self.save_dir {
                    self.save_path = Some(save_dir.join(filename));
                } else {
                    log::warn!(
                        "save dir is not set, the retrieved filename will not be automatically \
                         set."
                    );
                }
            }
        }

        Ok(meta)
    }

    /// set the adapter
    pub fn adapter(mut self, adapter: AnyAdapter) -> Self {
        self.adapter = Some(adapter);
        self
    }

    /// set the cancel token
    pub fn cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = Some(cancel_token);
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
        if self.runtime.is_none() {
            return Err(TaskManagerBuildError::FieldValidationFailed(
                "runtime is not set".to_string(),
            ));
        }
        if self.adapter.is_none() {
            return Err(TaskManagerBuildError::FieldValidationFailed(
                "adapter is not set".to_string(),
            ));
        }
        match self.save_path {
            Some(ref path) => {
                if path.file_name().is_none() {
                    return Err(TaskManagerBuildError::FieldValidationFailed(
                        "save path must have a filename".to_string(),
                    ));
                }
            }
            None => {
                return Err(TaskManagerBuildError::FieldValidationFailed(
                    "save path is not set".to_string(),
                ));
            }
        }
        Ok(())
    }

    pub async fn build<'a>(
        mut self,
    ) -> Result<(TaskManager<'a>, Sender<TaskManagerCommand>), TaskManagerBuildError> {
        if self.cancel_token.is_none() {
            return Err(TaskManagerBuildError::FieldValidationFailed(
                "cancel token is not set".to_string(),
            ));
        }
        let cancel_token = self.cancel_token.take().unwrap();

        // retrieve the meta
        let _ = self.retrieve_meta().await?;
        self.validate()?;

        let adapter = self.adapter.take().unwrap();
        let runtime = self.runtime.take().unwrap();

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

        // prepare the file handle
        let save_path = self.save_path.unwrap();
        let mut temp_path = save_path.clone();
        if let Some(filename) = temp_path.file_name() {
            let mut file_name = filename.to_os_string();
            file_name.push(".partial");
            temp_path.set_file_name(file_name);
        }

        let (cmd_tx, cmd_rx) = unbounded();

        Ok((
            TaskManager {
                adapter,
                mode,
                save_path,
                state_control: TaskManagerStateControl::default(),
                meta: self.meta.take().unwrap(),
                control_channel: unbounded(),
                runners_notification: RunnerNotification::default(),
                cmd_rx,
                tmp_path: temp_path,
                runtime: runtime.clone(),
                task: TaskImpl::new(mode, runtime),
                cancel_token,
                runners: HashMap::new(),
                runner_progress: HashMap::new(),
                runner_speed: HashMap::new(),
            },
            cmd_tx,
        ))
    }
}
