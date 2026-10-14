use std::{
    pin::Pin,
    task::{Context, Poll, ready},
};

use async_ringbuf::traits::*;

use super::{Phase, RunResult, RunnerLoop};
use crate::runner::{LifecycleEvent, LifecycleSender, TaskError, TaskRunError};

/// Push `event` once the lifecycle channel has room; `false` when its
/// consumer is gone.
fn poll_publish(
    lifecycle_tx: &mut LifecycleSender,
    event: &mut Option<LifecycleEvent>,
    cx: &mut Context<'_>,
) -> Poll<bool> {
    let open = ready!(Pin::new(&mut *lifecycle_tx).poll_ready(cx));
    let event = event.take().expect("an event is published once");
    // The only producer saw a vacant slot.
    if open && lifecycle_tx.try_push(event).is_err() {
        unreachable!("a vacant slot was filled by another producer");
    }
    Poll::Ready(open)
}

impl RunnerLoop<'_> {
    pub(super) fn poll_started(&mut self, cx: &mut Context<'_>) -> Poll<RunResult> {
        let mut started = Some(LifecycleEvent::Started);
        poll_publish(&mut self.runner.lifecycle_tx, &mut started, cx).map(|open| {
            open.then_some(())
                .ok_or(TaskRunError::Failed(TaskError::ChannelClosed))
        })
    }

    pub(super) fn publish_stopped(&mut self, result: RunResult) {
        let event = match result {
            Ok(()) => LifecycleEvent::finished(),
            Err(TaskRunError::Cancelled) => LifecycleEvent::failed(TaskError::Cancelled),
            Err(TaskRunError::Failed(error)) => LifecycleEvent::failed(error),
        };
        self.phase = Phase::PublishingStopped(Some(event));
    }

    /// A consumer that is gone misses the event.
    pub(super) fn poll_stopped(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let Phase::PublishingStopped(event) = &mut self.phase else {
            unreachable!("not stopping");
        };
        poll_publish(&mut self.runner.lifecycle_tx, event, cx).map(drop)
    }
}
