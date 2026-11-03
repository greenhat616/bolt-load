use futures::stream::BoxStream;
mod content_range;
pub mod error;
pub use content_range::{ContentRange, InvalidContentRange};
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
#[non_exhaustive]
pub struct BoltLoadAdapterMeta {
    /// the content size
    pub content_size: u64,
    /// suggested filename
    pub filename: Option<String>,
    /// The combined `Content-Encoding` of the metadata responses, when it is
    /// not `identity`. The content size then counts encoded bytes, and the
    /// task downloads over one connection until the stream ends.
    pub content_encoding: Option<String>,
}

impl BoltLoadAdapterMeta {
    /// Metadata for `content_size` bytes, with no suggested filename and no
    /// content coding.
    pub fn new(content_size: u64) -> Self {
        Self {
            content_size,
            filename: None,
            content_encoding: None,
        }
    }

    /// Set the suggested filename.
    pub fn with_filename(mut self, filename: Option<String>) -> Self {
        self.filename = filename;
        self
    }

    /// Set the content coding; an `identity` value is dropped.
    pub fn with_content_encoding(mut self, content_encoding: Option<String>) -> Self {
        self.content_encoding = content_encoding.filter(|value| !is_identity(value));
        self
    }
}

/// Whether a combined `Content-Encoding` value lists only `identity`.
fn is_identity(content_encoding: &str) -> bool {
    content_encoding
        .split(',')
        .map(str::trim)
        .all(|coding| coding.is_empty() || coding.eq_ignore_ascii_case("identity"))
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
            Ok(BoltLoadAdapterMeta::new(1))
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

    #[test]
    fn meta_drops_an_identity_coding() {
        for value in ["", "identity", " Identity ", "identity, identity", ","] {
            let meta = BoltLoadAdapterMeta::new(1).with_content_encoding(Some(value.into()));
            assert_eq!(meta.content_encoding, None, "{value:?}");
        }
        for value in ["gzip", "identity, gzip"] {
            let meta = BoltLoadAdapterMeta::new(1).with_content_encoding(Some(value.into()));
            assert_eq!(meta.content_encoding.as_deref(), Some(value));
        }
    }
}
