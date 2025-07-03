use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use bolt_load::{
    adapter::{
        AnyBytesStream, BoltLoadAdapter, BoltLoadAdapterMeta, StreamError, UnretryableError,
    },
    runtime::ThreadedRuntimeImpl,
    task::{DownloadMode, TaskBuilder},
};
use bytes::Bytes;
use sha2::{Digest, Sha256};
use smol_cancellation_token::CancellationToken;
use tempfile::TempDir;
use tracing::{level_filters::LevelFilter, *};
use tracing_subscriber::{
    EnvFilter, Layer, fmt::format::FmtSpan, layer::SubscriberExt, util::SubscriberInitExt,
};

/// Calculate SHA256 hash of the content
#[cfg_attr(feature = "tracing", tracing::instrument(skip(content), ret))]
pub fn calculate_sha256(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}

/// Creates a deterministic test content with specified size for hash verification
#[cfg_attr(feature = "tracing", tracing::instrument)]
pub fn create_deterministic_content(size: usize) -> Vec<u8> {
    let mut content = Vec::with_capacity(size);
    let mut counter = 0u64;

    while content.len() < size {
        let bytes = counter.to_le_bytes();
        for &byte in &bytes {
            if content.len() < size {
                content.push(byte);
            }
        }
        counter += 1;
    }

    content
}

/// Simple test adapter for testing Task and TaskBuilder
#[derive(Clone)]
pub struct SimpleTestAdapter {
    content: Vec<u8>,
    expected_hash: String,
    support_range: bool,
    should_fail: bool,
    chunk_size: usize,
    call_count: Arc<AtomicUsize>,
    filename: Option<String>,
}

impl SimpleTestAdapter {
    /// Create a new test adapter with deterministic content
    #[cfg_attr(feature = "tracing", tracing::instrument)]
    pub fn new(size: usize) -> Self {
        let content = create_deterministic_content(size);
        let expected_hash = calculate_sha256(&content);

        Self {
            content,
            expected_hash,
            support_range: true,
            should_fail: false,
            chunk_size: 8192, // 8KB chunks by default
            call_count: Arc::new(AtomicUsize::new(0)),
            filename: Some("test_file.bin".to_string()),
        }
    }

    /// Create large test content (100MB)
    pub fn new_large() -> Self {
        Self::new(100 * 1024 * 1024) // 100MB
    }

    /// Configure range support
    pub fn with_range_support(mut self, support: bool) -> Self {
        self.support_range = support;
        self
    }

    /// Configure failure simulation
    pub fn with_failure(mut self, should_fail: bool) -> Self {
        self.should_fail = should_fail;
        self
    }

    /// Configure filename
    pub fn with_filename(mut self, filename: String) -> Self {
        self.filename = Some(filename);
        self
    }

    /// Configure chunk size
    pub fn with_chunk_size(mut self, chunk_size: usize) -> Self {
        self.chunk_size = chunk_size;
        self
    }

    /// Get expected hash for verification
    pub fn expected_hash(&self) -> &str {
        &self.expected_hash
    }

    /// Get call count
    pub fn call_count(&self) -> usize {
        self.call_count.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl BoltLoadAdapter for SimpleTestAdapter {
    async fn is_range_stream_available(&self) -> bool {
        info!(
            "[TEST ADAPTER] is_range_stream_available() called, returning: {}",
            self.support_range
        );
        self.call_count.fetch_add(1, Ordering::Relaxed);
        self.support_range
    }

    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, UnretryableError> {
        info!("[TEST ADAPTER] retrieve_meta() called");
        self.call_count.fetch_add(1, Ordering::Relaxed);

        if self.should_fail {
            info!("[TEST ADAPTER] retrieve_meta() returning failure");
            return Err(UnretryableError::Internal(
                "Simulated meta retrieval failure".to_string(),
            ));
        }

        info!(
            "[TEST ADAPTER] retrieve_meta() success, size: {}",
            self.content.len()
        );

        Ok(BoltLoadAdapterMeta {
            content_size: self.content.len() as u64,
            filename: self.filename.clone(),
        })
    }

    async fn full_stream(&self) -> Result<AnyBytesStream, StreamError> {
        info!(
            "[TEST ADAPTER] full_stream() called, content size: {}",
            self.content.len()
        );
        self.call_count.fetch_add(1, Ordering::Relaxed);

        if self.should_fail {
            info!("[TEST ADAPTER] full_stream() returning failure");
            return Err(StreamError::Unretryable(UnretryableError::Internal(
                "Simulated stream failure".to_string(),
            )));
        }

        let content = self.content.clone();
        let chunk_size = self.chunk_size;
        info!(
            "[TEST ADAPTER] full_stream() creating stream with chunk_size: {}",
            chunk_size
        );
        let stream = async_stream::stream! {
            for chunk in content.chunks(chunk_size) {
                yield Ok(Bytes::from(chunk.to_vec()));
            }
        };
        Ok(Box::pin(stream))
    }

    async fn range_stream(&self, start: u64, end: u64) -> Result<AnyBytesStream, StreamError> {
        self.call_count.fetch_add(1, Ordering::Relaxed);

        if self.should_fail {
            return Err(StreamError::Unretryable(UnretryableError::Internal(
                "Simulated range stream failure".to_string(),
            )));
        }

        if !self.support_range {
            return Err(StreamError::Unretryable(UnretryableError::Internal(
                "Range requests not supported".to_string(),
            )));
        }

        let start = start as usize;
        let end = end as usize;
        let content = self.content[start..end.min(self.content.len())].to_vec();
        let chunk_size = self.chunk_size;

        let stream = async_stream::stream! {
            for chunk in content.chunks(chunk_size) {
                yield Ok(Bytes::from(chunk.to_vec()));
            }
        };
        Ok(Box::pin(stream))
    }
}

pub fn init_tracing() {
    let current_crate = env!("CARGO_CRATE_NAME");
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_level(true)
        // .with_span_events(FmtSpan::ENTER | FmtSpan::CLOSE)
        .with_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::WARN.into())
                .parse(format!("bolt_load=trace,{current_crate}=trace"))
                .unwrap(),
        );
    let filter_layer = EnvFilter::builder()
        .with_default_directive(LevelFilter::TRACE.into())
        .from_env_lossy();

    let subscriber = tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer);

    let tokio_console_layer = console_subscriber::spawn();
    let _ = subscriber
        .with(tracing_tracy::TracyLayer::default())
        .with(tokio_console_layer)
        .try_init();
}

#[cfg_attr(feature = "tracing", tracing::instrument)]
async fn test_concurrent_vs_singleton_performance() {
    let file_size = 1024 * 1024 * 1024; // 1GB for reasonable test time
    let temp_dir = TempDir::new().unwrap();

    // Test concurrent mode
    let runtime = ThreadedRuntimeImpl::new_tokio_rt();
    let adapter1 = SimpleTestAdapter::new(file_size).with_range_support(true);
    let concurrent_expected_hash = adapter1.expected_hash().to_string();
    let concurrent_path = temp_dir.path().join("concurrent.bin");

    let mut concurrent_task = TaskBuilder::default()
        .adapter(Box::new(adapter1) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(concurrent_path.clone())
        .prefer_mode(DownloadMode::Concurrent)
        .cancel_token(CancellationToken::new())
        .runtime(runtime.clone())
        .build()
        .await
        .unwrap();

    // Test singleton mode
    let adapter2 = SimpleTestAdapter::new(file_size).with_range_support(false);
    let singleton_expected_hash = adapter2.expected_hash().to_string();
    let singleton_path = temp_dir.path().join("singleton.bin");

    let mut singleton_task = TaskBuilder::default()
        .adapter(Box::new(adapter2) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(singleton_path.clone())
        .prefer_mode(DownloadMode::Singleton)
        .cancel_token(CancellationToken::new())
        .runtime(runtime.clone())
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

fn main() {
    let time = Instant::now();
    init_tracing();
    trace!("tracing initialized in {:?}", time.elapsed());

    let time = Instant::now();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let elapsed = time.elapsed();
        info!("Runtime initialized in {:?}", elapsed);
        test_concurrent_vs_singleton_performance().await;
    });
    let elapsed = time.elapsed();
    info!("Total time: {:?}", elapsed);
}
