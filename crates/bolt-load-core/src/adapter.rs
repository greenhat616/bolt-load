use futures::stream::BoxStream;
pub mod error;
pub use error::{AdapterError, RetryableError, UnretryableError};
use error::{UnretryableSnafu, unretryable::RangeStreamNotSupportedSnafu};
use snafu::ResultExt;

/// A download source.
///
/// Implement it with `async fn`s; every returned future must be `Send`.
/// [`DynBoltLoadAdapter`] erases the implementing type, see [`AnyAdapter`].
#[dynosaur::dynosaur(pub DynBoltLoadAdapter = dyn(box) BoltLoadAdapter, bridge(dyn))]
pub trait BoltLoadAdapter: Send + Sync {
    /// Check if the adapter supports range stream
    /// For compatibility, error should be returned as false.
    fn is_range_stream_available(&self) -> impl Future<Output = bool> + Send {
        async { false }
    }

    /// Perform a meta request to the adapter
    fn retrieve_meta(
        &self,
    ) -> impl Future<Output = Result<BoltLoadAdapterMeta, AdapterError>> + Send;

    /// Get a full content stream from the adapter
    fn full_stream(&self) -> impl Future<Output = Result<AnyBytesStream, AdapterError>> + Send;

    /// Get a range content stream from the adapter
    /// Note: the range is followed as [start, end)
    // Not `expect`: dynosaur copies the attribute into code that uses the arguments.
    #[allow(unused_variables)]
    fn range_stream(
        &self,
        start: u64,
        end: u64,
    ) -> impl Future<Output = Result<AnyBytesStream, AdapterError>> + Send {
        async {
            RangeStreamNotSupportedSnafu {}
                .fail()
                .context(UnretryableSnafu {})
        }
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
/// A type-erased adapter, usually shared as `Arc<AnyAdapter>`.
///
/// Build one with [`DynBoltLoadAdapter::new_arc`] or
/// [`DynBoltLoadAdapter::new_box`].
pub type AnyAdapter = DynBoltLoadAdapter<'static>;

// TODO: maybe the chunk should be zero copy
// pub trait BoltLoaderAdapterAnyStream =
//     BoltLoadAdapter<Box<dyn Stream<Item = Vec<u8>> + Send>, Vec<u8>>;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    struct MetaOnly;

    impl BoltLoadAdapter for MetaOnly {
        async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError> {
            Ok(BoltLoadAdapterMeta {
                content_size: 1,
                filename: None,
            })
        }

        async fn full_stream(&self) -> Result<AnyBytesStream, AdapterError> {
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    fn assert_send_sync<T: ?Sized + Send + Sync>() {}
    fn assert_send_static<T: Send + 'static>(_: &T) {}

    #[test]
    fn any_adapter_and_its_futures_are_send() {
        assert_send_sync::<AnyAdapter>();
        let adapter: Arc<AnyAdapter> = DynBoltLoadAdapter::new_arc(MetaOnly);
        assert_send_static(&async move { adapter.range_stream(0, 1).await });
    }

    #[test]
    fn default_methods_dispatch_through_the_erased_type() {
        let adapter: Arc<AnyAdapter> = DynBoltLoadAdapter::new_arc(MetaOnly);
        futures::executor::block_on(async {
            assert!(!adapter.is_range_stream_available().await);
            let err = adapter.range_stream(0, 1).await.err().unwrap();
            assert!(
                matches!(
                    err,
                    AdapterError::Unretryable {
                        source: UnretryableError::RangeStreamNotSupported
                    }
                ),
                "{err:?}"
            );
        });
    }
}
