//! The observable status of a task: a latest-value [`watch`] channel of
//! [`TaskStatus`] snapshots, published by the task's attempt.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
};

use bolt_load_sync::watch;

use super::{DownloadError, DownloadMode, FallbackReason, Progress};
use crate::adapter::BoltLoadAdapterMeta;

/// The lifecycle state of a task.
///
/// ```text
/// Pending ──► Initializing ──► Downloading ──► Finished
///                  │                │
///                  └────────────────┴──► Failed | Cancelled
/// ```
///
/// A terminal state never changes. It deliberately has no `Ord`: `Failed`
/// during initialization does not come "after" `Downloading`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum TaskState {
    /// Spawned, the attempt has not started yet.
    Pending,
    /// Retrieving the metadata, selecting the mode and resolving the
    /// destination.
    Initializing,
    /// Transferring the content.
    Downloading,
    /// The output file is committed.
    Finished,
    /// The task failed, see [`TaskStatus::error`].
    Failed,
    /// The task was cancelled.
    Cancelled,
}

impl TaskState {
    /// Whether the state is `Finished`, `Failed` or `Cancelled`.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Finished | Self::Failed | Self::Cancelled)
    }
}

/// Download and write speed in bytes per second, EMA-smoothed.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct Speed {
    /// Bytes received per second.
    pub download: f64,
    /// Bytes written to the file per second; zero for the singleton mode.
    pub write: f64,
}

impl Speed {
    pub fn new(download: f64, write: f64) -> Self {
        Self { download, write }
    }
}

/// What initialization decided, available from the end of `Initializing` on.
///
/// A fallback to one connection replaces it once, see [`Self::fallback`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TaskInfo {
    /// The metadata the download is based on.
    pub meta: BoltLoadAdapterMeta,
    /// The mode of the transfer.
    pub mode: DownloadMode,
    /// The resolved destination.
    pub path: PathBuf,
    /// Why the task downloads over one connection although the server
    /// seemed to take ranges; the mode is then `Singleton` and the total
    /// unknown.
    ///
    /// Set when the metadata shows a content coding, or when ranges failed
    /// during the download. In the latter case the task starts over: the
    /// status turns `Downloading` again with this info and progress from
    /// zero.
    pub fallback: Option<FallbackReason>,
}

/// A snapshot of a task, published through a [`StatusReceiver`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TaskStatus {
    pub state: TaskState,
    /// Kept in every state, including `Failed` and `Cancelled`.
    pub progress: Progress,
    /// Zero outside `Downloading` and in its first value.
    pub speed: Speed,
    /// `Some` from the end of `Initializing` on.
    pub info: Option<Arc<TaskInfo>>,
    /// `Some` exactly when the state is `Failed` or `Cancelled`.
    pub error: Option<DownloadError>,
    /// The temporary file that could not be removed after a failure or a
    /// cancellation; the error that caused the removal is kept in `error`.
    pub leftover_temp_file: Option<PathBuf>,
}

impl TaskStatus {
    fn new(state: TaskState, progress: Progress, info: Option<Arc<TaskInfo>>) -> Self {
        Self {
            state,
            progress,
            speed: Speed::default(),
            info,
            error: None,
            leftover_temp_file: None,
        }
    }

    /// Whether the state is terminal; no newer status follows.
    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }

    /// The downloaded fraction in `0.0..=1.0`; `None` when the total is
    /// unknown or zero.
    pub fn fraction(&self) -> Option<f64> {
        match self.progress.total {
            Some(total) if total > 0 => Some(self.progress.downloaded as f64 / total as f64),
            _ => None,
        }
    }
}

/// Observes a task's [`TaskStatus`]; see [`watch::Receiver`].
///
/// It only guarantees the newest value: states and samples may be coalesced.
/// The channel closes after the terminal status, so
/// `while rx.changed().await.is_ok()` ends once the receiver has seen it.
pub type StatusReceiver = watch::Receiver<TaskStatus>;

struct Shared {
    tx: watch::Sender<TaskStatus>,
    /// Whether the terminal status was published; later publishes are
    /// ignored. Held while publishing, so none can follow the terminal one.
    terminal: Mutex<bool>,
    /// Set when downloading starts, replaced by a fallback.
    info: Mutex<Option<Arc<TaskInfo>>>,
}

impl Shared {
    fn info(&self) -> Option<Arc<TaskInfo>> {
        self.info
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn publish(&self, status: TaskStatus) {
        let terminal = self.terminal.lock().unwrap_or_else(PoisonError::into_inner);
        if *terminal {
            debug_assert!(false, "a status was published after the terminal one");
            return;
        }
        let previous = self.tx.send_replace(status);
        drop(terminal);
        drop(previous);
    }
}

/// Owns the only status sender of one attempt; publishes the transitions.
pub(crate) struct StatusPublisher(Arc<Shared>);

impl StatusPublisher {
    /// A publisher whose channel holds the `Pending` status.
    pub(crate) fn new() -> (Self, StatusReceiver) {
        let (tx, rx) = watch::channel(TaskStatus::new(
            TaskState::Pending,
            Progress::default(),
            None,
        ));
        let shared = Shared {
            tx,
            terminal: Mutex::new(false),
            info: Mutex::new(None),
        };
        (Self(Arc::new(shared)), rx)
    }

    /// Another handle to the same channel, for the panic handler.
    pub(crate) fn share(&self) -> Self {
        Self(self.0.clone())
    }

    pub(crate) fn initializing(&self) {
        self.0.publish(TaskStatus::new(
            TaskState::Initializing,
            Progress::default(),
            None,
        ));
    }

    /// Record `info` and publish a `Downloading` status with zero speed: the
    /// first one, or the first after a fallback.
    pub(crate) fn downloading(&self, info: TaskInfo, progress: Progress) {
        let info = Arc::new(info);
        *self.0.info.lock().unwrap_or_else(PoisonError::into_inner) = Some(info.clone());
        self.0.publish(TaskStatus::new(
            TaskState::Downloading,
            progress,
            Some(info),
        ));
    }

    /// Periodic progress samples while downloading.
    pub(crate) fn reporter(&self) -> ProgressReporter {
        ProgressReporter(self.0.clone())
    }

    /// Publish the terminal status. `Err(Cancelled)` ends in `Cancelled`,
    /// any other error in `Failed`.
    pub(crate) fn finish(
        &self,
        outcome: Result<(), DownloadError>,
        progress: Progress,
        leftover_temp_file: Option<PathBuf>,
    ) {
        let (state, error) = match outcome {
            Ok(()) => (TaskState::Finished, None),
            Err(DownloadError::Cancelled) => (TaskState::Cancelled, Some(DownloadError::Cancelled)),
            Err(e) => (TaskState::Failed, Some(e)),
        };
        let status = TaskStatus {
            error,
            leftover_temp_file,
            ..TaskStatus::new(state, progress, self.0.info())
        };
        let mut terminal = self
            .0
            .terminal
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *terminal {
            debug_assert!(false, "the terminal status was published twice");
            return;
        }
        *terminal = true;
        let previous = self.0.tx.send_replace(status);
        drop(terminal);
        drop(previous);
    }

    /// Whether the terminal status was published.
    pub(crate) fn is_finished(&self) -> bool {
        *self
            .0
            .terminal
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The last published progress.
    pub(crate) fn progress(&self) -> Progress {
        self.0.tx.borrow().progress.clone()
    }

    /// A receiver that has seen the current value, for tests.
    #[cfg(test)]
    pub(crate) fn subscribe(&self) -> StatusReceiver {
        self.0.tx.subscribe()
    }
}

/// Publishes progress samples in the `Downloading` state; cheap to clone.
///
/// Must not outlive the transfer, so the channel closes after the terminal
/// status.
#[derive(Clone)]
pub(crate) struct ProgressReporter(Arc<Shared>);

impl ProgressReporter {
    pub(crate) fn report(&self, progress: Progress, speed: Speed) {
        self.0.publish(TaskStatus {
            speed,
            ..TaskStatus::new(TaskState::Downloading, progress, self.0.info())
        });
    }

    /// Whether anyone observes the status; a sample nobody reads need not be
    /// built.
    pub(crate) fn is_observed(&self) -> bool {
        self.0.tx.receiver_count() > 0
    }

    /// A reporter whose samples go to `rx`, for tests driving a transfer
    /// directly.
    #[cfg(test)]
    pub(crate) fn for_test() -> (Self, StatusReceiver) {
        let (publisher, rx) = StatusPublisher::new();
        (publisher.reporter(), rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> TaskInfo {
        TaskInfo {
            meta: BoltLoadAdapterMeta::new(10),
            mode: DownloadMode::Singleton,
            path: "out.bin".into(),
            fallback: None,
        }
    }

    #[test]
    #[expect(
        clippy::single_range_in_vec_init,
        reason = "lists holding one downloaded range"
    )]
    fn transitions_are_published_in_order() {
        let (publisher, mut rx) = StatusPublisher::new();
        assert_eq!(rx.borrow().state, TaskState::Pending);
        publisher.initializing();
        assert_eq!(rx.borrow_and_update().state, TaskState::Initializing);
        publisher.downloading(info(), Progress::default());
        let status = rx.borrow_and_update();
        assert_eq!(status.state, TaskState::Downloading);
        assert!(status.info.is_some());
        publisher
            .reporter()
            .report(Progress::new(Some(10), 5, vec![0..5]), Speed::new(1.0, 2.0));
        let status = rx.borrow_and_update();
        assert_eq!(status.fraction(), Some(0.5));
        assert_eq!(status.speed, Speed::new(1.0, 2.0));
        publisher.finish(Ok(()), Progress::new(Some(10), 10, vec![0..10]), None);
        let status = rx.borrow_and_update();
        assert_eq!(status.state, TaskState::Finished);
        assert!(status.error.is_none());
        assert!(status.info.is_some(), "info stays in later states");
        assert!(publisher.is_finished());
    }

    #[test]
    #[expect(
        clippy::single_range_in_vec_init,
        reason = "lists holding one downloaded range"
    )]
    fn a_fallback_replaces_the_info() {
        let (publisher, rx) = StatusPublisher::new();
        publisher.initializing();
        let concurrent = TaskInfo {
            mode: DownloadMode::Concurrent,
            ..info()
        };
        publisher.downloading(concurrent, Progress::default());
        publisher
            .reporter()
            .report(Progress::new(Some(10), 5, vec![0..5]), Speed::default());
        let fallback = TaskInfo {
            fallback: Some(FallbackReason::RangeIgnored),
            ..info()
        };
        publisher.downloading(fallback, Progress::default());
        let status = rx.borrow();
        assert_eq!(status.state, TaskState::Downloading);
        assert_eq!(status.progress, Progress::default(), "starts over");
        let info = status.info.as_ref().unwrap();
        assert_eq!(info.mode, DownloadMode::Singleton);
        assert_eq!(info.fallback, Some(FallbackReason::RangeIgnored));
        drop(status);
        publisher.finish(Ok(()), Progress::new(Some(3), 3, vec![0..3]), None);
        let info = rx.borrow().info.clone().unwrap();
        assert_eq!(info.fallback, Some(FallbackReason::RangeIgnored), "kept");
    }

    #[test]
    fn cancellation_is_its_own_state() {
        let (publisher, rx) = StatusPublisher::new();
        publisher.initializing();
        publisher.finish(Err(DownloadError::Cancelled), Progress::default(), None);
        let status = rx.borrow();
        assert_eq!(status.state, TaskState::Cancelled);
        assert!(
            status
                .error
                .as_ref()
                .is_some_and(DownloadError::is_cancelled)
        );
        assert!(status.info.is_none());
    }

    #[test]
    fn the_channel_closes_after_the_last_handle() {
        let (publisher, mut rx) = StatusPublisher::new();
        let reporter = publisher.reporter();
        publisher.finish(Err(DownloadError::Aborted), Progress::default(), None);
        drop(publisher);
        assert!(!rx.is_closed(), "a reporter keeps the channel open");
        drop(reporter);
        assert!(rx.changed_blocking().is_ok());
        assert_eq!(rx.borrow_and_update().state, TaskState::Failed);
        assert!(rx.changed_blocking().is_err());
    }

    #[test]
    fn the_terminal_status_is_published_without_receivers() {
        let (publisher, rx) = StatusPublisher::new();
        let late = publisher.subscribe();
        drop((rx, late));
        publisher.finish(Ok(()), Progress::default(), None);
        assert_eq!(publisher.subscribe().borrow().state, TaskState::Finished);
    }

    #[test]
    fn fraction_is_none_without_a_total() {
        let (_publisher, rx) = StatusPublisher::new();
        assert_eq!(rx.borrow().fraction(), None);
        let status = TaskStatus::new(
            TaskState::Downloading,
            Progress::new(Some(0), 0, vec![]),
            None,
        );
        assert_eq!(status.fraction(), None);
    }
}
