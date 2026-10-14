//! The runner's event loop: a state machine polled in place.
//!
//! Every event source lives as long as the loop, so no future is rebuilt per
//! event. Each phase polls its own sources in priority order:
//!
//! | Phase | Priority |
//! |---|---|
//! | `Connecting` | cancel > control > connect timeout > connection |
//! | `PublishingStarted` | lifecycle vacancy |
//! | `Streaming`, reading | cancel > control > idle timeout > stream |
//! | `Streaming`, emitting | cancel > control > data vacancy |
//! | `Draining` | cancel > drained or consumer closed > drain timeout |
//! | `PublishingStopped` | lifecycle vacancy |

use std::{
    future::{Future, poll_fn},
    pin::Pin,
    task::{Context, Poll, ready},
};

use bolt_load_utils::telemetry::*;
use bytes::Bytes;
use smol_cancellation_token::WaitForCancellationFutureOwned;

use self::{
    connect::ConnectingState,
    drain::{DataDrainOutcome, DrainState},
    emit::EmitResult,
    stream::StreamingState,
};
use super::{LifecycleEvent, RunnerInput, TaskError, TaskRunError, TaskRunner};
use crate::{
    adapter::{AdapterError, AnyBytesStream},
    task::ControlEvent,
};

mod connect;
pub(super) mod drain;
mod emit;
mod lifecycle;
mod stream;

type RunResult = Result<(), TaskRunError>;
type Cancelled = Pin<Box<WaitForCancellationFutureOwned>>;

enum Phase {
    /// Limits received meanwhile apply once streaming; the range requested
    /// from the server is already fixed.
    Connecting(ConnectingState),
    PublishingStarted,
    Streaming(StreamingState),
    Draining(DrainState),
    PublishingStopped(Option<LifecycleEvent>),
    Stopped,
}

/// Why a buffer is being emitted, deciding what follows a successful push.
enum Continuation {
    /// Buffer the rest of the current item.
    ResumeItem,
    /// The stream ended or the size limit was reached.
    Validate,
    IdleTimeout,
    StreamError(AdapterError),
}

/// The event a phase selected.
#[derive(derive_more::Debug)]
pub(super) enum Step {
    #[debug("Connected(..)")]
    Connected(Result<AnyBytesStream, AdapterError>),
    ConnectTimeout,
    Started(RunResult),
    Cancelled,
    Control(Result<ControlEvent, TaskRunError>),
    IdleTimeout,
    #[debug("Download(..)")]
    Download(Option<Result<Bytes, AdapterError>>),
    Emitted(EmitResult),
    Drained(DataDrainOutcome),
    StoppedPublished,
}

/// The state of one [`TaskRunner::run`] call.
pub(super) struct RunnerLoop<'a> {
    runner: &'a mut TaskRunner,
    phase: Phase,
    /// Allocated once per run.
    cancelled: Cancelled,
}

impl TaskRunner {
    /// run the task runner
    /// This function will block until the task is finished or cancelled
    /// It should be called in a new thread or async spawn context
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip(self), name = "TaskRunner::run", fields(runner_id = self.id))
    )]
    pub async fn run(&mut self) {
        let mut run = RunnerLoop::new(self);
        poll_fn(|cx| run.poll_run(cx)).await;
    }

    /// Like [`Self::run`], but once `Started` is published, wait for `ready`
    /// before streaming, so a consumer can prepare for the data without the
    /// idle and drain timeouts running.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, name = "TaskRunner::run", fields(runner_id = self.id))
    )]
    pub(crate) async fn run_after_started(&mut self, ready: impl Future<Output = ()>) {
        let mut run = RunnerLoop::new(self);
        if poll_fn(|cx| run.poll_until_started(cx)).await {
            ready.await;
            run.handle_step(Step::Started(Ok(())));
            poll_fn(|cx| run.poll_run(cx)).await;
        }
    }
}

impl<'a> RunnerLoop<'a> {
    pub(super) fn new(runner: &'a mut TaskRunner) -> Self {
        let cancelled = Box::pin(runner.cancel_token.clone().cancelled_owned());
        let phase = match runner.input {
            RunnerInput::Connected(_) => Phase::PublishingStarted,
            RunnerInput::Connecting(_) => Phase::Connecting(ConnectingState::new(
                runner.connect_timeout,
                runner.built_at,
            )),
            RunnerInput::Abandoned => Phase::Stopped,
        };
        Self {
            runner,
            phase,
            cancelled,
        }
    }

    /// Run until `Started` is published, without handling it; `false` when
    /// the runner stopped before.
    fn poll_until_started(&mut self, cx: &mut Context<'_>) -> Poll<bool> {
        while !matches!(self.phase, Phase::Stopped) {
            let step = ready!(self.poll_step(cx));
            if matches!(step, Step::Started(Ok(()))) {
                return Poll::Ready(true);
            }
            self.handle_step(step);
        }
        Poll::Ready(false)
    }

    fn poll_run(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        while !matches!(self.phase, Phase::Stopped) {
            let step = ready!(self.poll_step(cx));
            self.handle_step(step);
        }
        Poll::Ready(())
    }

    /// Poll the sources of the current phase, see the module docs.
    pub(super) fn poll_step(&mut self, cx: &mut Context<'_>) -> Poll<Step> {
        match &self.phase {
            Phase::Connecting(_) => {
                if let Poll::Ready(()) = self.poll_cancel(cx) {
                    return Poll::Ready(Step::Cancelled);
                }
                if let Poll::Ready(event) = self.runner.poll_control(cx) {
                    return Poll::Ready(Step::Control(event));
                }
                if let Poll::Ready(()) = self.poll_connect_timeout(cx) {
                    return Poll::Ready(Step::ConnectTimeout);
                }
                self.poll_connect(cx).map(Step::Connected)
            }
            Phase::PublishingStarted => self.poll_started(cx).map(Step::Started),
            Phase::Streaming(state) if state.is_emitting() => self.poll_emit(cx).map(Step::Emitted),
            Phase::Streaming(_) => {
                if let Poll::Ready(()) = self.poll_cancel(cx) {
                    return Poll::Ready(Step::Cancelled);
                }
                if let Poll::Ready(event) = self.runner.poll_control(cx) {
                    return Poll::Ready(Step::Control(event));
                }
                if let Poll::Ready(()) = self.poll_idle(cx) {
                    return Poll::Ready(Step::IdleTimeout);
                }
                self.poll_read(cx).map(Step::Download)
            }
            Phase::Draining(_) => self.poll_drain(cx).map(Step::Drained),
            Phase::PublishingStopped(_) => self.poll_stopped(cx).map(|()| Step::StoppedPublished),
            Phase::Stopped => unreachable!("a stopped runner is not polled"),
        }
    }

    #[inline]
    fn poll_cancel(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        self.cancelled.as_mut().poll(cx)
    }

    pub(super) fn handle_step(&mut self, step: Step) {
        match step {
            Step::Connected(Ok(stream)) => {
                self.on_connected(stream);
                self.phase = Phase::PublishingStarted;
            }
            Step::Connected(Err(source)) => {
                warn!("runner: failed to connect: {source}");
                self.begin_stop(Err(TaskError::StreamError { source }.into()));
            }
            Step::ConnectTimeout => {
                warn!(
                    phase = "connecting",
                    "runner: not connected within {:?}", self.runner.connect_timeout
                );
                self.begin_stop(Err(TaskError::Timeout.into()));
            }
            Step::Started(Ok(())) => {
                trace!("runner: started");
                self.phase = Phase::Streaming(StreamingState::new(self.runner.idle_timeout));
            }
            Step::Started(Err(error)) => self.begin_stop(Err(error)),
            Step::Cancelled => {
                trace!("runner: cancelled");
                self.begin_stop(Err(TaskRunError::Cancelled));
            }
            Step::Control(event) => {
                trace!("runner: control signal: {event:?}");
                if let Err(error) = event.and_then(|event| self.runner.apply_limit(&event)) {
                    self.begin_stop(Err(error));
                }
            }
            Step::IdleTimeout => {
                warn!(
                    "runner: no data received in the last {:?}",
                    self.runner.idle_timeout
                );
                self.flush(Continuation::IdleTimeout);
            }
            Step::Download(None) => self.flush(Continuation::Validate),
            Step::Download(Some(Err(source))) => self.flush(Continuation::StreamError(source)),
            Step::Download(Some(Ok(item))) => self.on_item(item),
            Step::Emitted(result) => self.on_emitted(result),
            Step::Drained(outcome) => self.finish_drain(outcome),
            Step::StoppedPublished => {
                self.runner.data_tx.take();
                self.phase = Phase::Stopped;
            }
        }
    }

    /// Stop with `result`, draining the data channel first when the
    /// delivered bytes count.
    fn begin_stop(&mut self, result: RunResult) {
        if matches!(self.phase, Phase::Connecting(_)) {
            // Release the connection before reporting the stop.
            self.runner.input = RunnerInput::Abandoned;
        }
        if TaskRunner::should_wait_data_drain(&result) {
            self.phase = Phase::Draining(DrainState::new(result));
        } else {
            self.publish_stopped(result);
        }
    }

    /// Continue after an emitted buffer.
    ///
    /// Cancellation and a failed push win over everything; an upstream error
    /// wins over a control error deferred by the push.
    fn resume_after_emit(&mut self, then: Continuation, result: EmitResult) {
        let deferred = match result {
            Ok(deferred) => deferred,
            Err(error) => return self.begin_stop(Err(error)),
        };
        match (then, deferred) {
            (Continuation::StreamError(source), _) => {
                self.begin_stop(Err(TaskError::StreamError { source }.into()));
            }
            (_, Some(error)) => self.begin_stop(Err(error)),
            (Continuation::ResumeItem, None) => self.resume_item(),
            (Continuation::Validate, None) => {
                let result = self.runner.validate_size();
                self.begin_stop(result);
            }
            (Continuation::IdleTimeout, None) => self.begin_stop(Err(TaskError::Timeout.into())),
        }
    }
}
