use std::{
    fmt,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, ready},
};

use smol_cancellation_token::CancellationToken;

use super::{
    DownloadError, DownloadMode, Progress, TaskBuilder, TaskState, TaskStatus,
    status::StatusReceiver,
};
use crate::adapter::{AnyAdapter, BoltLoadAdapter, DynBoltLoadAdapter};

/// A spawned download: an awaitable, cancel-on-drop handle.
///
/// Await it for the outcome, or observe it through [`Task::status`].
/// Dropping it cancels the download and lets its cleanup run in the
/// background; [`Task::detach`] keeps it running instead.
///
/// ```ignore
/// let task = Task::builder(adapter)
///     .runtime(rt)
///     .save_to("large-file.bin")
///     .spawn()?;
///
/// let mut status = task.status();
/// while status.changed().await.is_ok() {
///     let s = status.borrow_and_update();
///     println!("{:?} {}/{:?}", s.state, s.progress.downloaded, s.progress.total);
/// }
/// let output = task.await?;
/// ```
#[must_use = "dropping a Task cancels the download; call `.detach()` to keep it running"]
pub struct Task {
    /// The attempt's thread; `None` once joined or detached.
    join: Option<blocking::Task<()>>,
    status: StatusReceiver,
    /// The task's own token, a child of the caller's.
    cancel_token: CancellationToken,
    /// Cached after the join, so awaiting again returns it.
    outcome: Option<Result<TaskOutput, DownloadError>>,
}

/// The outcome of a finished task.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TaskOutput {
    /// The committed output file.
    pub path: PathBuf,
    pub progress: Progress,
    pub mode: DownloadMode,
}

impl Task {
    /// Configure a download from `adapter`.
    pub fn builder(adapter: impl BoltLoadAdapter + 'static) -> TaskBuilder {
        Self::builder_arc(DynBoltLoadAdapter::new_arc(adapter))
    }

    /// Configure a download from an already type-erased, shared adapter.
    pub fn builder_arc(adapter: Arc<AnyAdapter>) -> TaskBuilder {
        TaskBuilder::new(adapter)
    }

    pub(super) fn new(
        join: blocking::Task<()>,
        status: StatusReceiver,
        cancel_token: CancellationToken,
    ) -> Self {
        Self {
            join: Some(join),
            status,
            cancel_token,
            outcome: None,
        }
    }

    /// A receiver of the task's status.
    ///
    /// It has seen only the initial `Pending` status, so its first
    /// `changed()` returns as soon as anything was published, and it still
    /// reads the terminal status after the task ended.
    pub fn status(&self) -> StatusReceiver {
        self.status.clone()
    }

    /// The current status.
    pub fn snapshot(&self) -> Arc<TaskStatus> {
        self.status.borrow()
    }

    /// Whether the terminal status is available. Awaiting may still be
    /// pending briefly, until the attempt's thread returns.
    pub fn is_terminal(&self) -> bool {
        self.status.borrow().is_terminal()
    }

    /// Request cancellation and return at once; await the task to join it.
    ///
    /// A task whose commit already started may still finish.
    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }

    /// The task's own token, a child of the one passed to
    /// [`TaskBuilder::cancel_token`]; cancelling it cancels this task only.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    /// Cancel and wait until every worker has exited and the temporary file
    /// was removed. Returns `Ok` when the commit had already started.
    pub async fn stop(self) -> Result<TaskOutput, DownloadError> {
        self.cancel();
        self.await
    }

    /// Let the download run without this handle. Receivers taken before
    /// keep observing it.
    pub fn detach(mut self) {
        if let Some(join) = self.join.take() {
            join.detach();
        }
    }

    /// Drive `pb` from the status on a dedicated thread until the task ends.
    ///
    /// The bar gets its length once the total is known and stays a spinner
    /// before; it is finished on success and abandoned otherwise. Style it
    /// or add it to a `MultiProgress` before passing it in.
    #[cfg(feature = "progressbar")]
    pub fn drive_progress_bar(&self, pb: indicatif::ProgressBar) -> std::thread::JoinHandle<()> {
        let mut status = self.status();
        std::thread::spawn(move || {
            loop {
                let s = status.borrow_and_update();
                if let Some(total) = s.progress.total
                    && pb.length() != Some(total)
                {
                    pb.set_length(total);
                }
                pb.set_position(s.progress.downloaded);
                match s.state {
                    TaskState::Finished => return pb.finish(),
                    state if state.is_terminal() => return pb.abandon(),
                    _ => {}
                }
                if status.changed_blocking().is_err() {
                    return pb.abandon();
                }
            }
        })
    }
}

/// The outcome recorded in the final status.
fn outcome(status: &TaskStatus) -> Result<TaskOutput, DownloadError> {
    match status.state {
        TaskState::Finished => match &status.info {
            Some(info) => Ok(TaskOutput {
                path: info.path.clone(),
                progress: status.progress.clone(),
                mode: info.mode,
            }),
            None => Err(DownloadError::Aborted),
        },
        TaskState::Cancelled => Err(DownloadError::Cancelled),
        TaskState::Failed => Err(status.error.clone().unwrap_or(DownloadError::Aborted)),
        _ => Err(DownloadError::Aborted),
    }
}

impl Future for Task {
    type Output = Result<TaskOutput, DownloadError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(join) = this.join.as_mut() {
            ready!(Pin::new(join).poll(cx));
            this.join = None;
            this.outcome = Some(outcome(&this.status.borrow()));
        }
        Poll::Ready(
            this.outcome
                .clone()
                .expect("a task that is neither running nor detached has an outcome"),
        )
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        // Still running: cancel, and let the cleanup finish in the background.
        if let Some(join) = self.join.take() {
            self.cancel_token.cancel();
            join.detach();
        }
    }
}

impl fmt::Debug for Task {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Task")
            .field("state", &self.status.borrow().state)
            .field("joined", &self.join.is_none())
            .finish()
    }
}
