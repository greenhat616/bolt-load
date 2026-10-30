//! Regression tests for issue #102: failures terminate with an error, and
//! stop/wait only return once every owned worker has exited.

use std::{path::Path, time::Duration};

use bolt_load_tests::adapter::{
    fault::{FaultKind, FaultPlan, Gate},
    simple::{SimpleTestAdapter, SimpleTestAdapterBuilder, calculate_blake3},
};
use smol_cancellation_token::CancellationToken;
use tempfile::TempDir;

use crate::{
    adapter::AdapterError,
    runner::TaskError,
    runtime::ThreadedRuntimeImpl,
    task::{
        DownloadError, DownloadMode, IoOp, Task, TaskBuilder, TaskOptions, TaskOutput, TaskState,
        test_support::{tmp_path, watchdog},
    },
};

const SIZE: usize = 256 * 1024;

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
    Task::builder(adapter.clone())
        .save_to(dest)
        .cancel_token(token.clone())
        .runtime(ThreadedRuntimeImpl::new_tokio_rt())
        .options(options)
}

fn start(
    adapter: &SimpleTestAdapter,
    dest: &Path,
    token: &CancellationToken,
    options: TaskOptions,
    mode: DownloadMode,
) -> Task {
    builder(adapter, dest, token, options)
        .prefer_mode(mode)
        .spawn()
        .expect("task should spawn")
}

/// The transfer error of a failed task; a cancellation is `Cancelled`.
fn task_error(result: Result<TaskOutput, DownloadError>) -> TaskError {
    match result {
        Err(DownloadError::Transfer { source }) => (*source).clone(),
        Err(DownloadError::Cancelled) => TaskError::Cancelled,
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
    let task = start(&adapter, &dest, &token, options, DownloadMode::Concurrent);

    match task_error(watchdog(task).await) {
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
    let task = start(
        &adapter,
        &dest,
        &token,
        TaskOptions::default(),
        DownloadMode::Concurrent,
    );

    match task_error(watchdog(task).await) {
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
    let task = start(&adapter, &dest, &token, options, DownloadMode::Concurrent);

    match task_error(watchdog(task).await) {
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
    let task = start(&adapter, &dest, &token, options, DownloadMode::Concurrent);

    watchdog(task).await.expect("download should recover");
    assert_downloaded(&adapter, &dest);
    assert!(!token.is_cancelled());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_hung_connect_times_out_while_others_progress() {
    const SIZE: usize = 32 * 1024 * 1024;
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("connect_timeout.bin");
    // The first runner keeps progressing, so the no-progress deadline never
    // comes; only the connect timeout ends the hung second connect.
    let adapter = SimpleTestAdapterBuilder::new()
        .content_size(SIZE)
        .support_range(true)
        .max_per_stream_speed(4 * 1024 * 1024)
        .range_connect_plan(FaultPlan::HangAt(1))
        .build()
        .unwrap();
    let mut options = TaskOptions::default().connect_timeout(Duration::from_millis(300));
    // Room for the strategy to split, whatever the CPU count.
    options.max_concurrency = Some(4);
    let token = CancellationToken::new();
    let task = start(&adapter, &dest, &token, options, DownloadMode::Concurrent);

    watchdog(task)
        .await
        .expect("the hung range should be retried");
    assert_downloaded(&adapter, &dest);
    assert!(
        adapter.range_attempts() >= 3,
        "{}",
        adapter.range_attempts()
    );
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
    let task = start(
        &adapter,
        &dest,
        &token,
        TaskOptions::default(),
        DownloadMode::Concurrent,
    );

    watchdog(gate.entered()).await;
    let result = watchdog(task.stop()).await;
    assert_cleaned_up(&adapter, &dest);
    assert!(task_error(result).is_cancelled());
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
    let token = CancellationToken::new();
    let task = start(
        &stalled,
        &dest,
        &token,
        TaskOptions::default(),
        DownloadMode::Concurrent,
    );
    // Cancel only after data has been handed to the writer.
    let mut status = task.status();
    watchdog(status.wait_for(|s| s.progress.downloaded > 0))
        .await
        .unwrap();
    let _ = watchdog(task.stop()).await;
    assert_cleaned_up(&stalled, &dest);

    let adapter = range_adapter().build().unwrap();
    let token = CancellationToken::new();
    let task = start(
        &adapter,
        &dest,
        &token,
        TaskOptions::default(),
        DownloadMode::Concurrent,
    );
    watchdog(task).await.expect("redownload should succeed");
    assert_downloaded(&adapter, &dest);
}

// T9
#[tokio::test(flavor = "multi_thread")]
async fn test_initialization_is_cancellable() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("t9.bin");

    // Cancelled before spawning.
    let adapter = range_adapter().build().unwrap();
    let token = CancellationToken::new();
    token.cancel();
    let task = builder(&adapter, &dest, &token, TaskOptions::default())
        .spawn()
        .unwrap();
    assert!(matches!(
        watchdog(task).await,
        Err(DownloadError::Cancelled)
    ));
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
        let task = builder(&adapter, &dest, &token, TaskOptions::default())
            .spawn()
            .unwrap();
        watchdog(gate.entered()).await;
        assert_eq!(task.snapshot().state, TaskState::Initializing);
        token.cancel();
        let result = watchdog(task).await;
        assert!(
            matches!(result, Err(DownloadError::Cancelled)),
            "{result:?}"
        );
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
    let task = start(
        &adapter,
        &dest,
        &token,
        idle.clone(),
        DownloadMode::Singleton,
    );
    watchdog(gate.entered()).await;
    assert!(task_error(watchdog(task.stop()).await).is_cancelled());
    assert_cleaned_up(&adapter, &dest);

    // Connection blocked past the connect timeout.
    let gate = Gate::new();
    let adapter = range_adapter()
        .full_connect_plan(FaultPlan::Block(gate.clone()))
        .build()
        .unwrap();
    let dest = dir.path().join("t10-connect.bin");
    let options = idle.clone().connect_timeout(Duration::from_millis(300));
    let task = start(
        &adapter,
        &dest,
        &CancellationToken::new(),
        options,
        DownloadMode::Singleton,
    );
    watchdog(gate.entered()).await;
    assert!(
        !tmp_path(&dest).exists(),
        "the file is created once connected"
    );
    assert!(matches!(
        task_error(watchdog(task).await),
        TaskError::Timeout
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
        let task = start(
            &adapter,
            &dest,
            &CancellationToken::new(),
            idle.clone(),
            DownloadMode::Singleton,
        );
        match task_error(watchdog(task).await) {
            TaskError::Timeout => {}
            other => panic!("{name}: expected Timeout, got {other:?}"),
        }
        assert_cleaned_up(&adapter, &dest);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_singleton_connect_failure_keeps_the_adapter_error() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("connect-failure.bin");
    let adapter = range_adapter()
        .full_connect_plan(FaultPlan::AlwaysFail(FaultKind::Unretryable))
        .build()
        .unwrap();
    let task = start(
        &adapter,
        &dest,
        &CancellationToken::new(),
        TaskOptions::default(),
        DownloadMode::Singleton,
    );
    assert!(matches!(
        task_error(watchdog(task).await),
        TaskError::StreamError {
            source: AdapterError::Unretryable { .. }
        }
    ));
    assert_cleaned_up(&adapter, &dest);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_singleton_file_open_failure_stops_the_runner() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("file.bin");
    // A directory in its place keeps the temporary file from being opened.
    std::fs::create_dir(tmp_path(&dest)).unwrap();
    let adapter = range_adapter().build().unwrap();
    let task = start(
        &adapter,
        &dest,
        &CancellationToken::new(),
        TaskOptions::default(),
        DownloadMode::Singleton,
    );
    assert!(matches!(
        watchdog(task).await,
        Err(DownloadError::Io {
            op: IoOp::Create,
            ..
        })
    ));
    assert_eq!(adapter.active_requests(), 0);
    assert_eq!(adapter.active_streams(), 0);
    assert!(!dest.exists());
}

// T11
#[tokio::test(flavor = "multi_thread")]
async fn test_singleton_success_keeps_caller_token() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("t11.bin");
    let adapter = range_adapter().build().unwrap();
    let token = CancellationToken::new();
    let task = start(
        &adapter,
        &dest,
        &token,
        TaskOptions::default(),
        DownloadMode::Singleton,
    );

    watchdog(task).await.expect("download should succeed");
    assert!(
        !token.is_cancelled(),
        "success must not cancel the caller's token"
    );
    assert_downloaded(&adapter, &dest);
}

// T12
#[tokio::test(flavor = "multi_thread")]
async fn test_await_is_repeatable_and_cancel_safe() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("t12.bin");
    let gate = Gate::new();
    let adapter = range_adapter()
        .range_connect_plan(FaultPlan::Block(gate.clone()))
        .build()
        .unwrap();
    let token = CancellationToken::new();
    let mut task = builder(&adapter, &dest, &token, TaskOptions::default())
        .spawn()
        .unwrap();
    watchdog(gate.entered()).await;

    // A dropped await must not detach or cancel the task.
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut task)
            .await
            .is_err()
    );
    assert!(!task.is_terminal());

    task.cancel();
    task.cancel();
    for _ in 0..2 {
        assert!(task_error(watchdog(&mut task).await).is_cancelled());
    }
    assert!(task.is_terminal());
    assert!(
        !token.is_cancelled(),
        "the caller's token is never cancelled"
    );
    assert_cleaned_up(&adapter, &dest);
}

// T14
#[tokio::test(flavor = "multi_thread")]
async fn test_failed_status_is_published_after_cleanup() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("t14.bin");
    let adapter = range_adapter()
        .range_connect_plan(FaultPlan::AlwaysFail(FaultKind::Unretryable))
        .build()
        .unwrap();
    let token = CancellationToken::new();
    let task = builder(&adapter, &dest, &token, TaskOptions::default())
        .spawn()
        .unwrap();
    let mut status = task.status();

    let terminal = watchdog(status.wait_for(|s| s.is_terminal()))
        .await
        .unwrap();
    let observed = (tmp_path(&dest).exists(), adapter.active_streams());
    assert_eq!(terminal.state, TaskState::Failed);
    assert_eq!(
        observed,
        (false, 0),
        "Failed must be published after the temporary file and streams are gone"
    );
    assert!(watchdog(task).await.is_err());
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
    let task = start(&adapter, &dest, &token, options, DownloadMode::Concurrent);

    match task_error(watchdog(task).await) {
        TaskError::RetryLimitExceeded { source, .. } => {
            assert!(matches!(*source, TaskError::Timeout), "got {source:?}")
        }
        other => panic!("expected RetryLimitExceeded, got {other:?}"),
    }
    assert_cleaned_up(&adapter, &dest);
}

// A task is one attempt: a new task from the same builder retries, and
// does not see the previous error (RERUN-1).
#[tokio::test(flavor = "multi_thread")]
async fn test_new_task_from_the_same_builder_retries() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("rerun.bin");
    let adapter = range_adapter()
        .full_connect_plan(FaultPlan::FailFirst(1, FaultKind::Unretryable))
        .build()
        .unwrap();
    let token = CancellationToken::new();
    let builder = builder(&adapter, &dest, &token, TaskOptions::default())
        .prefer_mode(DownloadMode::Singleton);

    assert!(watchdog(builder.clone().spawn().unwrap()).await.is_err());
    let task = builder.spawn().unwrap();
    let status = task.status();
    watchdog(task).await.expect("the retry should succeed");
    assert!(status.borrow().error.is_none());
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
    let task = builder(
        &adapter,
        &dest,
        &token,
        TaskOptions::default().no_progress_timeout(Duration::from_millis(300)),
    )
    .runtime(ThreadedRuntimeImpl::new_smol_rt())
    .prefer_mode(DownloadMode::Concurrent)
    .spawn()
    .unwrap();
    watchdog(task).await.expect("download should succeed");
    assert_downloaded(&adapter, &dest);
}
