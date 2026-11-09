use std::fmt;

use futures::stream::BoxStream;
mod content_range;
pub mod error;
mod validator;
pub use content_range::{ContentRange, InvalidContentRange};
pub use error::{AdapterError, RetryableError, UnretryableError};
pub use validator::{RangeExpectation, Validator};
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

    /// Get the full content from the adapter.
    ///
    /// The task decodes the body as [`FullResponse::content_encoding`]
    /// says, so an HTTP adapter sets it and
    /// [`FullResponse::content_type`] from the response.
    fn full_stream(&self) -> impl Future<Output = Result<FullResponse, AdapterError>> + Send;

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

    /// Get a range content stream, as [`Self::range_stream`] does, of the
    /// representation `expected` describes (design D14).
    ///
    /// The task calls it for every range, with the same `expected` for the
    /// whole download, built from the task's own metadata. An HTTP adapter
    /// sends `expected.validator` in `If-Range` and checks the response
    /// against it and against `expected.total`, and fails with
    /// [`UnretryableError::ResourceChanged`] when the response holds another
    /// representation. It must not take the validator or the total from
    /// what it cached instead: another task sharing the adapter may have
    /// refreshed it since.
    ///
    /// The default ignores `expected` and calls [`Self::range_stream`], so
    /// an adapter that does not implement it does not notice a change.
    // Not `expect`: dynosaur copies the attribute into code that uses the arguments.
    #[allow(unused_variables)]
    fn range_stream_if(
        &self,
        start: u64,
        end: u64,
        expected: &RangeExpectation,
    ) -> impl Future<Output = Result<AnyBytesStream, AdapterError>> + Send {
        self.range_stream(start, end)
    }

    /// Forget what the adapter cached about the resource.
    ///
    /// The task calls it before it downloads a changed resource again from
    /// scratch (design D12, D13), then retrieves the metadata and checks
    /// range support again. An adapter that caches must drop all of it:
    /// the metadata, whether ranges work, and the validators, so that the
    /// next [`Self::retrieve_meta`] and [`Self::is_range_stream_available`]
    /// ask the server again.
    ///
    /// - A call made before the refresh may still complete with what it
    ///   retrieved, but a call made after it must not be served a result
    ///   retrieved before it.
    /// - Range requests do not depend on the cache: each carries the
    ///   [`RangeExpectation`] of the task that sends it, so a refresh by one
    ///   task never changes the ranges of another task sharing the adapter.
    ///
    /// The default caches nothing, so it does nothing.
    fn refresh(&self) {}
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
    /// The strong `ETag` of the resource, as the server sent it, quotes
    /// included; a weak one is not kept.
    pub etag: Option<String>,
    /// The `Last-Modified` of the resource, as the server sent it.
    pub last_modified: Option<String>,
    /// The validator a range request sends in `If-Range`, if the resource
    /// has a usable one: its strong `ETag`, or else a `Last-Modified` at
    /// least one second older than `Date`. A resource with only a weak
    /// `ETag` has none (RFC 9110 §13.1.5). The task builds its
    /// [`RangeExpectation`] from it.
    pub validator: Option<Validator>,
}

impl BoltLoadAdapterMeta {
    /// Metadata for `content_size` bytes, with no suggested filename and no
    /// content coding.
    pub fn new(content_size: u64) -> Self {
        Self {
            content_size,
            filename: None,
            content_encoding: None,
            etag: None,
            last_modified: None,
            validator: None,
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

    /// Set the strong `ETag`. A weak one, `W/"…"`, is dropped: it cannot
    /// make a range request conditional.
    pub fn with_etag(mut self, etag: Option<String>) -> Self {
        self.etag = etag.filter(|value| !value.trim_start().starts_with("W/"));
        self
    }

    /// Set the `Last-Modified`.
    pub fn with_last_modified(mut self, last_modified: Option<String>) -> Self {
        self.last_modified = last_modified;
        self
    }

    /// Set the validator for `If-Range`.
    pub fn with_validator(mut self, validator: Option<Validator>) -> Self {
        self.validator = validator;
        self
    }

    /// What a range request of this metadata expects: its size and its
    /// validator.
    pub fn range_expectation(&self) -> RangeExpectation {
        RangeExpectation::new(self.content_size).with_validator(self.validator.clone())
    }
}

/// The response to [`BoltLoadAdapter::full_stream`]: the body, and the
/// headers the task needs to turn it into the file.
#[non_exhaustive]
pub struct FullResponse {
    /// The body, as the HTTP client hands it out.
    pub body: AnyBytesStream,
    /// The combined `Content-Encoding` of the body as handed out: `None`
    /// when the response has none, or when the HTTP client decoded the body
    /// and dropped the header. The task decodes what it lists.
    pub content_encoding: Option<String>,
    /// The combined `Content-Type`. A body whose type is the format of its
    /// only coding, such as `application/gzip` with `Content-Encoding:
    /// gzip`, is the file as it is.
    pub content_type: Option<String>,
}

impl FullResponse {
    /// A response whose body is the file, with no coding and no type.
    pub fn new(body: AnyBytesStream) -> Self {
        Self {
            body,
            content_encoding: None,
            content_type: None,
        }
    }

    /// Set the combined `Content-Encoding`.
    pub fn with_content_encoding(mut self, content_encoding: Option<String>) -> Self {
        self.content_encoding = content_encoding;
        self
    }

    /// Set the combined `Content-Type`.
    pub fn with_content_type(mut self, content_type: Option<String>) -> Self {
        self.content_type = content_type;
        self
    }
}

impl fmt::Debug for FullResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FullResponse")
            .field("content_encoding", &self.content_encoding)
            .field("content_type", &self.content_type)
            .finish_non_exhaustive()
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

        async fn full_stream(&self) -> Result<FullResponse, AdapterError> {
            Ok(FullResponse::new(Box::pin(futures::stream::empty())))
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
            // Without an override, the expectation is ignored.
            let expected = RangeExpectation::new(1)
                .with_validator(Some(Validator::StrongEtag("\"v1\"".into())));
            let err = adapter.range_stream_if(0, 1, &expected).await.err().unwrap();
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
