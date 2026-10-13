//! Cancellation of the metadata retry in the task builder and both task modes.

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
    runner::TaskError,
    runtime::ThreadedRuntimeImpl,
    task::{
        DownloadMode, MetaRetry, TaskBuilder, TaskManagerBuildError, TaskOptions,
        instance::{TaskEvent, TaskInstance, TaskInstanceError, TaskInstanceImpl},
    },
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

#[async_trait::async_trait]
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
async fn test_builder_rejects_invalid_retry_before_requesting() {
    let (adapter, _, calls) = SignalAdapter::new(false);
    let mut builder = TaskBuilder::default()
        .adapter(Box::new(adapter))
        .options(TaskOptions::default().meta_retry(MetaRetry::default().min_delay(Duration::ZERO)));
    let result = builder.retrieve_meta().await;
    assert!(
        matches!(
            result,
            Err(TaskManagerBuildError::FieldValidationFailed { .. })
        ),
        "{result:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

async fn assert_build_cancelled(hang: bool) {
    let (adapter, requested, calls) = SignalAdapter::new(hang);
    let dir = TempDir::new().unwrap();
    let token = CancellationToken::new();
    let build = TaskBuilder::default()
        .adapter(Box::new(adapter))
        .threaded_runtime(ThreadedRuntimeImpl::new_tokio_rt())
        .cancel_token(token.clone())
        .save_path(dir.path().join("file.bin"))
        .options(slow_retry())
        .build();
    let cancel = async {
        requested.recv().await.unwrap();
        token.cancel();
    };
    let (result, ()) = watchdog(futures::future::join(build, cancel)).await;
    assert!(
        matches!(result, Err(TaskManagerBuildError::Cancelled)),
        "{:?}",
        result.err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_builder_cancel_during_backoff() {
    assert_build_cancelled(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_builder_cancel_during_inflight_request() {
    assert_build_cancelled(true).await;
}

async fn assert_instance_cancelled(mode: DownloadMode, hang: bool) {
    let (adapter, requested, calls) = SignalAdapter::new(hang);
    let dir = TempDir::new().unwrap();
    let token = CancellationToken::new();
    let (event_tx, event_rx) = async_channel::unbounded();
    let mut task = TaskInstanceImpl::with_options(
        mode,
        ThreadedRuntimeImpl::new_tokio_rt(),
        None,
        slow_retry(),
    );
    task.run(
        Arc::new(Box::new(adapter)),
        dir.path().join("file.bin"),
        event_tx,
        token.clone(),
    )
    .unwrap();
    watchdog(requested.recv()).await.unwrap();
    token.cancel();
    let last = watchdog(async {
        let mut last = None;
        while let Ok(event) = event_rx.recv().await {
            last = Some(event);
        }
        last
    })
    .await;
    match last {
        Some(TaskEvent::Failed(TaskInstanceError::Failed { source })) => {
            assert!(
                matches!(*source, TaskError::Cancelled),
                "{mode:?}: {source:?}"
            )
        }
        other => panic!("{mode:?}: expected Failed(Cancelled), got {other:?}"),
    }
    watchdog(task.wait()).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1, "{mode:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_instance_cancel_during_backoff() {
    for mode in [DownloadMode::Singleton, DownloadMode::Concurrent] {
        assert_instance_cancelled(mode, false).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_instance_cancel_during_inflight_request() {
    for mode in [DownloadMode::Singleton, DownloadMode::Concurrent] {
        assert_instance_cancelled(mode, true).await;
    }
}
