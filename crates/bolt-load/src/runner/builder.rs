use async_ringbuf::{AsyncHeapRb, traits::Split};
use bolt_load_core::adapter::AnyBytesStream;
use smol_cancellation_token::CancellationToken;

use super::{
    ControlSignalReceiver, DATA_DRAIN_TIMEOUT, DATA_FRAME_CHANNEL_CAPACITY, DataFrame,
    DataFrameReceiver, LIFECYCLE_CHANNEL_CAPACITY, LifecycleEvent, LifecycleReceiver,
    RunnerMessageConsumer, TaskRunner, legacy_consumer,
};
use crate::task::RunnerId;

#[derive(Debug, snafu::Snafu)]
pub enum TaskRunnerBuilderError {
    #[snafu(display("stream is not set"))]
    StreamNotSet,
    #[snafu(display("runner id is not set"))]
    RunnerIdNotSet,
    #[snafu(display("control signal is not set"))]
    ControlSignalNotSet,
    #[snafu(display("cancel token is not set"))]
    CancelTokenNotSet,
}

#[derive(derive_more::Debug, Default)]
pub struct TaskRunnerBuilder {
    /// the total size of the task
    pub total: Option<u64>,
    /// the stream of the task
    #[debug("stream is set: {}", stream.is_some())]
    pub stream: Option<AnyBytesStream>,
    /// the runner id of the task
    pub runner_id: Option<RunnerId>,
    /// the receiver of the control signal
    pub control_signal: Option<ControlSignalReceiver>,
    /// the cancel token of the task
    pub cancel_token: Option<CancellationToken>,
    /// optional override for tests that need to exercise drain timeout paths quickly
    pub data_drain_timeout: Option<std::time::Duration>,
}

impl TaskRunnerBuilder {
    /// set the total size of the task
    pub fn total(mut self, total: u64) -> Self {
        self.total = Some(total);
        self
    }

    pub fn with_optional_total(mut self, total: Option<u64>) -> Self {
        self.total = total;
        self
    }

    /// set the stream of the task
    pub fn stream(mut self, stream: AnyBytesStream) -> Self {
        self.stream = Some(stream);
        self
    }

    /// set the runner id of the task
    pub fn runner_id(mut self, runner_id: RunnerId) -> Self {
        self.runner_id = Some(runner_id);
        self
    }

    /// set the receiver of the control signal
    pub fn control_signal(mut self, control_signal: ControlSignalReceiver) -> Self {
        self.control_signal = Some(control_signal);
        self
    }

    /// set the cancel token of the task
    pub fn cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = Some(cancel_token);
        self
    }

    #[cfg(test)]
    pub fn data_drain_timeout(mut self, data_drain_timeout: std::time::Duration) -> Self {
        self.data_drain_timeout = Some(data_drain_timeout);
        self
    }

    /// Build the task runner and return the runner, lifecycle receiver, and data receiver
    pub fn build(
        self,
    ) -> Result<(TaskRunner, LifecycleReceiver, DataFrameReceiver), TaskRunnerBuilderError> {
        self.build_inner(None)
    }

    pub fn build_legacy(
        self,
    ) -> Result<(TaskRunner, RunnerMessageConsumer), TaskRunnerBuilderError> {
        let (mut runner, lifecycle_rx, data_rx) = self.build()?;
        runner.legacy_mode = true;
        let consumer = legacy_consumer(runner.id, lifecycle_rx, data_rx);
        Ok((runner, consumer))
    }

    pub(crate) fn build_legacy_without_stream(
        self,
    ) -> Result<(TaskRunner, RunnerMessageConsumer), TaskRunnerBuilderError> {
        let stream: AnyBytesStream = Box::pin(futures::stream::pending());
        let (mut runner, lifecycle_rx, data_rx) = self.build_inner(Some(stream))?;
        runner.legacy_mode = true;
        let consumer = legacy_consumer(runner.id, lifecycle_rx, data_rx);
        Ok((runner, consumer))
    }

    fn build_inner(
        self,
        stream_override: Option<AnyBytesStream>,
    ) -> Result<(TaskRunner, LifecycleReceiver, DataFrameReceiver), TaskRunnerBuilderError> {
        let runner_id = self
            .runner_id
            .ok_or(TaskRunnerBuilderError::RunnerIdNotSet)?;
        let lifecycle_rb = AsyncHeapRb::<LifecycleEvent>::new(LIFECYCLE_CHANNEL_CAPACITY);
        let data_rb = AsyncHeapRb::<DataFrame>::new(DATA_FRAME_CHANNEL_CAPACITY);

        let (lifecycle_prod, lifecycle_cons) = lifecycle_rb.split();
        let (data_prod, data_cons) = data_rb.split();
        let runner = TaskRunner {
            id: runner_id,
            total: self.total,
            downloaded: 0,
            stream: stream_override
                .or(self.stream)
                .ok_or(TaskRunnerBuilderError::StreamNotSet)?,

            control_signal: self
                .control_signal
                .ok_or(TaskRunnerBuilderError::ControlSignalNotSet)?,
            lifecycle_tx: lifecycle_prod,
            data_tx: Some(data_prod),
            data_drain_timeout: self.data_drain_timeout.unwrap_or(DATA_DRAIN_TIMEOUT),
            legacy_mode: false,
            cancel_token: self
                .cancel_token
                .ok_or(TaskRunnerBuilderError::CancelTokenNotSet)?,
            shutdown_rx: None,
        };
        Ok((runner, lifecycle_cons, data_cons))
    }
}
