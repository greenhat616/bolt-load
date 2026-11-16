use std::{
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use bolt_load_utils::telemetry::*;

use super::{Phase, RunnerLoop};
use crate::{
    adapter::{AdapterError, AnyBytesStream},
    runner::RunnerInput,
};

/// Waiting for the connection.
pub(super) struct ConnectingState {
    /// Started on the runner's first poll; control signals do not extend it.
    timeout: async_io::Timer,
    started_at: Instant,
}

impl ConnectingState {
    /// Started at `started_at`, the runner's first poll.
    pub(super) fn new(timeout: Duration, built_at: Instant, started_at: Instant) -> Self {
        trace!(
            "runner: first polled {:?} after it was built",
            started_at - built_at
        );
        Self {
            timeout: async_io::Timer::after(timeout),
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
        Pin::new(&mut state.timeout).poll(cx).map(drop)
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
