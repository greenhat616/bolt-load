use async_trait::async_trait;
use futures::stream::BoxStream;
pub mod error;
pub use error::{AdapterError, RetryableError, UnretryableError};
use error::{UnretryableSnafu, unretryable::RangeStreamNotSupportedSnafu};
use snafu::ResultExt;

#[async_trait]
pub trait BoltLoadAdapter: Send + Sync {
    /// Check if the adapter supports range stream
    /// For compatibility, error should be returned as false.
    async fn is_range_stream_available(&self) -> bool {
        false
    }

    /// Perform a meta request to the adapter
    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError>;

    /// Get a full content stream from the adapter
    async fn full_stream(&self) -> Result<AnyBytesStream, AdapterError>;

    /// Get a range content stream from the adapter
    /// Note: the range is followed as [start, end)
    #[allow(unused_variables)]
    async fn range_stream(&self, start: u64, end: u64) -> Result<AnyBytesStream, AdapterError> {
        RangeStreamNotSupportedSnafu {}
            .fail()
            .context(UnretryableSnafu {})
    }
}

#[derive(Debug, Clone)]
pub struct BoltLoadAdapterMeta {
    /// the content size
    pub content_size: u64,
    /// suggested filename
    pub filename: Option<String>,
}

pub type AnyStream<'a, T> = BoxStream<'a, T>;
pub type AnyBytesStream = AnyStream<'static, Result<bytes::Bytes, AdapterError>>;
pub type AnyAdapter = Box<dyn BoltLoadAdapter + Send>;

// TODO: maybe the chunk should be zero copy
// pub trait BoltLoaderAdapterAnyStream =
//     BoltLoadAdapter<Box<dyn Stream<Item = Vec<u8>> + Send>, Vec<u8>>;
