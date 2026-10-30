//! Comprehensive tests for Task and TaskBuilder
//!
//! This module contains test coverage for:
//! - TaskBuilder configuration and validation
//! - The task handle: await, cancel, drop, detach, panic
//! - Actual file download with hash verification
//! - Error handling scenarios

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use bolt_load_tests::adapter::simple::{
    SimpleTestAdapter, SimpleTestAdapterBuilder, calculate_blake3,
};
use bolt_load_utils::telemetry::*;
use futures::task::{FutureObj, Spawn, SpawnError};
use pretty_assertions::assert_eq;
use smol_cancellation_token::CancellationToken;
use tempfile::TempDir;

use super::test_support::{assert_transitions, record, tmp_path, watchdog};
use crate::{
    adapter::{AdapterError, AnyBytesStream, BoltLoadAdapter, BoltLoadAdapterMeta},
    runtime::{
        DowncastLocalRuntime, Sleep, ThreadedRuntime, ThreadedRuntimeImpl, TimerBuilder, TimerImpl,
    },
    task::{
        BuildError, DownloadError, DownloadMode, StatusReceiver, Task, TaskBuilder, TaskOutput,
        TaskState, TaskStatus,
    },
};

/// Create test runtime
#[expect(
    clippy::needless_return,
    reason = "each runtime returns from its own cfg block"
)]
#[allow(unreachable_code)] // Only with both runtime features enabled.
fn create_test_runtime() -> ThreadedRuntimeImpl {
    #[cfg(feature = "tokio")]
    {
        use crate::runtime::TokioThreadedRuntime;
        return ThreadedRuntimeImpl::Tokio(TokioThreadedRuntime::new());
    }

    #[cfg(feature = "smol")]
    {
        return ThreadedRuntimeImpl::new_smol_rt();
    }

    #[cfg(not(any(feature = "tokio", feature = "smol")))]
    panic!("No runtime feature enabled for tests");
}

fn builder(adapter: impl BoltLoadAdapter + 'static) -> TaskBuilder {
    Task::builder(adapter).runtime(create_test_runtime())
}

fn assert_hash(path: &std::path::Path, expected_hash: &str, size: usize) {
    let content = std::fs::read(path).unwrap();
    assert_eq!(
        content.len(),
        size,
        "Downloaded file should have correct size"
    );
    assert_eq!(
        calculate_blake3(&content),
        expected_hash,
        "Downloaded file hash should match expected hash"
    );
}

// ============================================================================
// TaskBuilder Tests
// ============================================================================

#[tokio::test]
#[n0_tracing_test::traced_test]
async fn test_task_builder_basic_configuration() {
    let temp_dir = TempDir::new().unwrap();
    let save_path = temp_dir.path().join("basic_config.bin");

    let task = builder(SimpleTestAdapter::new(1024))
        .save_to(&save_path)
        .prefer_mode(DownloadMode::Singleton)
        .spawn()
        .expect("TaskBuilder should successfully spawn the task");

    let output = watchdog(task).await.unwrap();
    assert_eq!(output.path, save_path);
    assert_eq!(output.mode, DownloadMode::Singleton);
}

#[tokio::test]
#[n0_tracing_test::traced_test]
async fn test_task_builder_validation_errors() {
    let adapter = SimpleTestAdapter::new(1024);
    let temp_dir = TempDir::new().unwrap();
    let save_path = temp_dir.path().join("validation_test.bin");
    let invalid_destination = |builder: TaskBuilder| {
        let result = builder.spawn();
        assert!(
            matches!(result, Err(BuildError::InvalidDestination { .. })),
            "{result:?}"
        );
    };

    // Missing runtime
    let result = Task::builder(adapter.clone()).save_to(&save_path).spawn();
    assert!(
        matches!(result, Err(BuildError::MissingRuntime)),
        "{result:?}"
    );

    // Missing destination
    let result = builder(adapter.clone()).spawn();
    assert!(
        matches!(result, Err(BuildError::MissingDestination)),
        "{result:?}"
    );

    // Invalid options
    let result = builder(adapter.clone())
        .save_to(&save_path)
        .options(crate::task::TaskOptions::default().connect_timeout(Duration::ZERO))
        .spawn();
    assert!(
        matches!(result, Err(BuildError::InvalidOptions { .. })),
        "{result:?}"
    );

    // The parent directory does not exist
    invalid_destination(
        builder(adapter.clone()).save_to(temp_dir.path().join("nonexistent/test.bin")),
    );

    // The parent directory is not a directory
    let parent = temp_dir.path().join("nonexistent");
    std::fs::write(&parent, b"not a directory").unwrap();
    invalid_destination(builder(adapter.clone()).save_to(parent.join("test.bin")));

    // The destination is a directory
    invalid_destination(builder(adapter.clone()).save_to(temp_dir.path()));

    // `save_into` a missing directory
    invalid_destination(builder(adapter.clone()).save_into(temp_dir.path().join("missing")));

    // `validate` reports the same, and the cancel token is optional
    builder(adapter.clone())
        .save_to(&save_path)
        .validate()
        .unwrap();
    assert_eq!(adapter.call_count(), 0, "validation sends no request");
}

#[tokio::test(flavor = "multi_thread")]
#[n0_tracing_test::traced_test]
async fn test_task_builder_with_mode_preference() {
    let temp_dir = TempDir::new().unwrap();

    let singleton = builder(SimpleTestAdapter::new(1024).with_range_support(true))
        .save_to(temp_dir.path().join("singleton.bin"))
        .prefer_mode(DownloadMode::Singleton)
        .spawn()
        .unwrap();
    let concurrent = builder(SimpleTestAdapter::new(1024).with_range_support(true))
        .save_to(temp_dir.path().join("concurrent.bin"))
        .prefer_mode(DownloadMode::Concurrent)
        .spawn()
        .unwrap();

    assert_eq!(
        watchdog(singleton).await.unwrap().mode,
        DownloadMode::Singleton
    );
    assert_eq!(
        watchdog(concurrent).await.unwrap().mode,
        DownloadMode::Concurrent
    );
}

/// Counts the metadata requests of the adapter it wraps.
#[derive(Clone)]
struct MetaCounting {
    inner: SimpleTestAdapter,
    meta_requests: Arc<AtomicUsize>,
}

impl BoltLoadAdapter for MetaCounting {
    async fn is_range_stream_available(&self) -> bool {
        self.inner.is_range_stream_available().await
    }

    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError> {
        self.meta_requests.fetch_add(1, Ordering::SeqCst);
        self.inner.retrieve_meta().await
    }

    async fn full_stream(&self) -> Result<AnyBytesStream, AdapterError> {
        self.inner.full_stream().await
    }

    async fn range_stream(&self, start: u64, end: u64) -> Result<AnyBytesStream, AdapterError> {
        self.inner.range_stream(start, end).await
    }
}

/// S2: retrieve the metadata to choose where to save, then spawn without a
/// second request.
#[tokio::test]
#[n0_tracing_test::traced_test]
async fn test_task_builder_meta_retrieval() {
    let meta_requests = Arc::new(AtomicUsize::new(0));
    let adapter = MetaCounting {
        inner: SimpleTestAdapter::new(5000).with_filename("custom_file.txt".to_string()),
        meta_requests: meta_requests.clone(),
    };
    let temp_dir = TempDir::new().unwrap();

    let mut builder = builder(adapter);
    let meta = builder.retrieve_meta().await.unwrap();
    assert_eq!(meta.content_size, 5000);
    assert_eq!(meta.filename, Some("custom_file.txt".to_string()));
    // Cached: a second call sends no request either.
    builder.retrieve_meta().await.unwrap();

    let output = watchdog(builder.save_into(temp_dir.path()).spawn().unwrap())
        .await
        .unwrap();
    assert_eq!(output.path, temp_dir.path().join("custom_file.txt"));
    assert_eq!(meta_requests.load(Ordering::SeqCst), 1);
}

/// BUILD-6: a cloned builder spawns an independent task.
#[tokio::test]
#[n0_tracing_test::traced_test]
async fn test_cloned_builders_spawn_independent_tasks() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = SimpleTestAdapter::new(4096);
    let expected_hash = adapter.expected_hash().to_string();
    let template = builder(adapter).prefer_mode(DownloadMode::Singleton);

    let first = template
        .clone()
        .save_to(temp_dir.path().join("a.bin"))
        .spawn()
        .unwrap();
    let second = template
        .save_to(temp_dir.path().join("b.bin"))
        .spawn()
        .unwrap();
    let (first, second) = watchdog(async { tokio::join!(first, second) }).await;

    assert_hash(&first.unwrap().path, &expected_hash, 4096);
    assert_hash(&second.unwrap().path, &expected_hash, 4096);
}

// ============================================================================
// Task Handle Tests
// ============================================================================

#[tokio::test]
#[n0_tracing_test::traced_test]
async fn test_task_basic_lifecycle() {
    let temp_dir = TempDir::new().unwrap();
    let save_path = temp_dir.path().join("lifecycle_test.bin");
    let task = builder(SimpleTestAdapter::new(2048).with_range_support(false))
        .save_to(&save_path)
        .spawn()
        .unwrap();
    let rx = task.status();

    let output = watchdog(task).await.unwrap();

    let status = rx.borrow();
    assert_eq!(status.state, TaskState::Finished);
    assert!(status.is_terminal());
    assert_eq!(status.fraction(), Some(1.0));
    assert_eq!(output.mode, DownloadMode::Singleton);
    assert_eq!(output.progress.downloaded, 2048);
    assert!(save_path.exists());
}

/// Throttled so the download outlives the test's actions.
fn slow_adapter() -> SimpleTestAdapter {
    SimpleTestAdapterBuilder::new()
        .content_size(1024 * 1024)
        .support_range(true)
        .max_speed(256 * 1024)
        .build()
        .unwrap()
}

async fn wait_until_downloading(status: &mut StatusReceiver) {
    watchdog(status.wait_for(|s| s.progress.downloaded > 0))
        .await
        .expect("the task should start downloading");
}

#[tokio::test(flavor = "multi_thread")]
#[n0_tracing_test::traced_test]
async fn test_task_stop() {
    let temp_dir = TempDir::new().unwrap();
    let save_path = temp_dir.path().join("cancel_test.bin");
    let parent = CancellationToken::new();
    let task = builder(slow_adapter())
        .save_to(&save_path)
        .cancel_token(parent.clone())
        .spawn()
        .unwrap();
    let mut status = task.status();
    wait_until_downloading(&mut status).await;

    let result = watchdog(task.stop()).await;

    assert!(
        matches!(result, Err(DownloadError::Cancelled)),
        "{result:?}"
    );
    let last = status.borrow();
    assert_eq!(last.state, TaskState::Cancelled);
    assert!(
        last.progress.downloaded > 0,
        "progress is kept when cancelled"
    );
    assert!(
        !parent.is_cancelled(),
        "the caller's token is never cancelled"
    );
    assert!(!save_path.exists() && !tmp_path(&save_path).exists());
}

/// The handle's token cancels only its own task.
#[tokio::test(flavor = "multi_thread")]
#[n0_tracing_test::traced_test]
async fn test_task_cancel_token_is_a_child() {
    let temp_dir = TempDir::new().unwrap();
    let parent = CancellationToken::new();
    let task = builder(slow_adapter())
        .save_to(temp_dir.path().join("child.bin"))
        .cancel_token(parent.clone())
        .spawn()
        .unwrap();

    task.cancel_token().cancel();

    assert!(matches!(
        watchdog(task).await,
        Err(DownloadError::Cancelled)
    ));
    assert!(!parent.is_cancelled());
}

/// S4: cancelling the parent token cancels every running task.
#[tokio::test(flavor = "multi_thread")]
#[n0_tracing_test::traced_test]
async fn test_parent_token_cancels_every_task() {
    let temp_dir = TempDir::new().unwrap();
    let parent = CancellationToken::new();
    let template = builder(slow_adapter()).cancel_token(parent.clone());
    let tasks: Vec<Task> = (0..3)
        .map(|i| {
            template
                .clone()
                .save_to(temp_dir.path().join(format!("{i}.bin")))
                .spawn()
                .unwrap()
        })
        .collect();

    parent.cancel();

    for result in watchdog(futures::future::join_all(tasks)).await {
        assert!(
            matches!(result, Err(DownloadError::Cancelled)),
            "{result:?}"
        );
    }
}

/// S6: dropping the handle cancels the download and removes its temporary
/// file; a receiver taken before observes `Cancelled`, then `Closed`.
#[tokio::test(flavor = "multi_thread")]
#[n0_tracing_test::traced_test]
async fn test_dropping_the_task_cancels_it() {
    let temp_dir = TempDir::new().unwrap();
    let save_path = temp_dir.path().join("dropped.bin");
    let adapter = slow_adapter();
    let task = builder(adapter.clone())
        .save_to(&save_path)
        .spawn()
        .unwrap();
    let mut status = task.status();
    wait_until_downloading(&mut status).await;

    drop(task);

    let statuses = record(status).await;
    assert_transitions(&statuses);
    assert_eq!(statuses.last().unwrap().state, TaskState::Cancelled);
    assert!(!save_path.exists() && !tmp_path(&save_path).exists());
    assert_eq!(adapter.active_streams(), 0);
}

/// S6: a task dropped before its attempt starts still ends in `Cancelled`
/// without sending a request.
#[tokio::test(flavor = "multi_thread")]
#[n0_tracing_test::traced_test]
async fn test_dropping_the_task_before_it_starts() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = SimpleTestAdapter::new(1024);
    let task = builder(adapter.clone())
        .save_to(temp_dir.path().join("never.bin"))
        .spawn()
        .unwrap();
    let status = task.status();
    drop(task);

    let statuses = record(status).await;
    assert_transitions(&statuses);
    assert_eq!(statuses.last().unwrap().state, TaskState::Cancelled);
    assert_eq!(adapter.call_count(), 0);
}

/// TASK-4: a detached task keeps running and stays observable.
#[tokio::test(flavor = "multi_thread")]
#[n0_tracing_test::traced_test]
async fn test_detached_task_keeps_running() {
    let temp_dir = TempDir::new().unwrap();
    let save_path = temp_dir.path().join("detached.bin");
    let adapter = SimpleTestAdapter::new(64 * 1024).with_range_support(true);
    let expected_hash = adapter.expected_hash().to_string();
    let task = builder(adapter).save_to(&save_path).spawn().unwrap();
    let status = task.status();

    task.detach();

    let statuses = record(status).await;
    assert_transitions(&statuses);
    assert_eq!(statuses.last().unwrap().state, TaskState::Finished);
    assert_hash(&save_path, &expected_hash, 64 * 1024);
}

/// Panics in its stream, inline in the singleton attempt.
#[derive(Clone)]
struct PanickingAdapter(SimpleTestAdapter);

impl BoltLoadAdapter for PanickingAdapter {
    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError> {
        self.0.retrieve_meta().await
    }

    async fn full_stream(&self) -> Result<AnyBytesStream, AdapterError> {
        panic!("the adapter panicked");
    }
}

/// S8, TASK-8: a panic becomes `Failed(Panicked)` instead of unwinding.
#[tokio::test(flavor = "multi_thread")]
#[n0_tracing_test::traced_test]
async fn test_panic_becomes_an_error() {
    let temp_dir = TempDir::new().unwrap();
    let save_path = temp_dir.path().join("panic.bin");
    let task = builder(PanickingAdapter(SimpleTestAdapter::new(1024)))
        .save_to(&save_path)
        .spawn()
        .unwrap();
    let status = task.status();

    let result = watchdog(task).await;

    match result {
        Err(DownloadError::Panicked { message }) => {
            assert!(message.contains("the adapter panicked"), "{message}")
        }
        other => panic!("expected Panicked, got {other:?}"),
    }
    let statuses = record(status).await;
    assert_transitions(&statuses);
    assert_eq!(statuses.last().unwrap().state, TaskState::Failed);
    assert!(!save_path.exists() && !tmp_path(&save_path).exists());
}

/// Delegates to tokio but cannot provide a local runtime.
struct NoLocalRuntime(ThreadedRuntimeImpl);

impl Spawn for NoLocalRuntime {
    fn spawn_obj(&self, future: FutureObj<'static, ()>) -> Result<(), SpawnError> {
        self.0.spawn_obj(future)
    }
}

impl DowncastLocalRuntime for NoLocalRuntime {}

impl TimerBuilder for NoLocalRuntime {
    fn create_delayed_timer(&self, duration: Duration) -> TimerImpl {
        self.0.create_delayed_timer(duration)
    }

    fn sleep(&self, duration: Duration) -> Sleep {
        self.0.sleep(duration)
    }
}

impl ThreadedRuntime for NoLocalRuntime {}

/// INIT-3: a runtime without a local runtime fails the task instead of
/// panicking.
#[tokio::test]
#[n0_tracing_test::traced_test]
async fn test_missing_local_runtime_fails_the_task() {
    let temp_dir = TempDir::new().unwrap();
    let task = Task::builder(SimpleTestAdapter::new(1024))
        .runtime(ThreadedRuntimeImpl::new_other_rt(NoLocalRuntime(
            create_test_runtime(),
        )))
        .save_to(temp_dir.path().join("no-local.bin"))
        .spawn()
        .unwrap();
    let status = task.status();

    let result = watchdog(task).await;

    assert!(
        matches!(result, Err(DownloadError::Runtime { .. })),
        "{result:?}"
    );
    assert_eq!(status.borrow().state, TaskState::Failed);
}

// ============================================================================
// Destination Tests
// ============================================================================

/// S7: a hostile filename is reduced to its final segment inside the
/// directory; one that is not a file name fails without writing.
#[tokio::test]
#[n0_tracing_test::traced_test]
async fn test_save_into_rejects_hostile_filenames() {
    let temp_dir = TempDir::new().unwrap();
    let dir = temp_dir.path().join("downloads");
    std::fs::create_dir(&dir).unwrap();

    let adapter = SimpleTestAdapter::new(1024).with_filename("../../evil.bin".to_string());
    let output = watchdog(builder(adapter).save_into(&dir).spawn().unwrap())
        .await
        .unwrap();
    assert_eq!(output.path, dir.join("evil.bin"));
    assert!(!temp_dir.path().join("evil.bin").exists());

    let adapter = SimpleTestAdapter::new(1024).with_filename("..".to_string());
    let result = watchdog(builder(adapter).save_into(&dir).spawn().unwrap()).await;
    assert!(
        matches!(&result, Err(DownloadError::InvalidFilename { name }) if name == ".."),
        "{result:?}"
    );

    let adapter = SimpleTestAdapterBuilder::new()
        .content_size(1024)
        .without_filename()
        .build()
        .unwrap();
    let result = watchdog(builder(adapter).save_into(&dir).spawn().unwrap()).await;
    assert!(
        matches!(result, Err(DownloadError::MissingFilename)),
        "{result:?}"
    );

    let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
    assert_eq!(entries.len(), 1, "only evil.bin was written: {entries:?}");
}

// ============================================================================
// Actual Download Tests with Hash Verification
// ============================================================================

async fn download_and_verify(adapter: SimpleTestAdapter, size: usize, name: &str) -> Duration {
    let expected_hash = adapter.expected_hash().to_string();
    let temp_dir = TempDir::new().unwrap();
    let save_path = temp_dir.path().join(name);
    let task = builder(adapter).save_to(&save_path).spawn().unwrap();

    let start_time = Instant::now();
    let result = task.await;
    let duration = start_time.elapsed();

    assert!(
        result.is_ok(),
        "Download should complete successfully: {result:?}"
    );
    assert_hash(&save_path, &expected_hash, size);
    duration
}

#[tokio::test]
#[n0_tracing_test::traced_test]
async fn test_small_file_download_with_hash_verification() {
    let adapter = SimpleTestAdapter::new(10240).with_range_support(false); // 10KB
    let duration = download_and_verify(adapter, 10240, "small_download.bin").await;
    info!("✓ Small file download test passed in {duration:?}");
}

#[tokio::test]
#[n0_tracing_test::traced_test]
async fn test_medium_file_download_with_hash_verification() {
    let adapter = SimpleTestAdapter::new(1024 * 1024).with_range_support(false); // 1MB
    let duration = download_and_verify(adapter, 1024 * 1024, "medium_download.bin").await;
    info!("✓ Medium file download test passed in {duration:?}");
}

#[tokio::test(flavor = "multi_thread")]
#[n0_tracing_test::traced_test]
async fn test_large_file_download_with_hash_verification() {
    let adapter = SimpleTestAdapter::new_large().with_range_support(true); // 100MB
    let duration = download_and_verify(adapter, 100 * 1024 * 1024, "large_download.bin").await;
    let speed_mbps = (100.0 / duration.as_secs_f64()).round();
    info!("🎉 Large file download test passed in {duration:?}, {speed_mbps} MB/s");
}

// ============================================================================
// Error Handling Tests
// ============================================================================

#[tokio::test]
#[n0_tracing_test::traced_test]
async fn test_adapter_failure_handling() {
    let temp_dir = TempDir::new().unwrap();
    let task = builder(SimpleTestAdapter::new(1024).with_failure(true))
        .save_to(temp_dir.path().join("failure_test.bin"))
        .spawn()
        .expect("spawning sends no request, so it succeeds");

    let result = watchdog(task).await;

    assert!(
        matches!(result, Err(DownloadError::Metadata { .. })),
        "{result:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[n0_tracing_test::traced_test]
#[ignore = "an empty stream of unknown size fails with `Empty`, see #153"]
async fn test_zero_size_file_handling() {
    let temp_dir = TempDir::new().unwrap();
    let save_path = temp_dir.path().join("zero_size_test.bin");
    let task = builder(SimpleTestAdapter::new(0))
        .save_to(&save_path)
        .spawn()
        .unwrap();

    let output = watchdog(task).await.expect("should be ok");

    assert_eq!(output.mode, DownloadMode::Singleton);
    let content = std::fs::read(&save_path).unwrap();
    assert!(content.is_empty(), "Zero-size file should be empty");
}

// ============================================================================
// Performance Tests
// ============================================================================

async fn timed(task: Task) -> (TaskOutput, Duration) {
    let start_time = Instant::now();
    let output = task.await.expect("the task should succeed");
    (output, start_time.elapsed())
}

#[tokio::test(flavor = "multi_thread")]
#[n0_tracing_test::traced_test]
async fn test_concurrent_vs_singleton_performance() {
    let file_size = 1024 * 1024 * 1024; // 1GB for reasonable test time
    let temp_dir = TempDir::new().unwrap();
    let adapter1 = SimpleTestAdapter::new(file_size).with_range_support(true);
    let adapter2 = adapter1.clone_reset_count().with_range_support(false);
    let concurrent_expected_hash = adapter1.expected_hash().to_string();
    let singleton_expected_hash = adapter2.expected_hash().to_string();
    let runtime = ThreadedRuntimeImpl::new_tokio_rt();

    let concurrent = Task::builder(adapter1)
        .runtime(runtime.clone())
        .save_to(temp_dir.path().join("concurrent.bin"))
        .prefer_mode(DownloadMode::Concurrent)
        .spawn()
        .unwrap();
    let (concurrent, concurrent_duration) = timed(concurrent).await;
    let singleton = Task::builder(adapter2)
        .runtime(runtime)
        .save_to(temp_dir.path().join("singleton.bin"))
        .prefer_mode(DownloadMode::Singleton)
        .spawn()
        .unwrap();
    let (singleton, singleton_duration) = timed(singleton).await;

    assert_hash(&concurrent.path, &concurrent_expected_hash, file_size);
    assert_hash(&singleton.path, &singleton_expected_hash, file_size);
    let mib = (1024 * 1024) as f64;
    info!(
        "concurrent: {:.0} MB/s in {concurrent_duration:?}; singleton: {:.0} MB/s in \
         {singleton_duration:?}",
        file_size as f64 / concurrent_duration.as_secs_f64() / mib,
        file_size as f64 / singleton_duration.as_secs_f64() / mib,
    );
}

#[tokio::test(flavor = "multi_thread")]
#[n0_tracing_test::traced_test]
async fn test_concurrent_performance_only() {
    let file_size = 1024 * 1024 * 1024; // 1GB for reasonable test time
    let per_stream_speed = 10 * 1024 * 1024; // 10MB/s per stream
    let max_speed = per_stream_speed * 10; // 100MB/s total speed
    let temp_dir = TempDir::new().unwrap();
    // Use max_per_stream_speed to simulate slow downloads.
    let adapter = SimpleTestAdapterBuilder::new()
        .content_size(file_size)
        .support_range(true)
        .max_per_stream_speed(per_stream_speed)
        .max_speed(max_speed)
        .build()
        .expect("Failed to build adapter");
    let expected_hash = adapter.expected_hash().to_string();

    let task = Task::builder(adapter)
        .runtime(ThreadedRuntimeImpl::new_tokio_rt())
        .save_to(temp_dir.path().join("concurrent_performance_test.bin"))
        .prefer_mode(DownloadMode::Concurrent)
        .spawn()
        .unwrap();
    let (output, duration) = timed(task).await;

    assert_hash(&output.path, &expected_hash, file_size);
    info!(
        "concurrent: {:.0} MB/s in {duration:?}",
        file_size as f64 / duration.as_secs_f64() / (1024.0 * 1024.0)
    );
}

#[tokio::test(flavor = "multi_thread")]
#[n0_tracing_test::traced_test]
async fn test_concurrent_task_with_range_stream() {
    let adapter = SimpleTestAdapter::new(1024 * 1024).with_range_support(true);
    let duration = download_and_verify(adapter, 1024 * 1024, "concurrent_range_test.bin").await;
    info!("✓ Concurrent task with range stream test passed in {duration:?}");
}

#[tokio::test(flavor = "multi_thread")]
#[n0_tracing_test::traced_test]
async fn test_concurrent_task_with_max_limit() {
    let file_size = 1024 * 1024 * 1024;
    let per_stream_speed = 10 * 1024 * 1024; // 10MB/s per stream
    let max_speed = per_stream_speed * 10; // 100MB/s total speed
    let adapter = SimpleTestAdapterBuilder::new()
        .support_range(true)
        .max_concurrent_streams(2)
        .max_per_stream_speed(per_stream_speed)
        .max_speed(max_speed)
        .content_size(file_size)
        .build()
        .unwrap();
    let duration = download_and_verify(adapter, file_size, "concurrent_max_limit.bin").await;
    info!(
        "concurrent with a stream limit: {:.0} MB/s in {duration:?}",
        file_size as f64 / duration.as_secs_f64() / (1024.0 * 1024.0)
    );
}

// ============================================================================
// Type Tests
// ============================================================================

/// TASK-7: the public handles and their futures are `Send + Sync`.
#[test]
fn test_public_types_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_send<T: Send>(_: &T) {}
    assert_send_sync::<Task>();
    assert_send_sync::<TaskBuilder>();
    assert_send_sync::<TaskStatus>();
    assert_send_sync::<TaskOutput>();
    assert_send_sync::<DownloadError>();
    assert_send_sync::<BuildError>();
    assert_send_sync::<StatusReceiver>();
    let (_publisher, mut rx) = crate::task::status::StatusPublisher::new();
    assert_send(&rx.wait_for(|_| true));
    assert_send(&rx.changed());
    let mut builder = Task::builder(SimpleTestAdapter::new(1));
    assert_send(&builder.retrieve_meta());
}
