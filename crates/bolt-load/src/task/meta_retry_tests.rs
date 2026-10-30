//! Cancellation of the metadata retry during a task's initialization.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use smol_cancellation_token::CancellationToken;
use tempfile::TempDir;

use crate::{
    adapter::{AdapterError, AnyBytesStream, BoltLoadAdapter, BoltLoadAdapterMeta, RetryableError},
    runtime::ThreadedRuntimeImpl,
    task::{BuildError, DownloadError, MetaRetry, Task, TaskOptions, TaskState},
};

/// Far longer than [`WATCHDOG`], so finishing in time proves cancellation.
const BACKOFF: Duration = Duration::from_secs(30);
const WATCHDOG: Duration = Duration::from_secs(5);

async fn watchdog<F: Future>(fut: F) -> F::Output {
    tokio::time::timeout(WATCHDOG, fut)
        .await
        .expect("watchdog: cancellation did not take effect in time")
}

/// Signals every metadata request, then fails it with a retryable error or
/// never answers.
struct SignalAdapter {
    requested: async_channel::Sender<()>,
    calls: Arc<AtomicUsize>,
    hang: bool,
}

impl SignalAdapter {
    fn new(hang: bool) -> (Self, async_channel::Receiver<()>, Arc<AtomicUsize>) {
        let (requested, rx) = async_channel::unbounded();
        let calls = Arc::new(AtomicUsize::new(0));
        let adapter = Self {
            requested,
            calls: calls.clone(),
            hang,
        };
        (adapter, rx, calls)
    }
}

impl BoltLoadAdapter for SignalAdapter {
    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let _ = self.requested.try_send(());
        if self.hang {
            std::future::pending::<()>().await;
        }
        Err(RetryableError::Io {
            source: Arc::new(std::io::Error::other("transient")),
        }
        .into())
    }

    async fn full_stream(&self) -> Result<AnyBytesStream, AdapterError> {
        unreachable!("the metadata request never succeeds")
    }
}

fn slow_retry() -> TaskOptions {
    TaskOptions::default().meta_retry(MetaRetry::default().min_delay(BACKOFF).max_delay(BACKOFF))
}

#[tokio::test]
async fn test_spawn_rejects_invalid_retry_before_requesting() {
    let (adapter, _, calls) = SignalAdapter::new(false);
    let dir = TempDir::new().unwrap();
    let result = Task::builder(adapter)
        .runtime(ThreadedRuntimeImpl::new_tokio_rt())
        .save_to(dir.path().join("file.bin"))
        .options(TaskOptions::default().meta_retry(MetaRetry::default().min_delay(Duration::ZERO)))
        .spawn();
    assert!(
        matches!(result, Err(BuildError::InvalidOptions { .. })),
        "{result:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

/// Cancelling the caller's token during the backoff or an in-flight request
/// ends the task in `Cancelled` without waiting for the 30 s backoff (INIT-2).
async fn assert_task_cancelled(hang: bool) {
    let (adapter, requested, calls) = SignalAdapter::new(hang);
    let dir = TempDir::new().unwrap();
    let token = CancellationToken::new();
    let task = Task::builder(adapter)
        .runtime(ThreadedRuntimeImpl::new_tokio_rt())
        .cancel_token(token.clone())
        .save_to(dir.path().join("file.bin"))
        .options(slow_retry())
        .spawn()
        .unwrap();
    watchdog(requested.recv()).await.unwrap();
    token.cancel();
    let status = task.status();
    let result = watchdog(task).await;
    assert!(
        matches!(result, Err(DownloadError::Cancelled)),
        "{result:?}"
    );
    assert_eq!(status.borrow().state, TaskState::Cancelled);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cancel_during_backoff() {
    assert_task_cancelled(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cancel_during_inflight_request() {
    assert_task_cancelled(true).await;
}

/// Dropping `retrieve_meta` cancels it; the cached metadata is reused.
#[tokio::test(flavor = "multi_thread")]
async fn test_retrieve_meta_caches_and_drop_cancels() {
    let (adapter, requested, calls) = SignalAdapter::new(true);
    let mut builder = Task::builder(adapter).options(slow_retry());
    {
        let mut retrieve = std::pin::pin!(builder.retrieve_meta());
        watchdog(async {
            tokio::select! {
                _ = &mut retrieve => panic!("the request never answers"),
                requested = requested.recv() => requested.unwrap(),
            }
        })
        .await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(builder.cached_meta().is_none());
    let meta = BoltLoadAdapterMeta {
        content_size: 7,
        filename: None,
    };
    let builder = builder.meta(meta);
    assert_eq!(builder.cached_meta().map(|m| m.content_size), Some(7));
}
