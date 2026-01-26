use std::sync::Arc;

use async_ringbuf::{AsyncHeapRb, traits::Split};
use bolt_load_core::adapter::AnyBytesStream;
use smol_cancellation_token::CancellationToken;

use super::{ControlSignalReceiver, RunnerMessage, RunnerMessageConsumer, RunnerMessageSender, TaskRunner};
use crate::{
    DEFAULT_EVENT_CHANNEL_CAPACITY,
    task::RunnerId,
    task::instance::concurrent_task::file_writer::budget_sampler::BudgetSampler,
};

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
    /// the budget sampler for backpressure and speed measurement (optional)
    pub budget_sampler: Option<Arc<BudgetSampler>>,
}

impl TaskRunnerBuilder {
    /// set the total size of the task
    pub fn total(mut self, total: u64) -> Self {
        self.total = Some(total);
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

    /// set the budget sampler for backpressure and speed measurement
    pub fn budget_sampler(mut self, budget_sampler: Option<Arc<BudgetSampler>>) -> Self {
        self.budget_sampler = budget_sampler;
        self
    }

    /// Build the task runner and return the runner and the message consumer
    pub fn build(self) -> Result<(TaskRunner, RunnerMessageConsumer), TaskRunnerBuilderError> {
        let runner_id = self
            .runner_id
            .ok_or(TaskRunnerBuilderError::RunnerIdNotSet)?;
        let rb = AsyncHeapRb::<RunnerMessage>::new(DEFAULT_EVENT_CHANNEL_CAPACITY);
        let (prod, cons) = rb.split();
        let runner = TaskRunner {
            total: self.total,
            downloaded: 0,
            stream: self.stream.ok_or(TaskRunnerBuilderError::StreamNotSet)?,

            control_signal: self
                .control_signal
                .ok_or(TaskRunnerBuilderError::ControlSignalNotSet)?,
            notify: RunnerMessageSender::new(runner_id, prod),
            cancel_token: self
                .cancel_token
                .ok_or(TaskRunnerBuilderError::CancelTokenNotSet)?,
            shutdown_rx: None,
            budget_sampler: self.budget_sampler,
        };
        Ok((runner, cons))
    }
}
