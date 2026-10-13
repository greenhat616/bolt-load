//! Regression tests for issue #102: failures terminate with an error, and
//! stop/wait only return once every owned worker has exited.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use bolt_load_tests::adapter::{
    fault::{FaultKind, FaultPlan, Gate},
    simple::{SimpleTestAdapter, SimpleTestAdapterBuilder, calculate_blake3},
};
use smol_cancellation_token::CancellationToken;
use tempfile::TempDir;

use crate::{
    DOWNLOADING_TMP_EXTENSION,
    adapter::{AdapterError, BoltLoadAdapter},
    runner::TaskError,
    runtime::ThreadedRuntimeImpl,
    task::{
        DownloadMode, Task, TaskBuilder, TaskEvent, TaskInstanceError, TaskManagerBuildError,
        TaskOptions,
    },
};

const WATCHDOG: Duration = Duration::from_secs(30);
const SIZE: usize = 256 * 1024;

async fn watchdog<F: Future>(fut: F) -> F::Output {
    tokio::time::timeout(WATCHDOG, fut)
        .await
        .expect("watchdog: operation did not finish in time")
}

fn tmp_path(dest: &Path) -> PathBuf {
    let mut path = dest.as_os_str().to_os_string();
    path.push(DOWNLOADING_TMP_EXTENSION);
    PathBuf::from(path)
}

fn range_adapter() -> SimpleTestAdapterBuilder {
    SimpleTestAdapterBuilder::new()
        .content_size(SIZE)
        .support_range(true)
}

fn builder(
    adapter: &SimpleTestAdapter,
    dest: &Path,
    token: &CancellationToken,
    options: TaskOptions,
) -> TaskBuilder {
    TaskBuilder::default()
        .adapter(Box::new(adapter.clone()) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(dest.to_path_buf())
        .cancel_token(token.clone())
        .threaded_runtime(ThreadedRuntimeImpl::new_tokio_rt())
        .options(options)
}

async fn start(
    adapter: &SimpleTestAdapter,
    dest: &Path,
    token: &CancellationToken,
    options: TaskOptions,
    mode: DownloadMode,
) -> Task {
    let mut task = builder(adapter, dest, token, options)
        .prefer_mode(mode)
        .build()
        .await
        .expect("task should build");
    task.run().await.expect("task should start");
    task
}

fn task_error(result: Result<(), TaskInstanceError>) -> TaskError {
    match result {
        Err(TaskInstanceError::Failed { source }) => (*source).clone(),
        other => panic!("expected a failed task, got {other:?}"),
    }
}

fn assert_cleaned_up(adapter: &SimpleTestAdapter, dest: &Path) {
    assert!(!tmp_path(dest).exists(), "temporary file must be removed");
    assert!(!dest.exists(), "destination must not be created");
    assert_eq!(
        adapter.active_requests(),
        0,
        "no request may outlive the task"
    );
    assert_eq!(
        adapter.active_streams(),
        0,
        "no stream may outlive the task"
    );
}

fn assert_downloaded(adapter: &SimpleTestAdapter, dest: &Path) {
    let content = std::fs::read(dest).unwrap();
    assert_eq!(calculate_blake3(&content), adapter.expected_hash());
    assert!(!tmp_path(dest).exists(), "temporary file must be published");
}

// T1
#[tokio::test(flavor = "multi_thread")]
async fn test_concurrent_retryable_failures_hit_retry_limit() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("t1.bin");
    let adapter = range_adapter()
        .range_connect_plan(FaultPlan::AlwaysFail(FaultKind::Retryable))
        .build()
        .unwrap();
    let options = TaskOptions::default().max_consecutive_failures(3);
    let token = CancellationToken::new();
    let mut task = start(&adapter, &dest, &token, options, DownloadMode::Concurrent).await;

    match task_error(watchdog(task.wait()).await) {
        TaskError::RetryLimitExceeded { limit, source } => {
            assert_eq!(limit, 3);
            assert!(source.is_retryable(), "unexpected source: {source:?}");
        }
        other => panic!("expected RetryLimitExceeded, got {other:?}"),
    }
    assert_cleaned_up(&adapter, &dest);
}

// T2
#[tokio::test(flavor = "multi_thread")]
async fn test_concurrent_unretryable_failure_keeps_adapter_error() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("t2.bin");
    let adapter = range_adapter()
        .range_connect_plan(FaultPlan::AlwaysFail(FaultKind::Unretryable))
        .build()
        .unwrap();
    let token = CancellationToken::new();
    let mut task = start(
        &adapter,
        &dest,
        &token,
        TaskOptions::default(),
        DownloadMode::Concurrent,
    )
    .await;

    match task_error(watchdog(task.wait()).await) {
        TaskError::StreamError {
            source: AdapterError::Unretryable { .. },
        } => {}
        other => panic!("expected the adapter's unretryable error, got {other:?}"),
    }
    let first_wave = std::thread::available_parallelism().map_or(4, |n| n.get());
    assert!(
        adapter.range_attempts() <= first_wave,
        "an unretryable failure must not be retried"
    );
    assert_cleaned_up(&adapter, &dest);
}

// T3
#[tokio::test(flavor = "multi_thread")]
async fn test_concurrent_stall_fails_with_no_progress() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("t3.bin");
    let adapter = range_adapter().stall_after_bytes(0).build().unwrap();
    let options = TaskOptions::default().no_progress_timeout(Duration::from_secs(2));
    let token = CancellationToken::new();
    let mut task = start(&adapter, &dest, &token, options, DownloadMode::Concurrent).await;

    match task_error(watchdog(task.wait()).await) {
        TaskError::NoProgress { timeout } => assert_eq!(timeout, Duration::from_secs(2)),
        other => panic!("expected NoProgress, got {other:?}"),
    }
    assert_cleaned_up(&adapter, &dest);
}

// T4
#[tokio::test(flavor = "multi_thread")]
async fn test_concurrent_recovers_after_transient_failures() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("t4.bin");
    let adapter = range_adapter()
        .range_connect_plan(FaultPlan::FailFirst(2, FaultKind::Retryable))
        .build()
        .unwrap();
    let options = TaskOptions::default().max_consecutive_failures(3);
    let token = CancellationToken::new();
    let mut task = start(&adapter, &dest, &token, options, DownloadMode::Concurrent).await;

    watchdog(task.wait())
        .await
        .expect("download should recover");
    assert_downloaded(&adapter, &dest);
    assert!(!token.is_cancelled());
}

// T6
#[tokio::test(flavor = "multi_thread")]
async fn test_stop_while_connecting_joins_requests() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("t6.bin");
    let gate = Gate::new();
    let adapter = range_adapter()
        .range_connect_plan(FaultPlan::Block(gate.clone()))
        .build()
        .unwrap();
    let token = CancellationToken::new();
    let mut task = start(
        &adapter,
        &dest,
        &token,
        TaskOptions::default(),
        DownloadMode::Concurrent,
    )
    .await;

    watchdog(gate.entered()).await;
    watchdog(task.stop()).await;
    assert_cleaned_up(&adapter, &dest);
    assert!(task_error(task.wait().await).is_cancelled());
}

// T8
#[tokio::test(flavor = "multi_thread")]
async fn test_redownload_after_cancel_has_no_stale_writes() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("t8.bin");
    let stalled = range_adapter()
        .stall_after_bytes(SIZE as u64 / 2)
        .build()
        .unwrap();
    let downloaded = Arc::new(AtomicU64::new(0));
    let token = CancellationToken::new();
    let mut task = builder(&stalled, &dest, &token, TaskOptions::default())
        .prefer_mode(DownloadMode::Concurrent)
        .on_task_state_changed({
            let downloaded = downloaded.clone();
            move |event| {
                if let TaskEvent::Downloading(progress) = event {
                    downloaded.fetch_max(progress.progress().downloaded, Ordering::SeqCst);
                }
            }
        })
        .build()
        .await
        .unwrap();
    task.run().await.unwrap();
    // Cancel only after data has been handed to the writer.
    watchdog(async {
        while downloaded.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    watchdog(task.stop()).await;
    assert_cleaned_up(&stalled, &dest);

    let adapter = range_adapter().build().unwrap();
    let token = CancellationToken::new();
    let mut task = start(
        &adapter,
        &dest,
        &token,
        TaskOptions::default(),
        DownloadMode::Concurrent,
    )
    .await;
    watchdog(task.wait())
        .await
        .expect("redownload should succeed");
    assert_downloaded(&adapter, &dest);
}

// T9
#[tokio::test(flavor = "multi_thread")]
async fn test_build_is_cancellable() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("t9.bin");

    // Cancelled before build.
    let adapter = range_adapter().build().unwrap();
    let token = CancellationToken::new();
    token.cancel();
    let result = builder(&adapter, &dest, &token, TaskOptions::default())
        .build()
        .await;
    assert!(matches!(result, Err(TaskManagerBuildError::Cancelled)));
    assert_eq!(adapter.call_count(), 0, "no request may be sent");

    // Cancelled while retrieving the metadata, then while probing range support.
    let (meta_gate, probe_gate) = (Gate::new(), Gate::new());
    for (adapter, gate) in [
        (
            range_adapter()
                .meta_gate(meta_gate.clone())
                .build()
                .unwrap(),
            meta_gate,
        ),
        (
            range_adapter()
                .range_probe_gate(probe_gate.clone())
                .build()
                .unwrap(),
            probe_gate,
        ),
    ] {
        let token = CancellationToken::new();
        let build = tokio::spawn(builder(&adapter, &dest, &token, TaskOptions::default()).build());
        watchdog(gate.entered()).await;
        token.cancel();
        let result = watchdog(build).await.unwrap();
        assert!(matches!(result, Err(TaskManagerBuildError::Cancelled)));
        assert_eq!(adapter.active_requests(), 0);
    }
    assert!(!dest.exists() && !tmp_path(&dest).exists());
}

// T10
#[tokio::test(flavor = "multi_thread")]
async fn test_singleton_never_hangs() {
    let dir = TempDir::new().unwrap();
    let idle = TaskOptions::default().stream_idle_timeout(Duration::from_millis(300));

    // Connection blocked, then cancelled.
    let gate = Gate::new();
    let adapter = range_adapter()
        .full_connect_plan(FaultPlan::Block(gate.clone()))
        .build()
        .unwrap();
    let dest = dir.path().join("t10-cancel.bin");
    let token = CancellationToken::new();
    let mut task = start(
        &adapter,
        &dest,
        &token,
        idle.clone(),
        DownloadMode::Singleton,
    )
    .await;
    watchdog(gate.entered()).await;
    watchdog(task.stop()).await;
    assert!(task_error(task.wait().await).is_cancelled());
    assert_cleaned_up(&adapter, &dest);

    // Connection blocked past the no-progress deadline.
    let adapter = range_adapter()
        .full_connect_plan(FaultPlan::Block(Gate::new()))
        .build()
        .unwrap();
    let dest = dir.path().join("t10-connect.bin");
    let options = idle.clone().no_progress_timeout(Duration::from_millis(300));
    let mut task = start(
        &adapter,
        &dest,
        &CancellationToken::new(),
        options,
        DownloadMode::Singleton,
    )
    .await;
    assert!(matches!(
        task_error(watchdog(task.wait()).await),
        TaskError::NoProgress { .. }
    ));
    assert_cleaned_up(&adapter, &dest);

    // Stalled before the first byte, after some bytes, and on empty frames.
    for (name, adapter) in [
        (
            "first-byte",
            range_adapter().stall_after_bytes(0).build().unwrap(),
        ),
        (
            "partial",
            range_adapter()
                .stall_after_bytes(SIZE as u64 / 2)
                .build()
                .unwrap(),
        ),
        (
            "empty-frames",
            range_adapter()
                .stall_after_bytes(0)
                .stall_with_empty_frames(true)
                .build()
                .unwrap(),
        ),
    ] {
        let dest = dir.path().join(format!("t10-{name}.bin"));
        let mut task = start(
            &adapter,
            &dest,
            &CancellationToken::new(),
            idle.clone(),
            DownloadMode::Singleton,
        )
        .await;
        match task_error(watchdog(task.wait()).await) {
            TaskError::Timeout => {}
            other => panic!("{name}: expected Timeout, got {other:?}"),
        }
        assert_cleaned_up(&adapter, &dest);
    }
}

// T11
#[tokio::test(flavor = "multi_thread")]
async fn test_singleton_success_keeps_caller_token() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("t11.bin");
    let adapter = range_adapter().build().unwrap();
    let token = CancellationToken::new();
    let mut task = start(
        &adapter,
        &dest,
        &token,
        TaskOptions::default(),
        DownloadMode::Singleton,
    )
    .await;

    watchdog(task.wait())
        .await
        .expect("download should succeed");
    assert!(
        !token.is_cancelled(),
        "success must not cancel the caller's token"
    );
    assert_downloaded(&adapter, &dest);
}

// T12
#[tokio::test(flavor = "multi_thread")]
async fn test_wait_and_stop_are_idempotent() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("t12.bin");
    let gate = Gate::new();
    let adapter = range_adapter()
        .range_connect_plan(FaultPlan::Block(gate.clone()))
        .build()
        .unwrap();
    let token = CancellationToken::new();
    let mut task = builder(&adapter, &dest, &token, TaskOptions::default())
        .build()
        .await
        .unwrap();

    // Before `run`.
    watchdog(task.wait()).await.unwrap();

    task.run().await.unwrap();
    watchdog(gate.entered()).await;

    // A dropped `wait` future must not detach the task.
    assert!(
        tokio::time::timeout(Duration::from_millis(50), task.wait())
            .await
            .is_err()
    );

    watchdog(task.stop()).await;
    watchdog(task.stop()).await;
    for _ in 0..2 {
        assert!(task_error(watchdog(task.wait()).await).is_cancelled());
    }
    assert_cleaned_up(&adapter, &dest);
}

// T14
#[tokio::test(flavor = "multi_thread")]
async fn test_failed_event_is_emitted_after_cleanup() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("t14.bin");
    let adapter = range_adapter()
        .range_connect_plan(FaultPlan::AlwaysFail(FaultKind::Unretryable))
        .build()
        .unwrap();
    let observed = Arc::new(Mutex::new(None));
    let token = CancellationToken::new();
    let mut task = builder(&adapter, &dest, &token, TaskOptions::default())
        .on_task_state_changed({
            let observed = observed.clone();
            let adapter = adapter.clone();
            let tmp = tmp_path(&dest);
            move |event| {
                if let TaskEvent::Failed(_) = event {
                    observed
                        .lock()
                        .unwrap()
                        .replace((tmp.exists(), adapter.active_streams()));
                }
            }
        })
        .build()
        .await
        .unwrap();
    task.run().await.unwrap();

    assert!(watchdog(task.wait()).await.is_err());
    assert_eq!(
        *observed.lock().unwrap(),
        Some((false, 0)),
        "Failed must be emitted after the temporary file and streams are gone"
    );
}

// Concurrent runners honour `stream_idle_timeout`; with the 30 s default this
// would trip the watchdog.
#[tokio::test(flavor = "multi_thread")]
async fn test_concurrent_idle_runners_time_out_and_exhaust_retries() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("idle.bin");
    let adapter = range_adapter().stall_after_bytes(0).build().unwrap();
    let options = TaskOptions::default()
        .stream_idle_timeout(Duration::from_millis(200))
        .max_consecutive_failures(2);
    let token = CancellationToken::new();
    let mut task = start(&adapter, &dest, &token, options, DownloadMode::Concurrent).await;

    match task_error(watchdog(task.wait()).await) {
        TaskError::RetryLimitExceeded { source, .. } => {
            assert!(matches!(*source, TaskError::Timeout), "got {source:?}")
        }
        other => panic!("expected RetryLimitExceeded, got {other:?}"),
    }
    assert_cleaned_up(&adapter, &dest);
}

// A failed run must not leak its error into a later successful run.
#[tokio::test(flavor = "multi_thread")]
async fn test_successful_rerun_clears_previous_error() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("rerun.bin");
    let adapter = range_adapter()
        .full_connect_plan(FaultPlan::FailFirst(1, FaultKind::Unretryable))
        .build()
        .unwrap();
    let token = CancellationToken::new();
    let mut task = start(
        &adapter,
        &dest,
        &token,
        TaskOptions::default(),
        DownloadMode::Singleton,
    )
    .await;
    assert!(watchdog(task.wait()).await.is_err());

    task.run().await.unwrap();
    watchdog(task.wait()).await.expect("rerun should succeed");
    assert_downloaded(&adapter, &dest);
}

// A short no-progress deadline must not expire before the first runner is
// scheduled on any runtime.
#[cfg(feature = "smol")]
#[tokio::test(flavor = "multi_thread")]
async fn test_short_deadline_downloads_on_smol() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("smol.bin");
    let adapter = range_adapter().build().unwrap();
    let token = CancellationToken::new();
    let mut task = builder(
        &adapter,
        &dest,
        &token,
        TaskOptions::default().no_progress_timeout(Duration::from_millis(300)),
    )
    .threaded_runtime(ThreadedRuntimeImpl::new_smol_rt())
    .prefer_mode(DownloadMode::Concurrent)
    .build()
    .await
    .unwrap();
    task.run().await.unwrap();
    watchdog(task.wait())
        .await
        .expect("download should succeed");
    assert_downloaded(&adapter, &dest);
}
