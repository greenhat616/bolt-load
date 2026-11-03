pub use retryable::RetryableError;
use snafu::prelude::*;
pub use unretryable::UnretryableError;

/// The error type for the adapter stream
#[derive(Debug, Snafu, Clone)]
#[snafu(visibility(pub))]
pub enum AdapterError {
    /// The error is retryable
    #[snafu(display("retryable error: {source}"))]
    Retryable { source: RetryableError },

    /// The error is unretryable
    #[snafu(display("unretryable error: {source}"))]
    Unretryable { source: UnretryableError },
}

impl From<RetryableError> for AdapterError {
    fn from(e: RetryableError) -> Self {
        Self::Retryable { source: e }
    }
}

impl From<UnretryableError> for AdapterError {
    fn from(e: UnretryableError) -> Self {
        Self::Unretryable { source: e }
    }
}

pub mod unretryable {
    use std::{ops::Range, sync::Arc, time::Duration};

    use snafu::prelude::*;

    use super::{super::ContentRange, retryable::RetryableError};

    #[derive(Debug, Snafu, Clone)]
    #[snafu(visibility(pub))]
    #[non_exhaustive]
    pub enum UnretryableError {
        #[snafu(display("access denied ({status}): {message}"))]
        Unauthorized { status: u16, message: String },
        #[snafu(display("resource not found ({status})"))]
        NotFound { status: u16 },
        /// The server rejects the load (429/503). `retry_after` is the
        /// server's `Retry-After`, when it sent a usable one.
        #[snafu(display("server is congested ({status})"))]
        Congestion {
            status: u16,
            retry_after: Option<Duration>,
        },
        /// A status with no more specific variant.
        #[snafu(display("unexpected http status {status}"))]
        Status { status: u16 },
        /// The server answered a range request with the whole resource.
        #[snafu(display("server ignored the range request"))]
        RangeIgnored,
        /// The response does not prove it carries the requested bytes.
        #[snafu(display(
            "range response does not match bytes {}-{}: {}",
            requested.start,
            requested.end.saturating_sub(1),
            got.map_or_else(|| "no usable Content-Range".to_owned(), |got| got.to_string())
        ))]
        RangeMismatch {
            requested: Range<u64>,
            got: Option<ContentRange>,
        },
        /// The body of a range response runs past its `Content-Range`. A
        /// client that decodes a content-coded body and drops
        /// `Content-Encoding` does this.
        #[snafu(display("range response body runs past {got}; the client may have decoded it"))]
        RangeBodyTooLong { got: ContentRange },
        /// The body of a range response ends cleanly before its
        /// `Content-Range` is covered. A client that decodes a body that
        /// does not compress does this; so does a server that miscounts.
        #[snafu(display(
            "range response body ends after {received} bytes of {got}; the client may have \
             decoded it"
        ))]
        RangeBodyTooShort { got: ContentRange, received: u64 },
        /// 416. `total` comes from `Content-Range: bytes */total`.
        #[snafu(display("range not satisfiable (total {total:?})"))]
        RangeNotSatisfiable { total: Option<u64> },
        #[snafu(display("internal error: {message}"))]
        /// The error is internal. such as a http request, we do not retrieve the meta, and we call the range stream directly
        Internal { message: String },
        #[snafu(display("task cancelled"))]
        Cancelled,
        #[snafu(display("io error: {source}"))]
        Io { source: Arc<std::io::Error> },
        #[snafu(display("range stream is not supported"))]
        RangeStreamNotSupported,

        #[snafu(whatever, display("{message}"))]
        Whatever {
            message: String,
            #[snafu(source(from(Arc<dyn std::error::Error + Send + Sync>, Some)))]
            source: Option<Arc<dyn std::error::Error + Send + Sync>>,
        },
    }

    impl UnretryableError {
        pub fn from_retryable_error(e: RetryableError) -> Self {
            match e {
                RetryableError::Io { source: e } => Self::Io { source: e },
                RetryableError::Status { status } => Self::Status { status },
            }
        }
    }

    impl From<std::io::Error> for UnretryableError {
        fn from(e: std::io::Error) -> Self {
            Self::Io {
                source: Arc::new(e),
            }
        }
    }
}

pub mod retryable {
    use std::sync::Arc;

    use snafu::prelude::*;

    #[derive(Debug, Snafu, Clone)]
    #[snafu(visibility(pub))]
    #[non_exhaustive]
    pub enum RetryableError {
        #[snafu(display("io error: {source}"))]
        Io { source: Arc<std::io::Error> },
        /// A status worth retrying, such as 408 or a 5xx.
        #[snafu(display("http status {status}"))]
        Status { status: u16 },
    }
}
