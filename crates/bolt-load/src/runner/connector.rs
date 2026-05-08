use bolt_load_core::adapter::{AdapterError, AnyBytesStream};
use futures::{FutureExt, future::BoxFuture};
use snafu::ResultExt;

pub enum StreamConnector {
    DirectStream(AnyBytesStream),
    StreamConnector(BoxFuture<'static, Result<AnyBytesStream, AdapterError>>),
}

impl StreamConnector {
    pub async fn connect(self) -> Result<AnyBytesStream, ConnectionError> {
        match self {
            Self::DirectStream(stream) => Ok(stream),
            Self::StreamConnector(future) => future.await.context(ConnectionSnafu),
        }
    }
}

#[derive(Debug, snafu::Snafu)]
#[snafu(display("failed to connect stream: {source}"))]
pub struct ConnectionError {
    pub source: AdapterError,
}

impl ConnectionError {
    pub const fn is_retryable(&self) -> bool {
        matches!(self.source, AdapterError::Retryable { .. })
    }
}

impl<F> From<F> for StreamConnector
where
    F: Future<Output = Result<AnyBytesStream, AdapterError>> + Send + 'static,
{
    fn from(value: F) -> Self {
        StreamConnector::StreamConnector(value.boxed())
    }
}
