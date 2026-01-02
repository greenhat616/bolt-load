use std::{
    num::NonZeroU32,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use bolt_load_core::adapter::*;
use bolt_load_utils::telemetry::*;
use bytes::Bytes;
use governor::{Quota, RateLimiter};

/// Creates a deterministic test content with specified size for hash verification
pub fn create_deterministic_content(size: usize) -> Vec<u8> {
    let mut content = Vec::with_capacity(size);
    let mut counter = 0u64;

    // SAFETY: we are sure the content is not shared and the size is correct
    // It's useful for testing (debug mode) to have a fastest content generation
    unsafe {
        let ptr: *mut u8 = content.as_mut_ptr();
        let mut offset = 0;

        while offset + 8 <= size {
            std::ptr::write_unaligned(ptr.add(offset) as *mut u64, counter.to_le());
            offset += 8;
            counter += 1;
        }

        if offset < size {
            let bytes = counter.to_le_bytes();
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.add(offset), size - offset);
        }

        content.set_len(size);
    }

    content
}

/// Calculate blake3 hash of the content
pub fn calculate_blake3(content: &[u8]) -> String {
    let hash = blake3::hash(content);
    hash.to_hex().to_string()
}

/// Simple test adapter for testing Task and TaskBuilder
#[derive(Clone, Debug)]
pub struct SimpleTestAdapterBuilder {
    content_size: Option<usize>,
    support_range: Option<bool>,
    should_fail: Option<bool>,
    chunk_size: Option<usize>,
    filename: Option<String>,
    max_speed: Option<u64>,
    max_per_stream_speed: Option<u64>,
}

pub type GlobalRateLimiter = Arc<
    RateLimiter<
        governor::state::direct::NotKeyed,
        governor::state::InMemoryState,
        governor::clock::DefaultClock,
    >,
>;

impl Default for SimpleTestAdapterBuilder {
    fn default() -> Self {
        Self {
            content_size: None,
            support_range: None,
            should_fail: None,
            chunk_size: None,
            filename: Some("test_file.bin".to_string()),
            max_speed: None,
            max_per_stream_speed: None,
        }
    }
}

/// Build error type for SimpleTestAdapterBuilder
#[derive(Debug, Clone, thiserror::Error)]
#[error("content size is not set")]
pub struct SimpleTestAdapterBuildError;

impl SimpleTestAdapterBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn large() -> Self {
        Self {
            content_size: Some(100 * 1024 * 1024), // 100MB
            ..Self::default()
        }
    }

    pub fn large_with_range_support() -> Self {
        Self {
            content_size: Some(100 * 1024 * 1024), // 100MB
            support_range: Some(true),
            ..Self::default()
        }
    }

    pub fn content_size(mut self, content_size: usize) -> Self {
        self.content_size = Some(content_size);
        self
    }

    pub fn support_range(mut self, support_range: bool) -> Self {
        self.support_range = Some(support_range);
        self
    }

    pub fn should_fail(mut self, should_fail: bool) -> Self {
        self.should_fail = Some(should_fail);
        self
    }

    pub fn chunk_size(mut self, chunk_size: usize) -> Self {
        self.chunk_size = Some(chunk_size);
        self
    }

    pub fn filename(mut self, filename: String) -> Self {
        self.filename = Some(filename);
        self
    }

    pub fn without_filename(mut self) -> Self {
        self.filename = None;
        self
    }

    pub fn max_speed(mut self, max_speed: u64) -> Self {
        self.max_speed = Some(max_speed);
        self
    }

    pub fn max_per_stream_speed(mut self, max_per_stream_speed: u64) -> Self {
        self.max_per_stream_speed = Some(max_per_stream_speed);
        self
    }

    #[cfg_attr(feature = "tracing", tracing::instrument)]
    pub fn build(self) -> Result<SimpleTestAdapter, SimpleTestAdapterBuildError> {
        let Some(content_size) = self.content_size else {
            return Err(SimpleTestAdapterBuildError);
        };
        let content = create_deterministic_content(content_size);
        let expected_hash = calculate_blake3(&content);
        let chunk_size = self.chunk_size.unwrap_or(8192);
        let rate_limiter = self.max_speed.map(|speed| {
            Arc::new(RateLimiter::direct(
                Quota::per_second(
                    NonZeroU32::new(speed.try_into().expect("speed must be lossless to u32"))
                        .expect("speed must be non-zero"),
                )
                .allow_burst(
                    NonZeroU32::new(
                        chunk_size
                            .try_into()
                            .expect("chunk size must be lossless to u32"),
                    )
                    .expect("chunk size must be non-zero"),
                ),
            ))
        });

        Ok(SimpleTestAdapter {
            content: Bytes::from(content),
            expected_hash,
            support_range: self.support_range.unwrap_or(false),
            should_fail: self.should_fail.unwrap_or(false),
            chunk_size,
            call_count: Arc::new(AtomicUsize::new(0)),
            filename: self.filename,
            rate_limiter,
            max_per_stream_speed: self.max_per_stream_speed,
        })
    }
}

/// Simple test adapter for testing Task and TaskBuilder
#[derive(Clone, derive_more::Debug)]
pub struct SimpleTestAdapter {
    #[debug(ignore)]
    content: Bytes,
    expected_hash: String,
    support_range: bool,
    should_fail: bool,
    chunk_size: usize,
    call_count: Arc<AtomicUsize>,
    filename: Option<String>,
    #[debug(ignore)]
    rate_limiter: Option<GlobalRateLimiter>,
    /// the speed of the stream in bytes per second
    max_per_stream_speed: Option<u64>,
}

impl SimpleTestAdapter {
    /// Create a new test adapter with deterministic content
    pub fn new(size: usize) -> Self {
        SimpleTestAdapterBuilder::new()
            .content_size(size)
            .build()
            .expect("Failed to build SimpleTestAdapter")
    }

    /// Create large test content (100MB)
    pub fn new_large() -> Self {
        SimpleTestAdapterBuilder::large().build().unwrap()
    }

    /// Clone and reset call count - useful for creating multiple adapters with same content
    pub fn clone_reset_count(&self) -> Self {
        Self {
            content: self.content.clone(),
            expected_hash: self.expected_hash.clone(),
            support_range: self.support_range,
            should_fail: self.should_fail,
            chunk_size: self.chunk_size,
            call_count: Arc::new(AtomicUsize::new(0)),
            filename: self.filename.clone(),
            rate_limiter: self.rate_limiter.clone(),
            max_per_stream_speed: self.max_per_stream_speed,
        }
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

    /// Get content reference
    pub fn content(&self) -> &Bytes {
        &self.content
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
        let chunk_size = NonZeroU32::new(self.chunk_size as u32).unwrap();
        let rate_limiter = self.rate_limiter.clone();
        let stream_rate_limiter = self.max_per_stream_speed.map(|speed| {
            let quota = Quota::per_second(
                NonZeroU32::new(speed.try_into().expect("speed must be lossless to u32")).unwrap(),
            )
            .allow_burst(chunk_size);
            Arc::new(RateLimiter::direct(quota))
        });

        info!(
            "[TEST ADAPTER] full_stream() creating stream with chunk_size: {}",
            chunk_size
        );
        let stream = async_stream::stream! {
            for chunk in content.chunks(chunk_size.get() as usize) {
                let chunk_len = NonZeroU32::new(chunk.len() as u32).unwrap();
                if let Some(rate_limiter) = rate_limiter.clone() {
                    rate_limiter.until_n_ready(chunk_len).await.unwrap();
                }
                if let Some(stream_rate_limiter) = stream_rate_limiter.clone() {
                    stream_rate_limiter.until_n_ready(chunk_len).await.unwrap();
                }
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
        let content = self.content.slice(start..end.min(self.content.len()));
        let chunk_size = NonZeroU32::new(self.chunk_size as u32).unwrap();
        let rate_limiter = self.rate_limiter.clone();
        let stream_rate_limiter = self.max_per_stream_speed.map(|speed| {
            let quota = Quota::per_second(
                NonZeroU32::new(speed.try_into().expect("speed must be lossless to u32")).unwrap(),
            )
            .allow_burst(chunk_size);
            Arc::new(RateLimiter::direct(quota))
        });

        let stream = async_stream::stream! {
            for chunk in content.chunks(chunk_size.get() as usize) {
                let chunk_len = NonZeroU32::new(chunk.len() as u32).unwrap();
                if let Some(rate_limiter) = rate_limiter.clone() {
                    rate_limiter.until_n_ready(chunk_len).await.unwrap();
                }
                if let Some(stream_rate_limiter) = stream_rate_limiter.clone() {
                    stream_rate_limiter.until_n_ready(chunk_len).await.unwrap();
                }
                yield Ok(Bytes::from(chunk.to_vec()));
            }
        };

        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_simple_test_adapter_basic_functionality() {
        let size = 1024;
        let adapter = SimpleTestAdapter::new(size);

        // Test basic properties
        assert_eq!(adapter.content.len(), size);
        assert_eq!(adapter.call_count(), 0);

        // Test expected hash is calculated correctly
        let expected_hash = calculate_blake3(&adapter.content);
        assert_eq!(adapter.expected_hash(), &expected_hash);

        // Test that content is deterministic
        let adapter2 = SimpleTestAdapter::new(size);
        assert_eq!(adapter.content, adapter2.content);
        assert_eq!(adapter.expected_hash(), adapter2.expected_hash());
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_retrieve_meta_success() {
        let size = 2048;
        let adapter = SimpleTestAdapter::new(size).with_filename("custom_test.bin".to_string());

        let meta = adapter.retrieve_meta().await.unwrap();

        assert_eq!(meta.content_size, size as u64);
        assert_eq!(meta.filename, Some("custom_test.bin".to_string()));
        assert_eq!(adapter.call_count(), 1);
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_retrieve_meta_failure() {
        let adapter = SimpleTestAdapter::new(1024).with_failure(true);

        let result = adapter.retrieve_meta().await;
        assert!(result.is_err());

        match result.unwrap_err() {
            UnretryableError::Internal(msg) => {
                assert_eq!(msg, "Simulated meta retrieval failure");
            }
            _ => panic!("Expected Internal error"),
        }

        assert_eq!(adapter.call_count(), 1);
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_is_range_stream_available() {
        // Test range support enabled
        let adapter_with_range = SimpleTestAdapter::new(1024).with_range_support(true);
        assert!(adapter_with_range.is_range_stream_available().await);
        assert_eq!(adapter_with_range.call_count(), 1);

        // Test range support disabled
        let adapter_without_range = SimpleTestAdapter::new(1024).with_range_support(false);
        assert!(!adapter_without_range.is_range_stream_available().await);
        assert_eq!(adapter_without_range.call_count(), 1);
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_full_stream_success_with_hash_verification() {
        let size = 4096;
        let adapter = SimpleTestAdapter::new(size);
        let expected_hash = adapter.expected_hash().to_string();

        let stream = adapter.full_stream().await.unwrap();
        assert_eq!(adapter.call_count(), 1);

        // Collect all data from stream
        let mut downloaded_data = Vec::new();
        let mut stream = std::pin::pin!(stream);

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.unwrap();
            downloaded_data.extend_from_slice(&chunk);
        }

        // Verify downloaded data
        assert_eq!(downloaded_data.len(), size);
        assert_eq!(downloaded_data, *adapter.content);

        // Verify hash
        let actual_hash = calculate_blake3(&downloaded_data);
        assert_eq!(actual_hash, expected_hash);
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_full_stream_failure() {
        let adapter = SimpleTestAdapter::new(1024).with_failure(true);

        let result = adapter.full_stream().await;
        assert!(result.is_err());

        match result.err().unwrap() {
            StreamError::Unretryable(UnretryableError::Internal(msg)) => {
                assert_eq!(msg, "Simulated stream failure");
            }
            _ => panic!("Expected Unretryable Internal error"),
        }

        assert_eq!(adapter.call_count(), 1);
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_range_stream_success_with_hash_verification() {
        let size = 8192;
        let adapter = SimpleTestAdapter::new(size).with_range_support(true);

        let start = 1000u64;
        let end = 3000u64;
        let expected_range_data = &adapter.content[start as usize..end as usize];

        let stream = adapter.range_stream(start, end).await.unwrap();
        assert_eq!(adapter.call_count(), 1);

        // Collect all data from stream
        let mut downloaded_data = Vec::new();
        let mut stream = std::pin::pin!(stream);

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.unwrap();
            downloaded_data.extend_from_slice(&chunk);
        }

        // Verify range data
        assert_eq!(downloaded_data.len(), (end - start) as usize);
        assert_eq!(downloaded_data, expected_range_data);

        // Verify hash of range data
        let expected_range_hash = calculate_blake3(expected_range_data);
        let actual_hash = calculate_blake3(&downloaded_data);
        assert_eq!(actual_hash, expected_range_hash);
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_range_stream_not_supported() {
        let adapter = SimpleTestAdapter::new(1024).with_range_support(false);

        let result = adapter.range_stream(0, 500).await;
        assert!(result.is_err());

        match result.err().unwrap() {
            StreamError::Unretryable(UnretryableError::Internal(msg)) => {
                assert_eq!(msg, "Range requests not supported");
            }
            _ => panic!("Expected Unretryable Internal error"),
        }

        assert_eq!(adapter.call_count(), 1);
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_range_stream_edge_cases() {
        let size = 1000;
        let adapter = SimpleTestAdapter::new(size).with_range_support(true);

        // Test range that exceeds content size
        let start = 800u64;
        let end = 1500u64; // Beyond content size
        let expected_data = &adapter.content[start as usize..];

        let stream = adapter.range_stream(start, end).await.unwrap();

        let mut downloaded_data = Vec::new();
        let mut stream = std::pin::pin!(stream);

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.unwrap();
            downloaded_data.extend_from_slice(&chunk);
        }

        assert_eq!(downloaded_data, expected_data);

        // Test zero-length range
        let stream = adapter.range_stream(500, 500).await.unwrap();
        let mut stream = std::pin::pin!(stream);
        let chunk = stream.next().await;
        assert!(chunk.is_none()); // Should be empty

        assert_eq!(adapter.call_count(), 2);
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_large_adapter() {
        let adapter = SimpleTestAdapterBuilder::large_with_range_support()
            .build()
            .unwrap();
        let expected_size = 100 * 1024 * 1024; // 100MB

        assert_eq!(adapter.content.len(), expected_size);

        // Test meta retrieval
        let meta = adapter.retrieve_meta().await.unwrap();
        assert_eq!(meta.content_size, expected_size as u64);

        // Test a small range to avoid excessive memory usage in tests
        let start = 50 * 1024 * 1024u64; // 50MB
        let end = start + 1024; // 1KB range
        let expected_data = &adapter.content[start as usize..end as usize];

        let stream = adapter.range_stream(start, end).await.unwrap();
        let mut downloaded_data = Vec::new();
        let mut stream = std::pin::pin!(stream);

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.unwrap();
            downloaded_data.extend_from_slice(&chunk);
        }

        assert_eq!(downloaded_data, expected_data);
        let expected_hash = calculate_blake3(expected_data);
        let actual_hash = calculate_blake3(&downloaded_data);
        assert_eq!(actual_hash, expected_hash);
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_call_count_tracking() {
        let adapter = SimpleTestAdapter::new(1024);

        assert_eq!(adapter.call_count(), 0);

        // Each method call should increment call count
        let _ = adapter.is_range_stream_available().await;
        assert_eq!(adapter.call_count(), 1);

        let _ = adapter.retrieve_meta().await;
        assert_eq!(adapter.call_count(), 2);

        let _ = adapter.full_stream().await;
        assert_eq!(adapter.call_count(), 3);

        let _ = adapter.range_stream(0, 100).await;
        assert_eq!(adapter.call_count(), 4);
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_configuration_chain() {
        let adapter = SimpleTestAdapter::new(2048)
            .with_range_support(false)
            .with_failure(true)
            .with_filename("chained_config.bin".to_string());

        // Test configured values
        assert!(!adapter.is_range_stream_available().await);
        assert!(adapter.retrieve_meta().await.is_err());

        // Test that range requests fail due to disabled support
        let range_result = adapter.range_stream(0, 100).await;
        assert!(range_result.is_err());
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_deterministic_content_generation() {
        // Test that content generation is truly deterministic
        let content1 = create_deterministic_content(1000);
        let content2 = create_deterministic_content(1000);
        assert_eq!(content1, content2);

        // Test different sizes produce different total content
        let content_small = create_deterministic_content(100);
        let content_large = create_deterministic_content(200);
        assert_ne!(content_small.len(), content_large.len());
        // The first 100 bytes should be the same since it's deterministic
        assert_eq!(content_small, content_large[..100]);

        // Test hash calculation consistency
        let hash1 = calculate_blake3(&content1);
        let hash2 = calculate_blake3(&content2);
        assert_eq!(hash1, hash2);
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_chunk_size_behavior() {
        let adapter = SimpleTestAdapter::new(10000); // 10KB content
        let stream = adapter.full_stream().await.unwrap();

        let mut chunk_count = 0;
        let mut total_size = 0;
        let mut stream = std::pin::pin!(stream);

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.unwrap();
            chunk_count += 1;
            total_size += chunk.len();

            // Each chunk should be at most the configured chunk size (8KB by default)
            assert!(chunk.len() <= 8192);
        }

        assert_eq!(total_size, 10000);
        // With 10KB content and 8KB chunks, we should have 2 chunks (8KB + 2KB)
        assert_eq!(chunk_count, 2);
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_concurrent_access() {
        let adapter = SimpleTestAdapter::new(1024).with_range_support(true);
        let adapter = Arc::new(adapter);

        // Test concurrent access doesn't cause issues
        let handles = (0..5)
            .map(|i| {
                let adapter = adapter.clone();
                tokio::spawn(async move {
                    let start = i * 100;
                    let end = start + 100;
                    let stream = adapter.range_stream(start, end).await.unwrap();

                    let mut data = Vec::new();
                    let mut stream = std::pin::pin!(stream);
                    while let Some(chunk_result) = stream.next().await {
                        let chunk = chunk_result.unwrap();
                        data.extend_from_slice(&chunk);
                    }
                    data
                })
            })
            .collect::<Vec<_>>();

        let results = futures::future::join_all(handles).await;

        // Verify all tasks completed successfully
        for (i, result) in results.into_iter().enumerate() {
            let data = result.unwrap();
            let start = i * 100;
            let end = start + 100;
            let expected = &adapter.content[start..end];
            assert_eq!(data, expected);
        }

        // Call count should reflect all the range_stream calls
        assert_eq!(adapter.call_count(), 5);
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_global_speed_limit() {
        use std::time::Instant;

        // Set speed limit to 50KB/s (50 * 1024 bytes per second)
        let speed_limit = 50 * 1024u64;
        let content_size = 200 * 1024; // 200KB content
        let chunk_size = 8192; // 8KB chunks

        let adapter = SimpleTestAdapterBuilder::new()
            .content_size(content_size)
            .chunk_size(chunk_size)
            .max_speed(speed_limit)
            .build()
            .unwrap();

        let start_time = Instant::now();
        let stream = adapter.full_stream().await.unwrap();

        let mut downloaded_size = 0;
        let mut stream = std::pin::pin!(stream);

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.unwrap();
            downloaded_size += chunk.len();
        }

        let elapsed = start_time.elapsed();
        let elapsed_secs = elapsed.as_secs_f64();

        // Verify all data was downloaded
        assert_eq!(downloaded_size, content_size);

        // Calculate actual speed
        let actual_speed = downloaded_size as f64 / elapsed_secs;

        // Expected minimum time: 200KB / 50KB/s = 4 seconds
        // We allow for some overhead, so actual speed should be less than speed_limit * 1.3
        // and greater than speed_limit * 0.7 (to account for burst and timing variance)
        let min_expected_speed = speed_limit as f64 * 0.7;
        let max_expected_speed = speed_limit as f64 * 1.3;

        println!(
            "Speed limit: {} B/s, Actual speed: {:.2} B/s, Elapsed: {:.2}s",
            speed_limit, actual_speed, elapsed_secs
        );

        assert!(
            actual_speed >= min_expected_speed && actual_speed <= max_expected_speed,
            "Actual speed {:.2} B/s is outside expected range [{:.2}, {:.2}] B/s",
            actual_speed,
            min_expected_speed,
            max_expected_speed
        );
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_per_stream_speed_limit() {
        use std::time::Instant;

        // Set per-stream speed limit to 40KB/s
        let stream_speed_limit = 40 * 1024u64;
        let content_size = 160 * 1024; // 160KB content
        let chunk_size = 8192; // 8KB chunks

        let adapter = SimpleTestAdapterBuilder::new()
            .content_size(content_size)
            .chunk_size(chunk_size)
            .max_per_stream_speed(stream_speed_limit)
            .build()
            .unwrap();

        let start_time = Instant::now();
        let stream = adapter.full_stream().await.unwrap();

        let mut downloaded_size = 0;
        let mut stream = std::pin::pin!(stream);

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.unwrap();
            downloaded_size += chunk.len();
        }

        let elapsed = start_time.elapsed();
        let elapsed_secs = elapsed.as_secs_f64();

        // Verify all data was downloaded
        assert_eq!(downloaded_size, content_size);

        // Calculate actual speed
        let actual_speed = downloaded_size as f64 / elapsed_secs;

        // Expected minimum time: 160KB / 40KB/s = 4 seconds
        let min_expected_speed = stream_speed_limit as f64 * 0.7;
        let max_expected_speed = stream_speed_limit as f64 * 1.3;

        println!(
            "Stream speed limit: {} B/s, Actual speed: {:.2} B/s, Elapsed: {:.2}s",
            stream_speed_limit, actual_speed, elapsed_secs
        );

        assert!(
            actual_speed >= min_expected_speed && actual_speed <= max_expected_speed,
            "Actual speed {:.2} B/s is outside expected range [{:.2}, {:.2}] B/s",
            actual_speed,
            min_expected_speed,
            max_expected_speed
        );
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_range_stream_speed_limit() {
        use std::time::Instant;

        // Test speed limit with range streams
        let speed_limit = 60 * 1024u64;
        let content_size = 300 * 1024; // 300KB content
        let chunk_size = 8192;

        let adapter = SimpleTestAdapterBuilder::new()
            .content_size(content_size)
            .chunk_size(chunk_size)
            .support_range(true)
            .max_speed(speed_limit)
            .build()
            .unwrap();

        // Download a range
        let start = 50 * 1024u64; // Start at 50KB
        let end = 200 * 1024u64; // End at 200KB (150KB total)
        let expected_size = (end - start) as usize;

        let start_time = Instant::now();
        let stream = adapter.range_stream(start, end).await.unwrap();

        let mut downloaded_size = 0;
        let mut stream = std::pin::pin!(stream);

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.unwrap();
            downloaded_size += chunk.len();
        }

        let elapsed = start_time.elapsed();
        let elapsed_secs = elapsed.as_secs_f64();

        // Verify correct range was downloaded
        assert_eq!(downloaded_size, expected_size);

        // Calculate actual speed
        let actual_speed = downloaded_size as f64 / elapsed_secs;

        // Expected minimum time: 150KB / 60KB/s = 2.5 seconds
        let min_expected_speed = speed_limit as f64 * 0.7;
        let max_expected_speed = speed_limit as f64 * 1.3;

        println!(
            "Range stream speed limit: {} B/s, Actual speed: {:.2} B/s, Elapsed: {:.2}s",
            speed_limit, actual_speed, elapsed_secs
        );

        assert!(
            actual_speed >= min_expected_speed && actual_speed <= max_expected_speed,
            "Actual speed {:.2} B/s is outside expected range [{:.2}, {:.2}] B/s",
            actual_speed,
            min_expected_speed,
            max_expected_speed
        );
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_combined_speed_limits() {
        use std::time::Instant;

        // Test both global and per-stream speed limits together
        // The effective limit should be the more restrictive one
        let global_speed_limit = 80 * 1024u64; // 80KB/s
        let stream_speed_limit = 50 * 1024u64; // 50KB/s (more restrictive)
        let content_size = 200 * 1024; // 200KB content
        let chunk_size = 8192;

        let adapter = SimpleTestAdapterBuilder::new()
            .content_size(content_size)
            .chunk_size(chunk_size)
            .max_speed(global_speed_limit)
            .max_per_stream_speed(stream_speed_limit)
            .build()
            .unwrap();

        let start_time = Instant::now();
        let stream = adapter.full_stream().await.unwrap();

        let mut downloaded_size = 0;
        let mut stream = std::pin::pin!(stream);

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.unwrap();
            downloaded_size += chunk.len();
        }

        let elapsed = start_time.elapsed();
        let elapsed_secs = elapsed.as_secs_f64();

        assert_eq!(downloaded_size, content_size);

        let actual_speed = downloaded_size as f64 / elapsed_secs;

        // When both limits are applied, the more restrictive one (stream_speed_limit) should dominate
        // However, both will contribute to the delay, so we expect speed closer to stream_speed_limit
        let min_expected_speed = stream_speed_limit as f64 * 0.5; // More lenient due to combined limits
        let max_expected_speed = stream_speed_limit as f64 * 1.3;

        println!(
            "Global limit: {} B/s, Stream limit: {} B/s, Actual speed: {:.2} B/s, Elapsed: {:.2}s",
            global_speed_limit, stream_speed_limit, actual_speed, elapsed_secs
        );

        assert!(
            actual_speed >= min_expected_speed && actual_speed <= max_expected_speed,
            "Actual speed {:.2} B/s is outside expected range [{:.2}, {:.2}] B/s",
            actual_speed,
            min_expected_speed,
            max_expected_speed
        );
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn test_no_speed_limit() {
        use std::time::Instant;

        // Test that without speed limit, download is fast
        let content_size = 100 * 1024; // 100KB content
        let chunk_size = 8192;

        let adapter = SimpleTestAdapterBuilder::new()
            .content_size(content_size)
            .chunk_size(chunk_size)
            .build()
            .unwrap();

        let start_time = Instant::now();
        let stream = adapter.full_stream().await.unwrap();

        let mut downloaded_size = 0;
        let mut stream = std::pin::pin!(stream);

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.unwrap();
            downloaded_size += chunk.len();
        }

        let elapsed = start_time.elapsed();
        let elapsed_secs = elapsed.as_secs_f64();

        assert_eq!(downloaded_size, content_size);

        // Without speed limit, download should be very fast (typically < 0.1s for 100KB in memory)
        // We just verify it completes much faster than if there was a 50KB/s limit (which would take 2s)
        assert!(
            elapsed_secs < 1.0,
            "Without speed limit, download took {:.2}s which is too slow",
            elapsed_secs
        );

        let actual_speed = downloaded_size as f64 / elapsed_secs;
        println!(
            "No speed limit - Actual speed: {:.2} MB/s, Elapsed: {:.4}s",
            actual_speed / 1024.0 / 1024.0,
            elapsed_secs
        );
    }
}
