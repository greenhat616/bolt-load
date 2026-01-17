use bolt_load_core::adapter::{AdapterError, AnyBytesStream};
use futures::{FutureExt, future::BoxFuture};
use snafu::ResultExt;

use super::{RunnerMessageConsumer, TaskRunner, TaskRunnerBuilder, TaskRunnerBuilderError};

pub enum StreamConnector {
    DirectStream(AnyBytesStream),
    StreamConnector(BoxFuture<'static, Result<AnyBytesStream, AdapterError>>),
}

#[derive(Debug, snafu::Snafu)]
pub enum RunnerConnectorError {
    #[snafu(display("build failed: {source}"))]
    Build { source: TaskRunnerBuilderError },
    #[snafu(display("connection failed: {source}"))]
    Connection { source: AdapterError },
}

impl RunnerConnectorError {
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Connection {
                source: AdapterError::Retryable { .. }
            }
        )
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
        let stream = match connector {
            StreamConnector::DirectStream(stream) => stream,
            StreamConnector::StreamConnector(connector) => {
                connector.await.context(ConnectionSnafu)?
            }
        };

        builder.stream(stream).build().context(BuildSnafu)
    }
}
