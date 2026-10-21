use std::{
    pin::Pin,
    task::{Context, Poll},
};

use smol_cancellation_token::{CancellationToken, WaitForCancellationFutureOwned};

use super::LoopEvent;

/// The task's cancellation, registered once per attempt.
pub(super) struct CancelSource {
    cancelled: Pin<Box<WaitForCancellationFutureOwned>>,
}

impl CancelSource {
    pub(super) fn new(token: CancellationToken) -> Self {
        Self {
            cancelled: Box::pin(token.cancelled_owned()),
        }
    }

    #[inline]
    pub(super) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<LoopEvent> {
        self.cancelled
            .as_mut()
            .poll(cx)
            .map(|()| LoopEvent::Cancelled)
    }
}
