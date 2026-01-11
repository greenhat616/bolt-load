use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_lock::OnceCell;
use bolt_load_core::adapter::AdapterError;
use bolt_load_utils::telemetry::*;
use smol_cancellation_token::CancellationToken;

use super::{AtomicTaskState, DownloadMode, Task, TaskInstanceImpl, TaskState};
use crate::{
    adapter::{AnyAdapter, BoltLoadAdapterMeta, UnretryableError},
    runtime::{LocalRuntimeBuilderImpl, ThreadedRuntimeImpl},
    task::{TaskStateChangedCallback, instance::TaskEvent},
};

#[non_exhaustive]
pub struct TaskBuilder {
    cancel_token: Option<CancellationToken>,
    threaded_runtime: Option<ThreadedRuntimeImpl>,
    local_runtime_builder: Option<LocalRuntimeBuilderImpl>,
    meta: OnceCell<BoltLoadAdapterMeta>,
    adapter: Option<AnyAdapter>,
    prefer_mode: Option<DownloadMode>,
    save_path: Option<PathBuf>,
    /// a directory to save the file, It is used to save the file, prefer the filename retrieved from the adapter
    save_dir: Option<PathBuf>,
    on_task_state_changed: Option<Vec<TaskStateChangedCallback>>,
}

impl Default for TaskBuilder {
    fn default() -> Self {
        Self {
            meta: OnceCell::new(),
            adapter: None,
            prefer_mode: None,
            save_path: None,
            save_dir: None,
            threaded_runtime: None,
            local_runtime_builder: None,
            cancel_token: None,
            on_task_state_changed: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TaskManagerBuildError {
    #[error("field validation failed: {0}")]
    FieldValidationFailed(String),
    #[error(transparent)]
    /// The error is unretryable, just returned by the adapter
    // TODO: mapping to UnretryableError
    AdapterError(#[from] AdapterError),
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

impl TaskBuilder {
    /// You can call this method to get the meta before build the task manager
    #[cfg_attr(feature = "tracing", tracing::instrument(skip(self)))]
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
                    warn!(
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

    /// set the save directory
    pub fn save_dir(mut self, dir: PathBuf) -> Self {
        self.save_dir = Some(dir);
        self
    }

    /// set the threaded runtime
    pub fn threaded_runtime(mut self, runtime: ThreadedRuntimeImpl) -> Self {
        self.threaded_runtime = Some(runtime);
        self
    }

    /// set the local runtime builder
    ///
    /// It is optional, if not set, the local runtime will be the same as the threaded runtime.
    ///
    /// If you use custom runtime, you may need to set the local runtime builder if its runtime cannot downcast the runtime to a local runtime.
    pub fn local_runtime_builder(mut self, builder: LocalRuntimeBuilderImpl) -> Self {
        self.local_runtime_builder = Some(builder);
        self
    }

    pub fn on_task_state_changed<T>(mut self, callback: T) -> Self
    where
        T: Fn(TaskEvent) + Send + Sync + 'static,
    {
        let boxed_callback: Box<dyn Fn(TaskEvent) + Send + Sync + 'static> = Box::new(callback);
        if let Some(ref mut callbacks) = self.on_task_state_changed {
            callbacks.push(boxed_callback);
        } else {
            self.on_task_state_changed = Some(vec![boxed_callback]);
        }
        self
    }

    fn validate(&self) -> Result<(), TaskManagerBuildError> {
        if self.threaded_runtime.is_none() {
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

    #[cfg_attr(feature = "tracing", tracing::instrument(
        skip(self),
        fields(
            save_path = ?self.save_path.as_ref().map(|p| p.to_string_lossy()),
            save_dir = ?self.save_dir.as_ref().map(|p| p.to_string_lossy()),
            prefer_mode = ?self.prefer_mode,
        )
    ))]
    pub async fn build(mut self) -> Result<Task, TaskManagerBuildError> {
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
        let runtime = self.threaded_runtime.take().unwrap();

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

        // Check if the parent directory exists
        let parent_dir = save_path
            .parent()
            .ok_or(TaskManagerBuildError::FieldValidationFailed(
                "save path parent directory does not exist".to_string(),
            ))?;

        let meta = async_fs::metadata(parent_dir).await.ok();
        if meta.is_none_or(|m| !m.is_dir()) {
            return Err(TaskManagerBuildError::FieldValidationFailed(
                "save path parent directory does not exist or is not a directory".to_string(),
            ));
        }

        // Check if the file exists and is a directory
        let meta = async_fs::metadata(&save_path).await;
        if meta.is_ok_and(|m| m.is_dir()) {
            return Err(TaskManagerBuildError::FieldValidationFailed(
                "save path already exists as a directory".to_string(),
            ));
        }

        let mut temp_path = save_path.clone();
        if let Some(filename) = temp_path.file_name() {
            let mut file_name = filename.to_os_string();
            file_name.push(".partial");
            temp_path.set_file_name(file_name);
        }

        Ok(Task {
            adapter: Arc::new(adapter),
            mode,
            save_path,
            meta: self.meta.take().unwrap(),
            tmp_path: temp_path,
            threaded_rt: runtime.clone(),
            local_runtime_builder: self.local_runtime_builder.clone(),
            task: TaskInstanceImpl::new(mode, runtime, self.local_runtime_builder.clone()),
            cancel_token,
            on_task_state_changed: Arc::new(self.on_task_state_changed.take().unwrap_or_default()),
            task_state: Arc::new(AtomicTaskState::new(TaskState::Idle)),
            event_handler_handle: None,
            last_error: Arc::new(Mutex::new(None)),
        })
    }
}
