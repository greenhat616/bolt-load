use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use async_ringbuf::traits::*;
use bolt_load_utils::telemetry::*;

use super::{Cancelled, Phase, RunResult, RunnerLoop};
use crate::runner::{DataFrameSender, TaskError, TaskRunError, TaskRunner};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::runner) enum DataDrainOutcome {
    Drained,
    Cancelled,
    TimedOut,
}

/// Waiting for the consumer to take the delivered frames before reporting
/// `result`.
pub(super) struct DrainState {
    result: RunResult,
    /// Started on the first poll.
    timeout: Option<async_io::Timer>,
}

impl DrainState {
    pub(super) fn new(result: RunResult) -> Self {
        Self {
            result,
            timeout: None,
        }
    }
}

impl TaskRunner {
    pub(super) fn should_wait_data_drain(result: &RunResult) -> bool {
        matches!(
            result,
            Ok(())
                | Err(TaskRunError::Failed(
                    TaskError::ExceededTotalSize
                        | TaskError::StreamError { .. }
                        | TaskError::Timeout,
                ))
        )
    }
}

/// Ready once the channel is empty or its consumer is gone.
fn poll_drained(data_tx: &DataFrameSender, cx: &mut Context<'_>) -> Poll<()> {
    let drained = || data_tx.is_closed() || data_tx.is_empty();
    if drained() {
        return Poll::Ready(());
    }
    data_tx.register_waker(cx.waker());
    // The consumer may have taken the last frame before the waker was registered.
    if drained() {
        return Poll::Ready(());
    }
    Poll::Pending
}

/// Priority: cancel > drained or consumer closed > timeout.
fn poll_drain(
    runner: &TaskRunner,
    cancelled: &mut Cancelled,
    timeout: &mut Option<async_io::Timer>,
    cx: &mut Context<'_>,
) -> Poll<DataDrainOutcome> {
    let Some(data_tx) = runner.data_tx.as_ref() else {
        return Poll::Ready(DataDrainOutcome::Drained);
    };
    if let Poll::Ready(()) = cancelled.as_mut().poll(cx) {
        return Poll::Ready(DataDrainOutcome::Cancelled);
    }
    if let Poll::Ready(()) = poll_drained(data_tx, cx) {
        return Poll::Ready(DataDrainOutcome::Drained);
    }
    let timeout = timeout.get_or_insert_with(|| async_io::Timer::after(runner.data_drain_timeout));
    Pin::new(timeout)
        .poll(cx)
        .map(|_| DataDrainOutcome::TimedOut)
}

impl RunnerLoop<'_> {
    #[inline]
    pub(super) fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<DataDrainOutcome> {
        let Phase::Draining(state) = &mut self.phase else {
            unreachable!("not draining");
        };
        poll_drain(self.runner, &mut self.cancelled, &mut state.timeout, cx)
    }

    pub(super) fn finish_drain(&mut self, outcome: DataDrainOutcome) {
        let Phase::Draining(state) = std::mem::replace(&mut self.phase, Phase::Stopped) else {
            unreachable!("not draining");
        };
        let result = match outcome {
            DataDrainOutcome::Drained => state.result,
            DataDrainOutcome::Cancelled => Err(TaskRunError::Cancelled),
            DataDrainOutcome::TimedOut => {
                warn!("data drain timed out; reporting as timeout so undrained bytes are retried");
                Err(TaskError::Timeout.into())
            }
        };
        self.publish_stopped(result);
    }
}

#[cfg(test)]
impl TaskRunner {
    /// Wait for the data channel to drain the way a stopping runner does.
    pub(in crate::runner) async fn wait_data_drained(&mut self) -> DataDrainOutcome {
        let mut cancelled = Box::pin(self.cancel_token.clone().cancelled_owned());
        let mut timeout = None;
        std::future::poll_fn(|cx| poll_drain(self, &mut cancelled, &mut timeout, cx)).await
    }
}
