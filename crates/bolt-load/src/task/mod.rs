use std::{
    fmt::Debug,
    ops::Range,
    path::PathBuf,
    sync::{Arc, Mutex, atomic::Ordering},
};

use async_channel::Sender;
use bolt_load_utils::telemetry::*;
use futures::{future::RemoteHandle, task::SpawnExt};
use smol_cancellation_token::CancellationToken;
#[cfg(feature = "tracing")]
use tracing::Instrument;

use crate::{
    DEFAULT_EVENT_CHANNEL_CAPACITY,
    adapter::{AnyAdapter, BoltLoadAdapterMeta},
    runtime::{LocalRuntimeBuilderImpl, ThreadedRuntimeImpl},
};

mod builder;
#[cfg(test)]
mod comprehensive_tests;
/// Task instance implementations (singleton and concurrent).
pub mod instance;

pub use builder::*;
use instance::*;

pub type RunnerId = usize;

pub type TaskStateChangedCallback = Box<dyn Fn(TaskEvent) + Send + Sync + 'static>;

/// messages for manager -> runner
#[derive(Debug, Clone)]
pub struct ManagerMessage(pub RunnerId, pub ManagerMessagesVariant);

#[derive(Debug, Clone)]
pub enum ManagerMessagesVariant {
    /// limit the total size of the task
    ///
    /// The limit number should be smaller than the total size of the task
    LimitTotal(u64),
}

pub type DownloadedChunks = Vec<Range<u64>>;

/// the progress of the download task
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Progress {
    /// the total size of the content
    /// possible None if the total size is unknown
    /// It requires the single-thread task can finished until the stream is None
    pub total: Option<u64>,

    /// the current downloaded size
    pub downloaded: u64,

    /// the downloaded chunks,
    /// for the single-thread task, it is the downloaded chunk
    /// for the concurrent task, it is the downloaded chunks
    pub downloaded_chunks: DownloadedChunks,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum DownloadMode {
    /// the download mode of single-thread download,
    /// Just use one runner task to fetch the content.
    Singleton,
    /// the download mode of concurrent download
    /// Due to we use multiple async tasks to download the content,
    /// and the async tasks is M:N thread model,
    /// so the download mode is concurrent, not called multi-thread.
    /// Because possible some runtime support single-thread async mode.
    #[default]
    Concurrent,
}

#[atomic_enum::atomic_enum]
#[derive(Default, PartialEq)]
pub enum TaskState {
    #[default]
    /// the task is idle, the initial state
    Idle,
    /// the task is spawning the task,
    /// Spawn the task and wait for the task to be initialized
    Spawning,
    /// the task is initializing the task
    Initializing,
    /// the task is downloading the content
    Downloading,
    /// the task is finished
    Finished,
    /// the task is failed with the error
    Failed,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum TaskFailedError {
    /// the task is cancelled
    #[error("the task is cancelled")]
    Cancelled,
    /// the task is stopped
    #[error("the task is stopped: {0:?}")]
    Stopped(TaskInstanceError),
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum TaskCommandError {}

type CommandResult<T> = Result<T, TaskCommandError>;
type CommandResponse<T> = oneshot::Sender<CommandResult<T>>;

pub enum TaskCommand {
    Cancel(CommandResponse<()>),
    // Pause(CommandResponse<()>),
    // Resume(CommandResponse<()>),
}

#[derive(Default)]
struct TaskStateControl(Option<(TaskState, async_channel::Sender<TaskState>)>);

impl TaskStateControl {
    pub fn new(state: TaskState, sender: Sender<TaskState>) -> Self {
        Self(Some((state, sender)))
    }

    pub async fn dispatch(
        &mut self,
        state: TaskState,
    ) -> Result<(), async_channel::SendError<TaskState>> {
        let Some((manager_state, sender)) = self.0.as_mut() else {
            unreachable!("we should init the state control first");
        };
        *manager_state = state;
        sender.send(state).await?;
        Ok(())
    }
}

#[non_exhaustive]
#[derive(derive_more::Debug)]
pub struct Task {
    /// the async runtime passed from the client
    #[debug(ignore)]
    threaded_rt: ThreadedRuntimeImpl,
    #[debug(ignore)]
    local_runtime_builder: Option<LocalRuntimeBuilderImpl>,
    /// the inner adapter of this task
    // TODO: support persistent adapter
    #[debug(ignore)]
    adapter: Arc<AnyAdapter>,
    /// the current mode of this task
    // TODO: maybe we should introduce a `prefer_mode` to indicate the preferred mode of this task.
    // TODO: support resumable download
    mode: DownloadMode,
    /// the save path of this task
    save_path: PathBuf,
    /// the tmp path of this task
    tmp_path: PathBuf,
    /// the meta of this task
    meta: BoltLoadAdapterMeta,
    /// the cancel token of this task
    cancel_token: CancellationToken,
    #[debug(ignore)]
    on_task_state_changed: Arc<Vec<TaskStateChangedCallback>>,
    #[debug(ignore)]
    task: TaskInstanceImpl,
    #[debug(ignore)]
    event_handler_handle: Option<RemoteHandle<()>>,
    task_state: Arc<AtomicTaskState>,
    last_error: Arc<Mutex<Option<TaskInstanceError>>>,
}

impl Task {
    pub fn builder() -> TaskBuilder {
        TaskBuilder::default()
    }

    /// check if the task is finished or failed
    pub fn is_finished(&self) -> bool {
        matches!(
            self.task_state.load(Ordering::Acquire),
            TaskState::Finished | TaskState::Failed
        )
    }

    /// check if the task is initialized
    pub fn is_initialized(&self) -> bool {
        self.task_state.load(Ordering::Acquire) != TaskState::Idle
    }

    #[cfg(feature = "progressbar")]
    pub fn with_progress_bar(&mut self) {
        let cb = std::mem::take(&mut self.on_task_state_changed);
        let mut cb =
            Arc::into_inner(cb).expect("we should init the callback first before running the task");
        let (tx, rx) = async_channel::bounded::<TaskEvent>(32);
        cb.push(Box::new(move |event| {
            tx.send_blocking(event).unwrap();
        }));

        std::thread::spawn(move || {
            let mut pb = None;
            while let Ok(event) = rx.recv_blocking() {
                match &event {
                    TaskEvent::Downloading(progress) => match &mut pb {
                        None => {
                            let content_size = progress.progress().total.unwrap_or(0);
                            let p = indicatif::ProgressBar::new(content_size);
                            p.set_style(
                                indicatif::ProgressStyle::with_template(
                                    "{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] \
                                     {bytes}/{total_bytes} ({bytes_per_sec}, {eta})",
                                )
                                .unwrap()
                                .progress_chars("#>-"),
                            );
                            pb = Some(p);
                        }
                        Some(pb) => {
                            pb.set_position(progress.progress().downloaded);
                        }
                    },
                    TaskEvent::Finished(_) => {
                        if let Some(pb) = pb.take() {
                            pb.finish();
                        }
                    }
                    _ => {}
                }
            }
        });
        self.on_task_state_changed = Arc::new(cb);
    }

    /// cancel the task, and do the cleanup work
    pub async fn stop(&mut self) {
        self.cancel_token.cancel();
        self.task.stop().await.unwrap();
    }

    #[cfg_attr(feature = "tracing", tracing::instrument)]
    pub async fn wait(&mut self) -> Result<(), TaskInstanceError> {
        if matches!(
            self.task_state.load(Ordering::Acquire),
            TaskState::Spawning | TaskState::Initializing | TaskState::Downloading
        ) {
            self.task.wait().await?;
            if let Some(handle) = self.event_handler_handle.take() {
                handle.await;
            }
        }
        let current_state = self.task_state.load(Ordering::Acquire);
        trace!("[TASK] Task::wait, current_state: {current_state:?}");
        if matches!(current_state, TaskState::Failed) {
            let error = self.last_error.lock().unwrap().clone().unwrap();
            return Err(error);
        }
        Ok(())
    }

    #[cfg_attr(feature = "tracing", tracing::instrument)]
    pub async fn run(&mut self) -> Result<(), TaskInstanceError> {
        let (event_tx, event_rx) = async_channel::bounded::<TaskEvent>(32);
        let cancel_token = self.cancel_token.clone();
        let on_task_state_changed = self.on_task_state_changed.clone();
        let task_state = self.task_state.clone();
        let last_error = self.last_error.clone();
        task_state.store(TaskState::Spawning, Ordering::Release);

        let fut = async move {
            while let Ok(event) = event_rx.recv().await {
                match &event {
                    TaskEvent::Finished(_) => {
                        #[cfg(test)]
                        trace!("[TASK] TaskEvent::Finished");
                        task_state.store(TaskState::Finished, Ordering::Release);
                    }
                    TaskEvent::Initializing => {
                        #[cfg(test)]
                        trace!("[TASK] TaskEvent::Initializing");
                        task_state.store(TaskState::Initializing, Ordering::Release);
                    }
                    TaskEvent::Downloading(_) => {
                        // trace!("[TASK] TaskEvent::Downloading, progress: {progress:?}");
                        task_state.store(TaskState::Downloading, Ordering::Release);
                    }
                    TaskEvent::Failed(error) => {
                        #[cfg(test)]
                        trace!("[TASK] TaskEvent::Failed");
                        task_state.store(TaskState::Failed, Ordering::Release);
                        last_error.lock().unwrap().replace(error.clone());
                    }
                }
                for callback in on_task_state_changed.iter() {
                    callback(event.clone());
                }
            }
        };

        #[cfg(feature = "tracing")]
        let fut = fut.instrument(tracing::trace_span!(
            parent: None,
            "Task::event_loop",
        ));

        let handle = self.threaded_rt.spawn_with_handle(fut).map_err(|e| {
            error!("failed to spawn the task: {e:?}");
            TaskInstanceError::new_failed(crate::runner::TaskFailedKind::Other(
                "failed to spawn the task".to_string(),
            ))
        })?;
        self.event_handler_handle = Some(handle);

        trace!("[TASK] Calling TaskInstance::run(), mode: {:?}", self.mode);
        match &self.task {
            TaskInstanceImpl::Singleton(_) => trace!("[TASK] Using SingletonTask"),
            TaskInstanceImpl::Concurrent(_) => trace!("[TASK] Using ConcurrentTask"),
        }

        self.task.run(
            self.adapter.clone(),
            self.save_path.clone(),
            event_tx,
            cancel_token,
        )?;
        trace!("[TASK] TaskInstance::run() completed");
        Ok(())
    }
}
