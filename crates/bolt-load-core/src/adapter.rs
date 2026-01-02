use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;

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
