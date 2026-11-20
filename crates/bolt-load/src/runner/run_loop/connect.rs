use std::{
    task::{Context, Poll},
    time::{Duration, Instant},
};

use bolt_load_utils::telemetry::*;

use super::{Phase, RunnerLoop};
use crate::{
    adapter::{AdapterError, AnyBytesStream},
    runner::RunnerInput,
    runtime::{Deadline, TimerSource},
};

/// Waiting for the connection.
pub(super) struct ConnectingState {
    /// Started on the runner's first poll; control signals do not extend it.
    timeout: Deadline,
    started_at: Instant,
}

impl ConnectingState {
    /// Started now, at the runner's first poll, on `source`'s clock.
    pub(super) fn new(timeout: Duration, built_at: Instant, source: &TimerSource) -> Self {
        let started_at = source.now();
        trace!(
            "runner: first polled {:?} after it was built",
            started_at.saturating_duration_since(built_at)
        );
        Self {
            timeout: source.deadline_after(started_at, timeout),
            started_at,
        }
    }
}

impl RunnerLoop<'_> {
    #[inline]
    pub(super) fn poll_connect_timeout(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let Phase::Connecting(state) = &mut self.phase else {
            unreachable!("not connecting");
        };
        state.timeout.poll(cx)
    }

    #[inline]
    pub(super) fn poll_connect(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<AnyBytesStream, AdapterError>> {
        let RunnerInput::Connecting(connect) = &mut self.runner.input else {
            unreachable!("connecting a connected runner");
        };
        connect.as_mut().poll(cx)
    }

    /// Stream from `stream` once `Started` is published.
    pub(super) fn on_connected(&mut self, stream: AnyBytesStream) {
        if let Phase::Connecting(state) = &self.phase {
            trace!(
                "runner: connected in {:?}",
                self.runner
                    .timer_source
                    .now()
                    .saturating_duration_since(state.started_at)
            );
        }
        self.runner.input = RunnerInput::Connected(stream);
    }
}
