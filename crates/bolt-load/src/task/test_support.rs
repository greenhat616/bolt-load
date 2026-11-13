//! Helpers shared by the task tests.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use smol_cancellation_token::CancellationToken;

use super::{DownloadMode, StatusReceiver, Task, TaskBuilder, TaskState, TaskStatus};
use crate::{DOWNLOADING_TMP_EXTENSION, adapter::BoltLoadAdapter, runtime::ThreadedRuntimeImpl};

pub(crate) const WATCHDOG: Duration = Duration::from_secs(30);

/// Fail instead of hanging when `fut` does not finish in time.
pub(crate) async fn watchdog<F: Future>(fut: F) -> F::Output {
    tokio::time::timeout(WATCHDOG, fut)
        .await
        .expect("watchdog: operation did not finish in time")
}

pub(crate) fn tmp_path(dest: &Path) -> PathBuf {
    let mut path = dest.as_os_str().to_os_string();
    path.push(DOWNLOADING_TMP_EXTENSION);
    PathBuf::from(path)
}

/// A builder on the current tokio runtime saving to `dest`, preferring
/// `mode` and cancelled by `token`.
pub(crate) fn builder(
    mode: DownloadMode,
    adapter: impl BoltLoadAdapter + 'static,
    dest: &Path,
    token: &CancellationToken,
) -> TaskBuilder {
    Task::builder(adapter)
        .runtime(ThreadedRuntimeImpl::new_tokio_rt())
        .save_to(dest)
        .prefer_mode(mode)
        .cancel_token(token.clone())
}

pub(crate) fn spawn(
    mode: DownloadMode,
    adapter: impl BoltLoadAdapter + 'static,
    dest: &Path,
    token: &CancellationToken,
) -> Task {
    builder(mode, adapter, dest, token)
        .spawn()
        .expect("task should spawn")
}

/// Every status `rx` observes, the current one first, until the channel
/// closes. Intermediate statuses may be coalesced.
pub(crate) async fn record(mut rx: StatusReceiver) -> Vec<Arc<TaskStatus>> {
    watchdog(async {
        let mut statuses = vec![rx.borrow_and_update()];
        while rx.changed().await.is_ok() {
            statuses.push(rx.borrow_and_update());
        }
        statuses
    })
    .await
}

fn rank(state: TaskState) -> u8 {
    match state {
        TaskState::Pending => 0,
        TaskState::Initializing => 1,
        TaskState::Downloading => 2,
        _ => 3,
    }
}

/// The observed states follow the status graph: they only move forward,
/// only `Downloading` repeats, and one terminal state comes last.
pub(crate) fn assert_transitions(statuses: &[Arc<TaskStatus>]) {
    let states: Vec<TaskState> = statuses.iter().map(|s| s.state).collect();
    for pair in states.windows(2) {
        let (prev, next) = (pair[0], pair[1]);
        assert!(
            rank(prev) < rank(next) || (prev == next && prev == TaskState::Downloading),
            "{prev:?} -> {next:?} in {states:?}"
        );
    }
    let last = states.last().expect("at least the current status");
    assert!(
        last.is_terminal(),
        "the last status is terminal: {states:?}"
    );
    for status in statuses {
        let failed = matches!(status.state, TaskState::Failed | TaskState::Cancelled);
        assert_eq!(status.error.is_some(), failed, "{status:?}");
        if rank(status.state) == 2 || status.state == TaskState::Finished {
            assert!(status.info.is_some(), "{status:?}");
        }
    }
}
