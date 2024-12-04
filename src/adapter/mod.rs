use async_trait::async_trait;
use futures::Stream;

#[cfg(feature = "reqwest")]
mod reqwest;

#[async_trait]
pub trait BoltLoadAdapter: Send + Sync {
    type Item: Send + Sync;
    type Stream: Stream<Item = Self::Item> + Unpin + Send;

    /// Check if the adapter supports range stream
    /// For compatibility, error should be returned as false.
    async fn is_range_stream_available(&self) -> bool {
        false
    }
    /// Get the content size of the adapter
    async fn get_content_size(&self) -> std::io::Result<u64>;

    /// Get a full content stream from the adapter
    async fn full_stream(&self) -> std::io::Result<Self::Stream>;

    /// Get a range content stream from the adapter
    /// Note: the range is followed as [start, end)
    #[allow(unused_variables)]
    async fn range_stream(&self, start: u64, end: u64) -> std::io::Result<Self::Stream> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Range stream is not supported",
        ))
    }
}

pub type AnyStream<T> = Box<dyn Stream<Item = T> + Unpin + Send>;

// TODO: maybe the chunk should be zero copy
// pub trait BoltLoaderAdapterAnyStream =
//     BoltLoadAdapter<Box<dyn Stream<Item = Vec<u8>> + Send>, Vec<u8>>;
