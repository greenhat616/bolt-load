use async_trait::async_trait;
use futures::Stream;

#[async_trait]
pub trait BoltLoadAdapter<S, T>:
    BoltLoadAdapterFullStream<S, T> + BoltLoadAdapterRangeStream<S, T>
where
    S: Stream<Item = T>,
{
    /// Check if the adapter supports range stream
    /// For compatibility, error should be returned as false.
    async fn is_range_stream_available(&self) -> bool {
        false
    }
    /// Get the content size of the adapter
    async fn get_content_size(&self) -> std::io::Result<u64>;
}

#[async_trait]
pub trait BoltLoadAdapterFullStream<S, T>
where
    S: Stream<Item = T>,
{
    /// Get a full content stream from the adapter
    async fn full_stream(&self) -> std::io::Result<S>;
}

#[async_trait]
pub trait BoltLoadAdapterRangeStream<S, T>
where
    S: Stream<Item = T>,
{
    /// Get a range content stream from the adapter
    async fn range_stream(&self, start: u64, end: u64) -> std::io::Result<S> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Range stream is not supported",
        ))
    }
}

// TODO: maybe the chunk should be zero copy
// pub trait BoltLoaderAdapterAnyStream =
//     BoltLoadAdapter<Box<dyn Stream<Item = Vec<u8>> + Send>, Vec<u8>>;
