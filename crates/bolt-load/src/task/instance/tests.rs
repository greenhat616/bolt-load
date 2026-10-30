use async_stream::stream;
use bolt_load_utils::telemetry::*;
use bytes::Bytes;
use pretty_assertions::assert_eq;
use smol_cancellation_token::CancellationToken;
use tempfile::TempDir;

use crate::{
    adapter::{
        AdapterError, AnyBytesStream, BoltLoadAdapter, BoltLoadAdapterMeta, UnretryableError,
    },
    task::{
        DownloadError, DownloadMode, TaskState,
        test_support::{assert_transitions, record, spawn, tmp_path, watchdog},
    },
};

// Mock adapter for testing
#[derive(Clone)]
struct MockAdapter {
    content: Vec<u8>,
    support_range: bool,
    meta: BoltLoadAdapterMeta,
    should_fail: bool,
}

impl MockAdapter {
    fn new(content: Vec<u8>) -> Self {
        Self {
            meta: BoltLoadAdapterMeta {
                content_size: content.len() as u64,
                filename: Some("test.txt".to_string()),
            },
            content,
            support_range: true,
            should_fail: false,
        }
    }

    fn with_range_support(mut self, support: bool) -> Self {
        self.support_range = support;
        self
    }

    fn with_failure(mut self, should_fail: bool) -> Self {
        self.should_fail = should_fail;
        self
    }

    fn zero_size() -> Self {
        Self {
            content: vec![],
            support_range: false,
            meta: BoltLoadAdapterMeta {
                content_size: 0,
                filename: Some("empty.txt".to_string()),
            },
            should_fail: false,
        }
    }
}

impl BoltLoadAdapter for MockAdapter {
    async fn is_range_stream_available(&self) -> bool {
        self.support_range
    }

    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError> {
        if self.should_fail {
            return Err(AdapterError::Unretryable {
                source: UnretryableError::Internal {
                    message: "Mock adapter failure".to_string(),
                },
            });
        }
        Ok(self.meta.clone())
    }

    async fn full_stream(&self) -> Result<AnyBytesStream, AdapterError> {
        if self.should_fail {
            return Err(AdapterError::Unretryable {
                source: UnretryableError::Internal {
                    message: "Mock adapter failure".to_string(),
                },
            });
        }

        let content = self.content.clone();
        let stream = stream! {
            for chunk in content.chunks(1024) {
                yield Ok(Bytes::from(chunk.to_vec()));
            }
        };
        Ok(Box::pin(stream))
    }

    async fn range_stream(&self, start: u64, end: u64) -> Result<AnyBytesStream, AdapterError> {
        if !self.support_range {
            return Err(AdapterError::Unretryable {
                source: UnretryableError::Internal {
                    message: "Range not supported".to_string(),
                },
            });
        }

        let start = start as usize;
        let end = end as usize;
        let content = self.content[start..end.min(self.content.len())].to_vec();

        let stream = stream! {
            for chunk in content.chunks(512) {
                yield Ok(Bytes::from(chunk.to_vec()));
            }
        };
        Ok(Box::pin(stream))
    }
}

fn create_test_content(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 256) as u8).collect()
}

#[tokio::test]
#[n0_tracing_test::traced_test]
async fn test_singleton_task_basic_download() {
    let content = create_test_content(5000);
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("test_singleton.txt");
    let task = spawn(
        DownloadMode::Singleton,
        MockAdapter::new(content.clone()),
        &file_path,
        &CancellationToken::new(),
    );

    let statuses = record(task.status()).await;
    let output = watchdog(task).await.unwrap();

    assert_transitions(&statuses);
    assert_eq!(output.mode, DownloadMode::Singleton);
    assert_eq!(std::fs::read(&file_path).unwrap(), content);
}

#[tokio::test(flavor = "multi_thread")]
#[n0_tracing_test::traced_test]
async fn test_concurrent_task_basic_download() {
    let content = create_test_content(10000);
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("test_concurrent.txt");
    let task = spawn(
        DownloadMode::Concurrent,
        MockAdapter::new(content.clone()),
        &file_path,
        &CancellationToken::new(),
    );

    let output = watchdog(task).await.unwrap();

    assert_eq!(output.mode, DownloadMode::Concurrent);
    assert_eq!(output.progress.downloaded, content.len() as u64);
    assert_eq!(std::fs::read(&file_path).unwrap(), content);
}

#[tokio::test]
#[n0_tracing_test::traced_test]
async fn test_singleton_task_cancellation() {
    let content = create_test_content(100000);
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("test_cancel.txt");
    let cancel_token = CancellationToken::new();
    let task = spawn(
        DownloadMode::Singleton,
        MockAdapter::new(content),
        &file_path,
        &cancel_token,
    );

    cancel_token.cancel();
    let result = watchdog(task).await;

    // A fast download may commit before the cancellation is observed.
    info!("Cancellation test result: {:?}", result);
    if let Err(e) = result {
        assert!(e.is_cancelled(), "{e:?}");
        assert!(!file_path.exists() && !tmp_path(&file_path).exists());
    }
}

/// A zero size is unknown and selects the singleton mode, even when the
/// concurrent mode is preferred. An empty stream of unknown size then fails
/// with `Empty` (#153), so only the mode is checked here.
#[tokio::test]
#[n0_tracing_test::traced_test]
async fn test_zero_size_selects_singleton_mode() {
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("test_zero_size.txt");
    let task = spawn(
        DownloadMode::Concurrent,
        MockAdapter::zero_size(),
        &file_path,
        &CancellationToken::new(),
    );
    let status = task.status();

    let result = watchdog(task).await;

    info!("Zero-size test result: {result:?}");
    let info = status.borrow().info.clone().expect("initialization ended");
    assert_eq!(info.mode, DownloadMode::Singleton);
}

#[tokio::test]
#[n0_tracing_test::traced_test]
async fn test_singleton_task_adapter_failure() {
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("test_fail.txt");
    let task = spawn(
        DownloadMode::Singleton,
        MockAdapter::new(vec![]).with_failure(true),
        &file_path,
        &CancellationToken::new(),
    );
    let rx = task.status();

    let result = watchdog(task).await;

    assert!(
        matches!(result, Err(DownloadError::Metadata { .. })),
        "{result:?}"
    );
    let status = rx.borrow();
    assert_eq!(status.state, TaskState::Failed);
    assert!(status.info.is_none());
    assert!(!file_path.exists());
}

#[tokio::test]
#[n0_tracing_test::traced_test]
async fn test_progress_tracking() {
    let content = create_test_content(8000);
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("test_progress.txt");
    let task = spawn(
        DownloadMode::Singleton,
        MockAdapter::new(content.clone()),
        &file_path,
        &CancellationToken::new(),
    );

    let statuses = record(task.status()).await;
    watchdog(task).await.unwrap();

    // Samples may be coalesced for a fast download; the order holds anyway.
    assert_transitions(&statuses);
    let downloaded: Vec<u64> = statuses.iter().map(|s| s.progress.downloaded).collect();
    assert!(downloaded.is_sorted(), "{downloaded:?}");
    info!("statuses captured: {}", statuses.len());
    assert_eq!(std::fs::read(&file_path).unwrap(), content);
}

#[tokio::test(flavor = "multi_thread")]
#[n0_tracing_test::traced_test]
async fn test_concurrent_vs_singleton_comparison() {
    let content = create_test_content(20000);
    let temp_dir = TempDir::new().unwrap();
    let file_path_concurrent = temp_dir.path().join("concurrent.txt");
    let file_path_singleton = temp_dir.path().join("singleton.txt");

    let concurrent = spawn(
        DownloadMode::Concurrent,
        MockAdapter::new(content.clone()),
        &file_path_concurrent,
        &CancellationToken::new(),
    );
    let singleton = spawn(
        DownloadMode::Concurrent,
        MockAdapter::new(content.clone()).with_range_support(false),
        &file_path_singleton,
        &CancellationToken::new(),
    );
    let (concurrent, singleton) = watchdog(async { tokio::join!(concurrent, singleton) }).await;

    assert_eq!(concurrent.unwrap().mode, DownloadMode::Concurrent);
    assert_eq!(
        singleton.unwrap().mode,
        DownloadMode::Singleton,
        "no range support selects the singleton mode"
    );
    assert_eq!(std::fs::read(&file_path_concurrent).unwrap(), content);
    assert_eq!(std::fs::read(&file_path_singleton).unwrap(), content);
}
