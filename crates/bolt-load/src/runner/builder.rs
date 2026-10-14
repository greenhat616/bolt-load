use async_ringbuf::{AsyncHeapRb, traits::Split};
use bolt_load_core::adapter::AnyBytesStream;
use smol_cancellation_token::CancellationToken;

use super::{
    ControlSignalReceiver, ControlSource, DATA_DRAIN_TIMEOUT, DATA_FRAME_CHANNEL_CAPACITY,
    DEFAULT_CONNECT_TIMEOUT, DEFAULT_IDLE_TIMEOUT, DataFrame, DataFrameReceiver,
    LIFECYCLE_CHANNEL_CAPACITY, LifecycleEvent, LifecycleReceiver, LimitReceiver, RunnerInput,
    StreamConnector, TaskRunner,
};
use crate::task::RunnerId;

#[derive(Debug, snafu::Snafu)]
pub enum TaskRunnerBuilderError {
    #[snafu(display("neither a stream nor a connector is set"))]
    /// Neither [`TaskRunnerBuilder::stream`] nor
    /// [`TaskRunnerBuilder::connector`] was called.
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
    total: Option<u64>,
    /// the stream of the task, or how to connect it
    #[debug("input is set: {}", input.is_some())]
    input: Option<RunnerInput>,
    /// the runner id of the task
    runner_id: Option<RunnerId>,
    /// where the control signals come from
    #[debug(skip)]
    control: Option<ControlSource>,
    /// the cancel token of the task
    cancel_token: Option<CancellationToken>,
    /// optional override for tests that need to exercise drain timeout paths quickly
    data_drain_timeout: Option<std::time::Duration>,
    /// how long the stream may yield no non-empty data before the runner fails with `Timeout`
    idle_timeout: Option<std::time::Duration>,
    /// how long connecting may take before the runner fails with `Timeout`
    connect_timeout: Option<std::time::Duration>,
}

impl TaskRunnerBuilder {
    /// set the total size of the task
    pub fn total(mut self, total: u64) -> Self {
        self.total = Some(total);
        self
    }

    /// set the stream of the task, replacing [`Self::connector`]
    pub fn stream(mut self, stream: AnyBytesStream) -> Self {
        self.input = Some(RunnerInput::Connected(stream));
        self
    }

    /// set how to connect the stream of the task, replacing [`Self::stream`]
    ///
    /// The runner connects it before reporting
    /// [`Started`](super::LifecycleEvent::Started); a failed connection stops
    /// the runner with [`TaskError::StreamError`](super::TaskError::StreamError)
    /// without reporting `Started`.
    pub fn connector(mut self, connector: impl Into<StreamConnector>) -> Self {
        self.input = Some(match connector.into() {
            StreamConnector::DirectStream(stream) => RunnerInput::Connected(stream),
            StreamConnector::StreamConnector(connect) => RunnerInput::Connecting(connect),
        });
        self
    }

    /// set the runner id of the task
    pub fn runner_id(mut self, runner_id: RunnerId) -> Self {
        self.runner_id = Some(runner_id);
        self
    }

    /// set the receiver of the control signal
    ///
    /// Every signal is applied in order; a limit that does not lower a known
    /// total fails the runner with
    /// [`TaskError::ExceededTotalSize`](super::TaskError::ExceededTotalSize).
    pub fn control_signal(mut self, control_signal: ControlSignalReceiver) -> Self {
        self.control = Some(ControlSource::Channel(Box::pin(control_signal)));
        self
    }

    /// Receive only the latest limit from `limits`, replacing
    /// [`Self::control_signal`].
    pub(crate) fn limit_receiver(mut self, limits: LimitReceiver) -> Self {
        self.control = Some(ControlSource::Latest(limits));
        self
    }

    /// set the cancel token of the task
    pub fn cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = Some(cancel_token);
        self
    }

    /// set the idle timeout of the stream, defaults to [`DEFAULT_IDLE_TIMEOUT`]
    pub fn idle_timeout(mut self, idle_timeout: std::time::Duration) -> Self {
        self.idle_timeout = Some(idle_timeout);
        self
    }

    /// set how long the [`Self::connector`] may take, defaults to
    /// [`DEFAULT_CONNECT_TIMEOUT`]
    ///
    /// The time runs from the runner's first poll and is not extended by
    /// control signals; on expiry the runner fails with the retryable
    /// [`TaskError::Timeout`](super::TaskError::Timeout) without reporting
    /// `Started`. It does not apply to a [`Self::stream`], which is
    /// connected already.
    pub fn connect_timeout(mut self, connect_timeout: std::time::Duration) -> Self {
        self.connect_timeout = Some(connect_timeout);
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
            input: self.input.ok_or(TaskRunnerBuilderError::StreamNotSet)?,

            control: self
                .control
                .ok_or(TaskRunnerBuilderError::ControlSignalNotSet)?,
            lifecycle_tx: lifecycle_prod,
            data_tx: Some(data_prod),
            data_drain_timeout: self.data_drain_timeout.unwrap_or(DATA_DRAIN_TIMEOUT),
            idle_timeout: self.idle_timeout.unwrap_or(DEFAULT_IDLE_TIMEOUT),
            connect_timeout: self.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT),
            built_at: std::time::Instant::now(),
            cancel_token: self
                .cancel_token
                .ok_or(TaskRunnerBuilderError::CancelTokenNotSet)?,
        };
        Ok((runner, lifecycle_cons, data_cons))
    }
}
