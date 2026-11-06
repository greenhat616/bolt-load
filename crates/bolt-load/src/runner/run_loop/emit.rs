use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, ready},
};

use async_ringbuf::traits::*;
use bolt_load_utils::telemetry::*;
use bytes::Bytes;

use super::{Cancelled, Phase, RunnerLoop};
use crate::{
    runner::{DataFrame, TaskError, TaskRunError, TaskRunner},
    task::ControlEvent,
};

/// `Err` when the push failed or the runner was cancelled, `Ok(Some(_))`
/// when the push succeeded but a control signal failed meanwhile.
pub(super) type EmitResult = Result<Option<TaskRunError>, TaskRunError>;

/// A frame being pushed downstream while the control signals that arrive
/// meanwhile are applied.
///
/// Only the first control error is kept. The stream is not polled while
/// pushing, so waiting for the consumer does not count as idle.
pub(super) struct Emission {
    frame: Option<DataFrame>,
    deferred: Option<TaskRunError>,
    control_open: bool,
}

impl Emission {
    pub(super) fn new(data: Bytes) -> Self {
        Self {
            frame: Some(DataFrame { data }),
            deferred: None,
            control_open: true,
        }
    }

    /// Priority: cancel > stop > control > data vacancy.
    ///
    /// `stopped` is the runner's own stop, when it may interrupt this
    /// emission. It does not once a control error was deferred: the runner
    /// ends with that error after the push.
    pub(super) fn poll(
        &mut self,
        runner: &mut TaskRunner,
        cancelled: &mut Cancelled,
        mut stopped: Option<&mut Cancelled>,
        cx: &mut Context<'_>,
    ) -> Poll<EmitResult> {
        if runner.data_tx.is_none() {
            return Poll::Ready(Err(TaskError::ChannelClosed.into()));
        }
        loop {
            if let Poll::Ready(()) = cancelled.as_mut().poll(cx) {
                return Poll::Ready(Err(TaskRunError::Cancelled));
            }
            if self.deferred.is_none()
                && let Some(stopped) = stopped.as_deref_mut()
                && let Poll::Ready(()) = stopped.as_mut().poll(cx)
            {
                return Poll::Ready(Err(TaskRunError::Cancelled));
            }
            if self.control_open
                && let Poll::Ready(event) = runner.poll_control(cx)
            {
                self.on_control(runner, event);
                continue;
            }
            return self
                .poll_push(runner, cx)
                .map(|pushed| pushed.map(|()| self.deferred.take()));
        }
    }

    fn on_control(&mut self, runner: &mut TaskRunner, event: Result<ControlEvent, TaskRunError>) {
        let error = match event {
            Ok(event) => runner.apply_limit(&event).err(),
            Err(error) => {
                // The manager has released this runner
                self.control_open = false;
                Some(error)
            }
        };
        match error {
            Some(error) if self.deferred.is_some() => {
                warn!("discarding control error during push: {error:?}");
            }
            error => self.deferred = self.deferred.take().or(error),
        }
    }

    fn poll_push(
        &mut self,
        runner: &mut TaskRunner,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), TaskRunError>> {
        let data_tx = runner.data_tx.as_mut().expect("checked by `poll`");
        if !ready!(Pin::new(&mut *data_tx).poll_ready(cx)) {
            return Poll::Ready(Err(TaskError::ChannelClosed.into()));
        }
        let frame = self.frame.take().expect("a frame is pushed once");
        let len = frame.data.len() as u64;
        // The only producer saw a vacant slot.
        if data_tx.try_push(frame).is_err() {
            unreachable!("a vacant slot was filled by another producer");
        }
        runner.downloaded += len;
        Poll::Ready(Ok(()))
    }
}

impl RunnerLoop<'_> {
    #[inline]
    pub(super) fn poll_emit(&mut self, cx: &mut Context<'_>) -> Poll<EmitResult> {
        let Phase::Streaming(state) = &mut self.phase else {
            unreachable!("not streaming");
        };
        let (emission, stoppable) = state.staged_mut().expect("a buffer was staged");
        let stopped = self.stopped.as_mut().filter(|_| stoppable);
        emission.poll(self.runner, &mut self.cancelled, stopped, cx)
    }
}

#[cfg(test)]
impl TaskRunner {
    /// Push `data` the way a running runner does, more of its item
    /// following.
    pub(in crate::runner) async fn emit(&mut self, data: Bytes) -> EmitResult {
        let mut cancelled = Box::pin(self.cancel_token.clone().cancelled_owned());
        let mut stopped = self
            .stop_token
            .clone()
            .map(|stop| Box::pin(stop.cancelled_owned()));
        let mut emission = Emission::new(data);
        std::future::poll_fn(|cx| emission.poll(self, &mut cancelled, stopped.as_mut(), cx)).await
    }
}
