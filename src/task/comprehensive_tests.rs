//! Comprehensive tests for Task and TaskBuilder
//!
//! This module contains test coverage for:
//! - TaskBuilder configuration and validation
//! - Task lifecycle management
//! - Actual file download with hash verification
//! - Error handling scenarios

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use smol_cancellation_token::CancellationToken;
use tempfile::TempDir;

use crate::{
    adapter::{
        BoltLoadAdapter,
        tests::{SimpleTestAdapter, calculate_sha256},
    },
    runtime::ThreadedRuntimeImpl,
    task::{DownloadMode, TaskBuilder, TaskEvent, TaskManagerBuildError},
};
use crate::utils::logging::*;

/// Create test runtime
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

// ============================================================================
// TaskBuilder Tests
// ============================================================================

#[tokio::test]
async fn test_task_builder_basic_configuration() {
    let runtime = create_test_runtime();
    let adapter = SimpleTestAdapter::new(1024);
    let temp_dir = TempDir::new().unwrap();
    let save_path = temp_dir.path().join("basic_test.bin");
    let cancel_token = CancellationToken::new();

    // Test basic builder configuration
    let task = TaskBuilder::default()
        .adapter(Box::new(adapter) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(save_path.clone())
        .cancel_token(cancel_token)
        .runtime(runtime)
        .build()
        .await;

    assert!(task.is_ok(), "TaskBuilder should successfully build task");

    let task = task.unwrap();
    assert!(!task.is_initialized(), "Task should start uninitialized");
    assert!(!task.is_finished(), "Task should not be finished initially");

    info!("✓ Basic TaskBuilder configuration test passed");
}

#[tokio::test]
async fn test_task_builder_validation_errors() {
    let runtime = create_test_runtime();
    let adapter = SimpleTestAdapter::new(1024);
    let temp_dir = TempDir::new().unwrap();
    let save_path = temp_dir.path().join("validation_test.bin");

    // Test missing adapter
    let result = TaskBuilder::default()
        .save_path(save_path.clone())
        .cancel_token(CancellationToken::new())
        .runtime(runtime.clone())
        .build()
        .await;

    assert!(matches!(
        result,
        Err(TaskManagerBuildError::FieldValidationFailed(_))
    ));

    // Test missing save path
    let result = TaskBuilder::default()
        .adapter(Box::new(adapter.clone()) as Box<dyn BoltLoadAdapter + Send>)
        .cancel_token(CancellationToken::new())
        .runtime(runtime.clone())
        .build()
        .await;

    assert!(matches!(
        result,
        Err(TaskManagerBuildError::FieldValidationFailed(_))
    ));

    // Test missing cancel token
    let result = TaskBuilder::default()
        .adapter(Box::new(adapter.clone()) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(save_path.clone())
        .runtime(runtime.clone())
        .build()
        .await;

    assert!(matches!(
        result,
        Err(TaskManagerBuildError::FieldValidationFailed(_))
    ));

    // Test invalid save path (parent directory does not exist)
    let result = TaskBuilder::default()
        .adapter(Box::new(adapter.clone()) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(temp_dir.path().join("nonexistent/test.bin"))
        .cancel_token(CancellationToken::new())
        .runtime(runtime.clone())
        .build()
        .await;

    assert!(matches!(
        result,
        Err(TaskManagerBuildError::FieldValidationFailed(_))
    ));

    // Test invalid save path (parent directory is not a directory)
    let parent = temp_dir.path().join("nonexistent");
    std::fs::write(&parent, b"not a directory").unwrap();
    let result = TaskBuilder::default()
        .adapter(Box::new(adapter.clone()) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(parent.join("test.bin"))
        .cancel_token(CancellationToken::new())
        .runtime(runtime.clone())
        .build()
        .await;
    assert!(matches!(
        result,
        Err(TaskManagerBuildError::FieldValidationFailed(_))
    ));

    // Test invalid save path (write a directory)
    let result = TaskBuilder::default()
        .adapter(Box::new(adapter.clone()) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(temp_dir.path().to_path_buf())
        .cancel_token(CancellationToken::new())
        .runtime(runtime.clone())
        .build()
        .await;

    assert!(matches!(
        result,
        Err(TaskManagerBuildError::FieldValidationFailed(_))
    ));

    info!("✓ TaskBuilder validation errors test passed");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_task_builder_with_mode_preference() {
    let runtime = create_test_runtime();
    let temp_dir = TempDir::new().unwrap();
    let save_path = temp_dir.path().join("mode_test.bin");

    // Test with range-supporting adapter and concurrent preference
    let adapter = SimpleTestAdapter::new(2048).with_range_support(true);
    let task = TaskBuilder::default()
        .adapter(Box::new(adapter) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(save_path.clone())
        .prefer_mode(DownloadMode::Concurrent)
        .cancel_token(CancellationToken::new())
        .runtime(runtime.clone())
        .build()
        .await
        .unwrap();

    // Task should be built successfully
    assert!(!task.is_finished());

    // Test with non-range-supporting adapter
    let adapter = SimpleTestAdapter::new(2048).with_range_support(false);
    let save_path2 = temp_dir.path().join("singleton_test.bin");
    let task = TaskBuilder::default()
        .adapter(Box::new(adapter) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(save_path2)
        .prefer_mode(DownloadMode::Concurrent)
        .cancel_token(CancellationToken::new())
        .runtime(runtime)
        .build()
        .await
        .unwrap();

    // Should still build successfully (falls back to singleton mode)
    assert!(!task.is_finished());

    info!("✓ TaskBuilder mode preference test passed");
}

#[tokio::test]
async fn test_task_builder_callback_configuration() {
    let runtime = create_test_runtime();
    let adapter = SimpleTestAdapter::new(1024);
    let temp_dir = TempDir::new().unwrap();
    let save_path = temp_dir.path().join("callback_test.bin");
    let cancel_token = CancellationToken::new();

    let callback_counter = Arc::new(AtomicUsize::new(0));
    let callback_counter_clone = callback_counter.clone();

    // Test with callback
    let task = TaskBuilder::default()
        .adapter(Box::new(adapter) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(save_path)
        .cancel_token(cancel_token)
        .runtime(runtime)
        .on_task_state_changed(move |_event: TaskEvent| {
            callback_counter_clone.fetch_add(1, Ordering::Relaxed);
        })
        .build()
        .await;

    assert!(
        task.is_ok(),
        "TaskBuilder with callback should build successfully"
    );

    let mut task = task.unwrap();

    // Run the task to trigger callbacks
    let run_result = task.run().await;
    if run_result.is_ok() {
        let _ = task.wait().await;
    }

    // Callback should have been called
    assert!(
        callback_counter.load(Ordering::Relaxed) > 0,
        "Callback should have been invoked"
    );

    info!("✓ TaskBuilder callback configuration test passed");
}

#[tokio::test]
async fn test_task_builder_meta_retrieval() {
    let runtime = create_test_runtime();
    let adapter = SimpleTestAdapter::new(5000).with_filename("custom_file.txt".to_string());
    let temp_dir = TempDir::new().unwrap();
    let cancel_token = CancellationToken::new();

    let mut builder = TaskBuilder::default()
        .adapter(Box::new(adapter) as Box<dyn BoltLoadAdapter + Send>)
        .save_dir(temp_dir.path().to_path_buf())
        .cancel_token(cancel_token)
        .runtime(runtime);

    // Test meta retrieval before building
    let meta = builder.retrieve_meta().await.unwrap();
    assert_eq!(meta.content_size, 5000);
    assert_eq!(meta.filename, Some("custom_file.txt".to_string()));

    // Build should succeed and use the retrieved filename
    let task = builder.build().await.unwrap();
    assert!(!task.is_finished());

    info!("✓ TaskBuilder meta retrieval test passed");
}

// ============================================================================
// Task Lifecycle Tests
// ============================================================================

#[tokio::test]
async fn test_task_basic_lifecycle() {
    let runtime = create_test_runtime();
    let adapter = SimpleTestAdapter::new(2048).with_range_support(false);
    let temp_dir = TempDir::new().unwrap();
    let save_path = temp_dir.path().join("lifecycle_test.bin");
    let cancel_token = CancellationToken::new();

    let mut task = TaskBuilder::default()
        .adapter(Box::new(adapter) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(save_path.clone())
        .cancel_token(cancel_token)
        .runtime(runtime)
        .build()
        .await
        .unwrap();

    // Initial state
    assert!(!task.is_initialized());
    assert!(!task.is_finished());

    // Run the task
    let run_result = task.run().await;
    assert!(run_result.is_ok(), "Task run should succeed");

    // Give the async task time to process events and update state
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Task should now be initialized
    assert!(task.is_initialized());

    // Wait for completion
    let wait_result = task.wait().await;
    info!("Wait result: {:?}", wait_result);
    assert!(wait_result.is_ok(), "Task should complete successfully");

    // Task should be finished
    assert!(task.is_finished());

    // Debug file path and directory contents
    info!("Expected save path: {:?}", save_path);
    info!("Path exists: {}", save_path.exists());

    // Check for partial file
    let mut partial_path = save_path.clone();
    if let Some(filename) = partial_path.file_name() {
        let mut file_name = filename.to_os_string();
        file_name.push(".partial");
        partial_path.set_file_name(file_name);
    }
    info!("Partial path: {:?}", partial_path);
    info!("Partial exists: {}", partial_path.exists());

    if let Some(parent) = save_path.parent() {
        info!("Parent directory: {:?}", parent);
        if parent.exists() {
            match std::fs::read_dir(parent) {
                Ok(entries) => {
                    info!("Directory contents:");
                    for entry in entries {
                        if let Ok(entry) = entry {
                            info!("  - {:?}", entry.path());
                        }
                    }
                }
                Err(e) => info!("Failed to read directory: {:?}", e),
            }
        } else {
            info!("Parent directory doesn't exist");
        }
    }

    // Verify file exists and has correct content
    assert!(save_path.exists(), "Downloaded file should exist");
    let downloaded_content = std::fs::read(&save_path).unwrap();
    assert_eq!(
        downloaded_content.len(),
        2048,
        "Downloaded file should have correct size"
    );

    info!("✓ Task basic lifecycle test passed");
}

#[tokio::test]
async fn test_task_cancellation() {
    let runtime = create_test_runtime();
    let adapter = SimpleTestAdapter::new(10000).with_range_support(false); // 10KB file
    let temp_dir = TempDir::new().unwrap();
    let save_path = temp_dir.path().join("cancellation_test.bin");
    let cancel_token = CancellationToken::new();

    let mut task = TaskBuilder::default()
        .adapter(Box::new(adapter) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(save_path.clone())
        .cancel_token(cancel_token.clone())
        .runtime(runtime)
        .build()
        .await
        .unwrap();

    // Start the task
    let run_result = task.run().await;
    assert!(run_result.is_ok());

    // Cancel after a short delay
    tokio::time::sleep(Duration::from_millis(10)).await;
    task.stop().await;

    // Wait for task to respond to cancellation
    let wait_result = task.wait().await;

    // Task should have been cancelled or completed
    assert!(task.is_finished());

    info!(
        "✓ Task cancellation test passed (result: {:?})",
        wait_result
    );
}

// ============================================================================
// Actual Download Tests with Hash Verification
// ============================================================================

#[tokio::test]
async fn test_small_file_download_with_hash_verification() {
    let runtime = create_test_runtime();
    let adapter = SimpleTestAdapter::new(10240).with_range_support(false); // 10KB
    let expected_hash = adapter.expected_hash().to_string();
    let temp_dir = TempDir::new().unwrap();
    let save_path = temp_dir.path().join("small_download.bin");
    let cancel_token = CancellationToken::new();

    let mut task = TaskBuilder::default()
        .adapter(Box::new(adapter) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(save_path.clone())
        .cancel_token(cancel_token)
        .runtime(runtime)
        .build()
        .await
        .unwrap();

    let start_time = Instant::now();

    // Run the download
    task.run().await.unwrap();
    let result = task.wait().await;

    let duration = start_time.elapsed();

    assert!(result.is_ok(), "Download should complete successfully");
    assert!(task.is_finished(), "Task should be finished");

    // Verify file exists and calculate hash
    assert!(save_path.exists(), "Downloaded file should exist");
    let downloaded_content = std::fs::read(&save_path).unwrap();
    let actual_hash = calculate_sha256(&downloaded_content);

    assert_eq!(
        actual_hash, expected_hash,
        "Downloaded file hash should match expected hash"
    );
    assert_eq!(
        downloaded_content.len(),
        10240,
        "Downloaded file should have correct size"
    );

    info!("✓ Small file download test passed");
    info!("  - Size: 10KB");
    info!("  - Duration: {:?}", duration);
    info!("  - Hash: {}", actual_hash);
}

#[tokio::test]
async fn test_medium_file_download_with_hash_verification() {
    let runtime = create_test_runtime();
    let adapter = SimpleTestAdapter::new(1024 * 1024).with_range_support(false); // 1MB
    let expected_hash = adapter.expected_hash().to_string();
    let temp_dir = TempDir::new().unwrap();
    let save_path = temp_dir.path().join("medium_download.bin");
    let cancel_token = CancellationToken::new();

    let mut task = TaskBuilder::default()
        .adapter(Box::new(adapter) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(save_path.clone())
        .cancel_token(cancel_token)
        .runtime(runtime)
        .build()
        .await
        .unwrap();

    let start_time = Instant::now();

    // Run the download
    task.run().await.unwrap();
    let result = task.wait().await;

    let duration = start_time.elapsed();

    assert!(result.is_ok(), "Download should complete successfully");

    // Verify file and hash
    let downloaded_content = std::fs::read(&save_path).unwrap();
    let actual_hash = calculate_sha256(&downloaded_content);

    assert_eq!(actual_hash, expected_hash, "Hash verification failed");
    assert_eq!(
        downloaded_content.len(),
        1024 * 1024,
        "File size verification failed"
    );

    info!("✓ Medium file download test passed");
    info!("  - Size: 1MB");
    info!("  - Duration: {:?}", duration);
    info!("  - Hash: {}", actual_hash);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_large_file_download_with_hash_verification() {
    let runtime = create_test_runtime();
    let adapter = SimpleTestAdapter::new_large().with_range_support(true); // 100MB
    let expected_hash = adapter.expected_hash().to_string();
    let temp_dir = TempDir::new().unwrap();
    let save_path = temp_dir.path().join("large_download.bin");
    let cancel_token = CancellationToken::new();

    let mut task = TaskBuilder::default()
        .adapter(Box::new(adapter) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(save_path.clone())
        .cancel_token(cancel_token)
        .runtime(runtime)
        .build()
        .await
        .unwrap();

    info!("🚀 Starting 100MB file download test...");
    let start_time = Instant::now();

    // Run the download
    task.run().await.unwrap();
    let result = task.wait().await;

    let duration = start_time.elapsed();

    assert!(
        result.is_ok(),
        "Large file download should complete successfully"
    );

    // Verify file exists and has correct size
    assert!(save_path.exists(), "Downloaded file should exist");
    let file_size = std::fs::metadata(&save_path).unwrap().len();
    assert_eq!(
        file_size,
        100 * 1024 * 1024,
        "File size should be exactly 100MB"
    );

    // Calculate and verify hash
    info!("🔍 Calculating hash for verification...");
    let hash_start = Instant::now();
    let downloaded_content = std::fs::read(&save_path).unwrap();
    let actual_hash = calculate_sha256(&downloaded_content);
    let hash_duration = hash_start.elapsed();

    assert_eq!(
        actual_hash, expected_hash,
        "Hash verification failed for large file"
    );

    // Calculate download speed
    let speed_mbps = (100.0 / duration.as_secs_f64()).round();

    info!("🎉 Large file download test passed!");
    info!("  - Size: 100MB");
    info!("  - Download time: {:?}", duration);
    info!("  - Hash calculation time: {:?}", hash_duration);
    info!("  - Average speed: {} MB/s", speed_mbps);
    info!("  - Hash: {}", actual_hash);

    // Clean up large file
    let _ = std::fs::remove_file(&save_path);
}

// ============================================================================
// Error Handling Tests
// ============================================================================

#[tokio::test]
async fn test_adapter_failure_handling() {
    let runtime = create_test_runtime();
    let adapter = SimpleTestAdapter::new(1024).with_failure(true);
    let temp_dir = TempDir::new().unwrap();
    let save_path = temp_dir.path().join("failure_test.bin");
    let cancel_token = CancellationToken::new();

    // Try to build task - should fail during meta retrieval
    let build_result = TaskBuilder::default()
        .adapter(Box::new(adapter) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(save_path)
        .cancel_token(cancel_token)
        .runtime(runtime)
        .build()
        .await;

    // Task should fail to build due to adapter failure
    assert!(
        build_result.is_err(),
        "Task should fail to build when adapter fails"
    );

    info!("✓ Adapter failure handling test passed");
    return;

    // This code is unreachable due to the return above
    unreachable!();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
// FIXME: support zero-size file download —— although it's wired, but it's a valid use case
async fn test_zero_size_file_handling() {
    let runtime = create_test_runtime();
    let adapter = SimpleTestAdapter::new(0); // Zero-size file
    let temp_dir = TempDir::new().unwrap();
    let save_path = temp_dir.path().join("zero_size_test.bin");
    let cancel_token = CancellationToken::new();

    let mut task = TaskBuilder::default()
        .adapter(Box::new(adapter) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(save_path.clone())
        .cancel_token(cancel_token)
        .runtime(runtime)
        .build()
        .await
        .unwrap();

    task.run().await.unwrap();
    let result = task.wait().await;

    info!("state: {:?}", task.task_state.load(Ordering::Acquire));
    result.expect("should be ok");
    // Zero-size downloads should complete successfully
    assert!(save_path.exists(), "Zero-size file should exist");
    let content = std::fs::read(&save_path).unwrap();
    assert!(content.is_empty(), "Zero-size file should be empty");
    info!("✓ Zero-size file handling test passed");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_concurrent_vs_singleton_performance() {
    let file_size = 1024 * 1024 * 1024; // 1GB for reasonable test time
    let temp_dir = TempDir::new().unwrap();

    // Test concurrent mode
    let runtime1 = create_test_runtime();
    let adapter1 = SimpleTestAdapter::new(file_size).with_range_support(true);
    let concurrent_expected_hash = adapter1.expected_hash().to_string();
    let concurrent_path = temp_dir.path().join("concurrent.bin");

    let mut concurrent_task = TaskBuilder::default()
        .adapter(Box::new(adapter1) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(concurrent_path.clone())
        .prefer_mode(DownloadMode::Concurrent)
        .cancel_token(CancellationToken::new())
        .runtime(runtime1)
        .build()
        .await
        .unwrap();

    // Test singleton mode
    let runtime2 = create_test_runtime();
    let adapter2 = SimpleTestAdapter::new(file_size).with_range_support(false);
    let singleton_expected_hash = adapter2.expected_hash().to_string();
    let singleton_path = temp_dir.path().join("singleton.bin");

    let mut singleton_task = TaskBuilder::default()
        .adapter(Box::new(adapter2) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(singleton_path.clone())
        .prefer_mode(DownloadMode::Singleton)
        .cancel_token(CancellationToken::new())
        .runtime(runtime2)
        .build()
        .await
        .unwrap();

    info!("🏁 Starting performance comparison test (1MB)...");

    // Run both downloads and measure time
    let start_time = Instant::now();

    concurrent_task.run().await.unwrap();
    singleton_task.run().await.unwrap();

    let ((concurrent_result, concurrent_duration), (singleton_result, singleton_duration)) = tokio::join!(
        async {
            let result = concurrent_task.wait().await;
            error!("Concurrent task finished");
            (result, start_time.elapsed())
        },
        async {
            let result = singleton_task.wait().await;
            error!("Singleton task finished");
            (result, start_time.elapsed())
        }
    );

    let total_duration = start_time.elapsed();

    let concurrent_speed = (file_size as f64 / concurrent_duration.as_secs_f64()).round();
    let singleton_speed = (file_size as f64 / singleton_duration.as_secs_f64()).round();

    // At least singleton should succeed
    assert!(
        singleton_result.is_ok(),
        "Singleton download should succeed"
    );

    // Verify singleton file
    let singleton_content = std::fs::read(&singleton_path).unwrap();
    let singleton_hash = calculate_sha256(&singleton_content);
    let concurrent_content = std::fs::read(&concurrent_path).unwrap();
    let concurrent_hash = calculate_sha256(&concurrent_content);
    assert_eq!(
        singleton_hash, singleton_expected_hash,
        "Singleton file hash should be correct"
    );
    assert_eq!(
        concurrent_hash, concurrent_expected_hash,
        "Concurrent file hash should be correct"
    );

    info!("✓ Performance comparison test completed");
    info!(
        "
    - Concurrent result: {concurrent_result:?}
        - Measured Speed: {concurrent_speed} MB/s
        - Duration: {concurrent_duration:?}
        - Hash: {concurrent_hash}
        - File size: {file_size} bytes"
    );
    info!(
        "
    - Singleton result: {singleton_result:?}
        - Measured Speed: {singleton_speed} MB/s
        - Duration: {singleton_duration:?}
        - Hash: {singleton_hash}
        - File size: {file_size} bytes"
    );
    info!("  - Total test duration: {total_duration:?}");

    // If concurrent also succeeded, verify its file
    if concurrent_result.is_ok() && concurrent_path.exists() {
        let concurrent_content = std::fs::read(&concurrent_path).unwrap();
        let concurrent_hash = calculate_sha256(&concurrent_content);
        assert_eq!(
            concurrent_hash, concurrent_expected_hash,
            "Concurrent file hash should be correct"
        );
        info!("  - Both modes completed successfully with matching hashes");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_concurrent_task_with_range_stream() {
    let runtime = create_test_runtime();
    let adapter = SimpleTestAdapter::new(1024 * 1024).with_range_support(true);
    let temp_dir = TempDir::new().unwrap();
    let save_path = temp_dir.path().join("concurrent_range_test.bin");
    let cancel_token = CancellationToken::new();

    let mut task = TaskBuilder::default()
        .adapter(Box::new(adapter) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(save_path.clone())
        .cancel_token(cancel_token)
        .runtime(runtime)
        .build()
        .await
        .unwrap();

    task.run().await.unwrap();
    let result = task.wait().await;

    result.expect("should be ok");
    assert!(save_path.exists(), "Downloaded file should exist");
    let content = std::fs::read(&save_path).unwrap();
    assert_eq!(
        content.len(),
        1024 * 1024,
        "Downloaded file should have correct size"
    );
    info!("✓ Concurrent task with range stream test passed");
}
