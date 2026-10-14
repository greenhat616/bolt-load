use std::task::{Context, Poll};

use super::RunnerLoop;
use crate::{
    adapter::{AdapterError, AnyBytesStream},
    runner::RunnerInput,
};

impl RunnerLoop<'_> {
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
        self.runner.input = RunnerInput::Connected(stream);
    }
}
