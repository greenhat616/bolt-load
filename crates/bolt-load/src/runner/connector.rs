use bolt_load_core::adapter::{AdapterError, AnyBytesStream};
use futures::{FutureExt, future::BoxFuture};
use snafu::ResultExt;

use super::{RunnerMessageConsumer, TaskRunner, TaskRunnerBuilder, TaskRunnerBuilderError};

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

#[derive(Debug, thiserror::Error)]
pub enum RunnerConnectorError {
    #[error("build failed: {source}")]
    Build { source: TaskRunnerBuilderError },
    #[error("connection failed: {source}")]
    Connection { source: ConnectionError },
}

impl RunnerConnectorError {
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::Connection { source } => source.is_retryable(),
            Self::Build { .. } => false,
        }
    }
}

pub struct RunnerConnector {
    connector: StreamConnector,
    pub builder: TaskRunnerBuilder,
}

impl RunnerConnector {
    pub fn new(connector: impl Into<StreamConnector>, builder: TaskRunnerBuilder) -> Self {
        Self {
            connector: connector.into(),
            builder,
        }
    }

    pub fn from_direct_stream(stream: AnyBytesStream) -> Self {
        Self {
            connector: StreamConnector::DirectStream(stream),
            builder: TaskRunner::builder(),
        }
    }

    pub fn from_stream_connector(
        connector: BoxFuture<'static, Result<AnyBytesStream, AdapterError>>,
    ) -> Self {
        Self {
            connector: StreamConnector::StreamConnector(connector),
            builder: TaskRunner::builder(),
        }
    }

    pub async fn connect(
        self,
    ) -> Result<(TaskRunner, RunnerMessageConsumer), RunnerConnectorError> {
        let RunnerConnector { connector, builder } = self;
        let stream = connector
            .connect()
            .await
            .map_err(|source| RunnerConnectorError::Connection { source })?;

        builder
            .stream(stream)
            .build_legacy()
            .map_err(|source| RunnerConnectorError::Build { source })
    }
}
