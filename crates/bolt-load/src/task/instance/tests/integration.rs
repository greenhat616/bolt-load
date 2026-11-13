//! Integration tests for both download modes:
//! 1. Basic download functionality
//! 2. Task cancellation
//! 3. Error handling
//! 4. Comparison between the modes

use std::time::Duration;

use bolt_load_tests::adapter::simple::{
    SimpleTestAdapter, SimpleTestAdapterBuilder, calculate_blake3,
};
use bolt_load_utils::telemetry::*;
use pretty_assertions::assert_eq;
use smol_cancellation_token::CancellationToken;
use tempfile::TempDir;

use crate::task::{
    DownloadError, DownloadMode, TaskState,
    test_support::{assert_transitions, record, spawn, tmp_path, watchdog},
};

const TEST_FILE_SIZE: usize = 4 * 1024 * 1024; // 4MB
const CHUNK_SIZE: usize = 8 * 1024; // 8KB

fn assert_content(path: &std::path::Path, expected_hash: &str) {
    let downloaded = std::fs::read(path).unwrap();
    assert_eq!(
        calculate_blake3(&downloaded),
        expected_hash,
        "Downloaded file content should match original content"
    );
}

#[tokio::test]
#[n0_tracing_test::traced_test]
async fn test_singleton_basic_functionality() {
    let adapter = SimpleTestAdapter::new(TEST_FILE_SIZE).with_chunk_size(CHUNK_SIZE);
    let content_hash = adapter.expected_hash().to_string();
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("singleton_test.bin");

    let task = spawn(
        DownloadMode::Singleton,
        adapter,
        &file_path,
        &CancellationToken::new(),
    );
    let statuses = record(task.status()).await;
    let output = watchdog(task).await.unwrap();

    assert_transitions(&statuses);
    assert_eq!(output.progress.total, Some(TEST_FILE_SIZE as u64));
    assert_eq!(output.progress.downloaded, TEST_FILE_SIZE as u64);
    assert_content(&file_path, &content_hash);
}

#[tokio::test(flavor = "multi_thread")]
#[n0_tracing_test::traced_test]
async fn test_concurrent_basic_functionality() {
    let adapter = SimpleTestAdapterBuilder::new()
        .content_size(TEST_FILE_SIZE)
        .chunk_size(CHUNK_SIZE)
        .support_range(true)
        .build()
        .expect("Failed to build adapter");
    let content_hash = adapter.expected_hash().to_string();
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("concurrent_test.bin");

    let task = spawn(
        DownloadMode::Concurrent,
        adapter,
        &file_path,
        &CancellationToken::new(),
    );
    let statuses = record(task.status()).await;
    let output = watchdog(task).await.unwrap();

    assert_transitions(&statuses);
    assert_eq!(output.mode, DownloadMode::Concurrent);
    assert_eq!(output.progress.total, Some(TEST_FILE_SIZE as u64));
    assert_eq!(output.progress.downloaded, TEST_FILE_SIZE as u64);
    assert_content(&file_path, &content_hash);
}

#[tokio::test(flavor = "multi_thread")]
#[n0_tracing_test::traced_test]
async fn test_task_cancellation() {
    let adapter = SimpleTestAdapter::new(TEST_FILE_SIZE).with_chunk_size(CHUNK_SIZE);
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("cancel_test.bin");
    let cancel_token = CancellationToken::new();

    let task = spawn(DownloadMode::Singleton, adapter, &file_path, &cancel_token);
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel_token.cancel();
    let rx = task.status();
    let result = watchdog(task).await;

    match result {
        Err(e) => {
            assert!(e.is_cancelled(), "{e:?}");
            assert_eq!(rx.borrow().state, TaskState::Cancelled);
            assert!(!file_path.exists() && !tmp_path(&file_path).exists());
        }
        Ok(_) => info!("the task completed before the cancellation, which is also normal"),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[n0_tracing_test::traced_test]
async fn test_adapter_failure_handling() {
    let adapter = SimpleTestAdapter::new(TEST_FILE_SIZE)
        .with_chunk_size(CHUNK_SIZE)
        .with_failure(true);
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("failure_test.bin");

    let task = spawn(
        DownloadMode::Singleton,
        adapter,
        &file_path,
        &CancellationToken::new(),
    );
    let result = watchdog(task).await;

    assert!(
        matches!(
            result,
            Err(DownloadError::Metadata { .. } | DownloadError::Transfer { .. })
        ),
        "Task should fail when adapter fails: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[n0_tracing_test::traced_test]
async fn test_task_modes_comparison() {
    let adapter = SimpleTestAdapter::new(TEST_FILE_SIZE)
        .with_chunk_size(CHUNK_SIZE)
        .with_range_support(true);
    let content_hash = adapter.expected_hash().to_string();
    let temp_dir = TempDir::new().unwrap();
    let singleton_path = temp_dir.path().join("singleton.bin");
    let concurrent_path = temp_dir.path().join("concurrent.bin");

    let singleton = spawn(
        DownloadMode::Singleton,
        adapter.clone(),
        &singleton_path,
        &CancellationToken::new(),
    );
    let concurrent = spawn(
        DownloadMode::Concurrent,
        adapter,
        &concurrent_path,
        &CancellationToken::new(),
    );
    let (singleton, concurrent) = watchdog(async { tokio::join!(singleton, concurrent) }).await;

    assert_eq!(singleton.unwrap().mode, DownloadMode::Singleton);
    assert_eq!(concurrent.unwrap().mode, DownloadMode::Concurrent);
    assert_content(&singleton_path, &content_hash);
    assert_content(&concurrent_path, &content_hash);
}
