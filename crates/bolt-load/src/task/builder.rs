use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_lock::OnceCell;
use bolt_load_core::adapter::AdapterError;
use bolt_load_utils::telemetry::*;
use futures::FutureExt;
use smol_cancellation_token::CancellationToken;

use super::{
    AtomicTaskState, DownloadMode, MetaRetry, Task, TaskInstanceImpl, TaskOptions, TaskState,
};
use crate::{
    adapter::{AnyAdapter, BoltLoadAdapterMeta},
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
    options: TaskOptions,
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
            options: TaskOptions::default(),
        }
    }
}

#[derive(Debug, snafu::Snafu)]
pub enum TaskManagerBuildError {
    #[snafu(display("field validation failed: {message}"))]
    FieldValidationFailed { message: String },
    #[snafu(transparent)]
    /// The error is unretryable, just returned by the adapter
    // TODO: mapping to UnretryableError
    AdapterError { source: AdapterError },
    #[snafu(transparent)]
    IOError { source: std::io::Error },
    /// The cancel token was cancelled before the task was built
    #[snafu(display("task build is cancelled"))]
    Cancelled,
}

/// Run `fut` unless `token` is cancelled first; the future is dropped on cancellation.
async fn until_cancelled<T>(
    token: &CancellationToken,
    fut: impl Future<Output = Result<T, TaskManagerBuildError>>,
) -> Result<T, TaskManagerBuildError> {
    if token.is_cancelled() {
        return Err(TaskManagerBuildError::Cancelled);
    }
    let fut = fut.fuse();
    let cancelled = token.cancelled().fuse();
    futures::pin_mut!(fut, cancelled);
    futures::select_biased! {
        _ = cancelled => Err(TaskManagerBuildError::Cancelled),
        res = fut => res,
    }
}

async fn try_get_or_init_meta<'a>(
    meta: &'a OnceCell<BoltLoadAdapterMeta>,
    adapter: &'a AnyAdapter,
    retry: &MetaRetry,
    rt: Option<&ThreadedRuntimeImpl>,
) -> Result<&'a BoltLoadAdapterMeta, TaskManagerBuildError> {
    let meta = meta
        .get_or_try_init(|| super::meta::retrieve_meta(adapter, retry, rt))
        .await?;
    Ok(meta)
}

impl TaskBuilder {
    /// You can call this method to get the meta before build the task manager
    ///
    /// Retryable errors are retried as [`TaskOptions::meta_retry`] allows,
    /// sleeping on the threaded runtime's timer if one is set. The cancel token
    /// is not observed here; only [`Self::build`] stops on cancellation.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip(self)))]
    pub async fn retrieve_meta(&mut self) -> Result<&BoltLoadAdapterMeta, TaskManagerBuildError> {
        self.options.validate()?;
        if self.adapter.is_none() {
            return Err(TaskManagerBuildError::FieldValidationFailed {
                message: "adapter is not set".to_string(),
            });
        }
        let adapter = self.adapter.as_ref().unwrap();
        let meta = try_get_or_init_meta(
            &self.meta,
            adapter,
            &self.options.meta_retry,
            self.threaded_runtime.as_ref(),
        )
        .await?;

        if self.save_path.is_none()
            && let Some(ref filename) = meta.filename
        {
            if let Some(ref save_dir) = self.save_dir {
                self.save_path = Some(save_dir.join(filename));
            } else {
                warn!("save dir is not set, the retrieved filename will not be automatically set.");
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

    /// set the termination options, see [`TaskOptions`]
    pub fn options(mut self, options: TaskOptions) -> Self {
        self.options = options;
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
            return Err(TaskManagerBuildError::FieldValidationFailed {
                message: "runtime is not set".to_string(),
            });
        }
        if self.adapter.is_none() {
            return Err(TaskManagerBuildError::FieldValidationFailed {
                message: "adapter is not set".to_string(),
            });
        }
        match self.save_path {
            Some(ref path) => {
                if path.file_name().is_none() {
                    return Err(TaskManagerBuildError::FieldValidationFailed {
                        message: "save path must have a filename".to_string(),
                    });
                }
            }
            None => {
                return Err(TaskManagerBuildError::FieldValidationFailed {
                    message: "save path is not set".to_string(),
                });
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
            return Err(TaskManagerBuildError::FieldValidationFailed {
                message: "cancel token is not set".to_string(),
            });
        }
        self.options.validate()?;
        let cancel_token = self.cancel_token.take().unwrap();
        until_cancelled(&cancel_token, async {
            self.retrieve_meta().await.map(drop)
        })
        .await?;
        self.validate()?;

        let adapter = self.adapter.take().unwrap();
        let runtime = self.threaded_runtime.take().unwrap();

        // TODO: support dynamic check while manager support resumable or persistent
        let mode = if self
            .prefer_mode
            .is_some_and(|m| m == DownloadMode::Singleton)
        {
            DownloadMode::Singleton
        } else if until_cancelled(&cancel_token, async {
            Ok(adapter.is_range_stream_available().await)
        })
        .await?
        {
            DownloadMode::Concurrent
        } else {
            DownloadMode::Singleton
        };

        // prepare the file handle
        let save_path = self.save_path.unwrap();

        // Check if the parent directory exists
        let parent_dir =
            save_path
                .parent()
                .ok_or(TaskManagerBuildError::FieldValidationFailed {
                    message: "save path parent directory does not exist".to_string(),
                })?;

        let meta = async_fs::metadata(parent_dir).await.ok();
        if meta.is_none_or(|m| !m.is_dir()) {
            return Err(TaskManagerBuildError::FieldValidationFailed {
                message: "save path parent directory does not exist or is not a directory"
                    .to_string(),
            });
        }

        // Check if the file exists and is a directory
        let meta = async_fs::metadata(&save_path).await;
        if meta.is_ok_and(|m| m.is_dir()) {
            return Err(TaskManagerBuildError::FieldValidationFailed {
                message: "save path already exists as a directory".to_string(),
            });
        }

        Ok(Task {
            adapter: Arc::new(adapter),
            mode,
            save_path,
            meta: self.meta.take().unwrap(),
            threaded_rt: runtime.clone(),
            local_runtime_builder: self.local_runtime_builder.clone(),
            task: TaskInstanceImpl::with_options(
                mode,
                runtime,
                self.local_runtime_builder.clone(),
                self.options.clone(),
            ),
            cancel_token,
            on_task_state_changed: Arc::new(self.on_task_state_changed.take().unwrap_or_default()),
            task_state: Arc::new(AtomicTaskState::new(TaskState::Idle)),
            event_handler_handle: None,
            last_error: Arc::new(Mutex::new(None)),
        })
    }
}
