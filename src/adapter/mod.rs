use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;

#[cfg(feature = "reqwest")]
pub mod reqwest;

#[cfg(feature = "ureq2")]
pub mod ureq2;

#[async_trait]
pub trait BoltLoadAdapter: Send + Sync {
    /// Check if the adapter supports range stream
    /// For compatibility, error should be returned as false.
    async fn is_range_stream_available(&self) -> bool {
        false
    }

    /// Perform a meta request to the adapter
    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, UnretryableError>;

    /// Get a full content stream from the adapter
    async fn full_stream(&self) -> Result<AnyBytesStream, StreamError>;

    /// Get a range content stream from the adapter
    /// Note: the range is followed as [start, end)
    #[allow(unused_variables)]
    async fn range_stream(&self, start: u64, end: u64) -> Result<AnyBytesStream, StreamError> {
        Err(
            UnretryableError::new_io_error(std::io::Error::other("Range stream is not supported"))
                .into(),
        )
    }
}

#[derive(Debug, Clone)]
pub struct BoltLoadAdapterMeta {
    /// the content size
    pub content_size: u64,
    /// suggested filename
    pub filename: Option<String>,
}

#[derive(Debug, thiserror::Error, Clone)]
pub enum RetryableError {
    #[error(transparent)]
    Io(#[from] Arc<std::io::Error>),
}

impl RetryableError {
    pub fn new_io_error(e: std::io::Error) -> Self {
        Self::Io(Arc::new(e))
    }
}

impl From<std::io::Error> for RetryableError {
    fn from(e: std::io::Error) -> Self {
        RetryableError::Io(Arc::new(e))
    }
}

#[derive(Debug, thiserror::Error, Clone)]
pub enum UnretryableError {
    #[error("access denied: {0}")]
    Unauthorized(String),
    #[error("resource not found")]
    NotFound,
    #[error("internal error: {0}")]
    /// The error is internal. such as a http request, we do not retrieve the meta, and we call the range stream directly
    Internal(String),

    #[error("exceeded request limits, reason: {0}")]
    ExceededRequestLimits(String),
    #[error("task cancelled")]
    Cancelled,
    #[error(transparent)]
    Io(#[from] Arc<std::io::Error>),
}

impl UnretryableError {
    pub fn new_io_error(e: std::io::Error) -> Self {
        Self::Io(Arc::new(e))
    }

    pub fn new_exceeded_request_limits(s: impl AsRef<str>) -> Self {
        Self::ExceededRequestLimits(s.as_ref().to_string())
    }

    pub fn from_retryable_error(e: RetryableError) -> Self {
        match e {
            RetryableError::Io(e) => Self::Io(e),
        }
    }
}

impl From<std::io::Error> for UnretryableError {
    fn from(e: std::io::Error) -> Self {
        UnretryableError::Io(Arc::new(e))
    }
}

#[derive(Debug, thiserror::Error, Clone)]
/// The error type for the adapter stream
pub enum StreamError {
    /// The error is retryable
    #[error(transparent)]
    Retryable(#[from] RetryableError),

    /// The error is unretryable
    #[error(transparent)]
    Unretryable(#[from] UnretryableError),
}

impl From<StreamError> for UnretryableError {
    fn from(e: StreamError) -> Self {
        match e {
            StreamError::Retryable(e) => match e {
                RetryableError::Io(e) => UnretryableError::Io(e),
            },
            StreamError::Unretryable(e) => e,
        }
    }
}

pub type AnyStream<'a, T> = BoxStream<'a, T>;
pub type AnyBytesStream = AnyStream<'static, Result<bytes::Bytes, StreamError>>;
pub type AnyAdapter = Box<dyn BoltLoadAdapter + Send>;

// TODO: maybe the chunk should be zero copy
// pub trait BoltLoaderAdapterAnyStream =
//     BoltLoadAdapter<Box<dyn Stream<Item = Vec<u8>> + Send>, Vec<u8>>;

#[cfg(test)]
pub mod tests {
    use super::*;

    use axum::response::IntoResponse;
    use bytes::Bytes;
    use rand::Rng;
    use sha2::{Digest, Sha256};
    use std::{
        io::{BufWriter, Seek, Write},
        sync::Arc,
        time::Duration,
    };
    use tempfile::tempfile;
    use tokio::{io::AsyncSeekExt, net::TcpListener};

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Creates a deterministic test content with specified size for hash verification
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

    /// Calculate SHA256 hash of the content
    pub fn calculate_sha256(content: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content);
        format!("{:x}", hasher.finalize())
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
            println!(
                "[TEST ADAPTER] is_range_stream_available() called, returning: {}",
                self.support_range
            );
            self.call_count.fetch_add(1, Ordering::Relaxed);
            self.support_range
        }

        async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, UnretryableError> {
            println!("[TEST ADAPTER] retrieve_meta() called");
            self.call_count.fetch_add(1, Ordering::Relaxed);

            if self.should_fail {
                println!("[TEST ADAPTER] retrieve_meta() returning failure");
                return Err(UnretryableError::Internal(
                    "Simulated meta retrieval failure".to_string(),
                ));
            }

            println!(
                "[TEST ADAPTER] retrieve_meta() success, size: {}",
                self.content.len()
            );

            Ok(BoltLoadAdapterMeta {
                content_size: self.content.len() as u64,
                filename: self.filename.clone(),
            })
        }

        async fn full_stream(&self) -> Result<AnyBytesStream, StreamError> {
            println!(
                "[TEST ADAPTER] full_stream() called, content size: {}",
                self.content.len()
            );
            self.call_count.fetch_add(1, Ordering::Relaxed);

            if self.should_fail {
                println!("[TEST ADAPTER] full_stream() returning failure");
                return Err(StreamError::Unretryable(UnretryableError::Internal(
                    "Simulated stream failure".to_string(),
                )));
            }

            let content = self.content.clone();
            let chunk_size = self.chunk_size;
            println!(
                "[TEST ADAPTER] full_stream() creating stream with chunk_size: {}",
                chunk_size
            );
            let stream = async_stream::stream! {
                for chunk in content.chunks(chunk_size) {
                    tokio::time::sleep(Duration::from_micros(10)).await;
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
                    tokio::time::sleep(Duration::from_micros(10)).await;
                    yield Ok(Bytes::from(chunk.to_vec()));
                }
            };
            Ok(Box::pin(stream))
        }
    }

    pub fn create_random_file(size: usize) -> anyhow::Result<std::fs::File> {
        let mut file = tempfile()?;
        let mut writer = BufWriter::new(file.try_clone()?);
        let mut rng = rand::rng();
        let mut buffer = [0; 1024];
        let mut remaining_size = size;
        while remaining_size > 0 {
            let bytes_to_write = std::cmp::min(remaining_size, buffer.len());
            rng.fill(&mut buffer[..bytes_to_write]);
            writer.write_all(&buffer[..bytes_to_write])?;
            remaining_size -= bytes_to_write;
        }
        // reset the file pointer to the beginning
        file.seek(std::io::SeekFrom::Start(0))?;
        Ok(file)
    }

    #[derive(Clone)]
    struct FileHolder(Arc<tokio::sync::Mutex<tokio::fs::File>>);

    pub async fn create_http_server() -> anyhow::Result<(u16, tokio::task::JoinHandle<()>)> {
        use axum::extract::State;
        use axum_extra::TypedHeader;

        let file = tokio::task::spawn_blocking(|| create_random_file(1024 * 1024)).await??;
        let holder = FileHolder(Arc::new(tokio::sync::Mutex::new(
            tokio::fs::File::from_std(file),
        )));
        let port = portpicker::pick_unused_port()
            .ok_or(anyhow::anyhow!("Failed to pick an unused port"))?;
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;

        /// a handler send without range
        async fn no_range_handler(
            State(holder): State<FileHolder>,
        ) -> impl axum::response::IntoResponse {
            let mut file = holder.0.lock().await;
            match file.seek(std::io::SeekFrom::Start(0)).await {
                Ok(_) => {
                    let file_size = file.metadata().await.unwrap().len();
                    let reader = tokio_util::io::ReaderStream::new(file.try_clone().await.unwrap());
                    let body = axum::body::Body::from_stream(reader);
                    let headers = [
                        (
                            axum::http::header::CONTENT_TYPE,
                            "text/plain; charset=utf-8",
                        ),
                        (axum::http::header::CONTENT_LENGTH, &format!("{file_size}")),
                        (
                            axum::http::header::CONTENT_DISPOSITION,
                            "attachment; filename=\"test.txt\"",
                        ),
                    ];
                    (headers, body).into_response()
                }
                Err(e) => (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    e.to_string().into_response(),
                )
                    .into_response(),
            }
        }

        /// a handler mock range stream
        async fn range_handler(
            State(holder): State<FileHolder>,
            range: Option<TypedHeader<axum_extra::headers::Range>>,
        ) -> impl axum::response::IntoResponse {
            let file = holder.0.lock().await;
            let mut file_cloned = file.try_clone().await.unwrap();
            file_cloned.seek(std::io::SeekFrom::Start(0)).await.unwrap();
            let body = axum_range::KnownSize::file(file_cloned).await.unwrap();
            let range = range.map(|TypedHeader(range)| range);
            let ranged = axum_range::Ranged::new(range, body);
            ranged.into_response()
        }

        let app = axum::Router::new()
            .route("/no_range", axum::routing::get(no_range_handler))
            .route("/range", axum::routing::get(range_handler))
            .with_state(holder);

        let handle = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap()
        });

        Ok((port, handle))
    }

    mod simple_test_adapter_tests {
        use super::*;
        use futures::StreamExt;
        use pretty_assertions::assert_eq;
        use test_log::test;

        #[test(tokio::test)]
        async fn test_simple_test_adapter_basic_functionality() {
            let size = 1024;
            let adapter = SimpleTestAdapter::new(size);

            // Test basic properties
            assert_eq!(adapter.content.len(), size);
            assert_eq!(adapter.call_count(), 0);

            // Test expected hash is calculated correctly
            let expected_hash = calculate_sha256(&adapter.content);
            assert_eq!(adapter.expected_hash(), &expected_hash);

            // Test that content is deterministic
            let adapter2 = SimpleTestAdapter::new(size);
            assert_eq!(adapter.content, adapter2.content);
            assert_eq!(adapter.expected_hash(), adapter2.expected_hash());
        }

        #[test(tokio::test)]
        async fn test_retrieve_meta_success() {
            let size = 2048;
            let adapter = SimpleTestAdapter::new(size).with_filename("custom_test.bin".to_string());

            let meta = adapter.retrieve_meta().await.unwrap();

            assert_eq!(meta.content_size, size as u64);
            assert_eq!(meta.filename, Some("custom_test.bin".to_string()));
            assert_eq!(adapter.call_count(), 1);
        }

        #[test(tokio::test)]
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

        #[test(tokio::test)]
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

        #[test(tokio::test)]
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
            assert_eq!(downloaded_data, adapter.content);

            // Verify hash
            let actual_hash = calculate_sha256(&downloaded_data);
            assert_eq!(actual_hash, expected_hash);
        }

        #[test(tokio::test)]
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

        #[test(tokio::test)]
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
            let expected_range_hash = calculate_sha256(expected_range_data);
            let actual_hash = calculate_sha256(&downloaded_data);
            assert_eq!(actual_hash, expected_range_hash);
        }

        #[test(tokio::test)]
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

        #[test(tokio::test)]
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

        #[test(tokio::test)]
        async fn test_large_adapter() {
            let adapter = SimpleTestAdapter::new_large();
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
            let expected_hash = calculate_sha256(expected_data);
            let actual_hash = calculate_sha256(&downloaded_data);
            assert_eq!(actual_hash, expected_hash);
        }

        #[test(tokio::test)]
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

        #[test(tokio::test)]
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

        #[test(tokio::test)]
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
            let hash1 = calculate_sha256(&content1);
            let hash2 = calculate_sha256(&content2);
            assert_eq!(hash1, hash2);
        }

        #[test(tokio::test)]
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

        #[test(tokio::test)]
        async fn test_concurrent_access() {
            let adapter = SimpleTestAdapter::new(1024);
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
    }
}
