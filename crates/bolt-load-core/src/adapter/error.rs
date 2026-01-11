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
    use std::sync::Arc;

    use snafu::prelude::*;

    use super::retryable::RetryableError;

    #[derive(Debug, Snafu, Clone)]
    #[snafu(visibility(pub))]
    pub enum UnretryableError {
        #[snafu(display("access denied: {message}"))]
        Unauthorized { message: String },
        #[snafu(display("resource not found"))]
        NotFound,
        #[snafu(display("service unavailable: {status} {description}"))]
        ServiceUnavailable { status: String, description: String },
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
    pub enum RetryableError {
        #[snafu(display("io error: {source}"))]
        Io { source: Arc<std::io::Error> },
    }
}
