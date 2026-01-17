use std::{
    ops::Range,
    pin::Pin,
    task::{Context, Poll},
};

use futures::{
    FutureExt, Stream,
    future::{Fuse, FusedFuture},
    task::AtomicWaker,
};
use futures_concurrency::future::FutureGroup;

use crate::{
    runner::{RunnerConnectorError, RunnerMessageConsumer},
    task::RunnerId,
};

pub type PendingRunnerReceiver =
    oneshot::Receiver<Result<RunnerMessageConsumer, RunnerConnectorError>>;

#[derive(Debug, Clone)]
pub struct PendingRunnerContext {
    pub runner_id: RunnerId,
    pub range: Range<u64>,
}

impl PendingRunnerContext {
    pub fn new(runner_id: RunnerId, range: Range<u64>) -> Self {
        Self { runner_id, range }
    }
}

pin_project_lite::pin_project! {
    /// Pending Runner Creation
    struct PendingRunner {
        context: PendingRunnerContext,
        #[pin]
        consumer_rx: Fuse<PendingRunnerReceiver>,
    }
}

#[derive(Debug, snafu::Snafu)]
pub enum PendingRunnerError {
    #[snafu(display("the consumer receiver is closed"))]
    ReceiverClosed,
    #[snafu(display("the connector failed: {source}"))]
    Connector { source: RunnerConnectorError },
}

pub struct PendingRunnerOutput {
    pub context: PendingRunnerContext,
    pub result: Result<RunnerMessageConsumer, PendingRunnerError>,
}

impl Future for PendingRunner {
    type Output = PendingRunnerOutput;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        match this.consumer_rx.poll_unpin(cx) {
            Poll::Ready(Ok(Ok(consumer))) => Poll::Ready(PendingRunnerOutput {
                context: this.context.clone(),
                result: Ok(consumer),
            }),
            Poll::Ready(Ok(Err(error))) => Poll::Ready(PendingRunnerOutput {
                context: this.context.clone(),
                result: Err(PendingRunnerError::Connector { source: error }),
            }),
            Poll::Ready(Err(_)) => Poll::Ready(PendingRunnerOutput {
                context: this.context.clone(),
                result: Err(PendingRunnerError::ReceiverClosed),
            }),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl FusedFuture for PendingRunner {
    fn is_terminated(&self) -> bool {
        self.consumer_rx.is_terminated()
    }
}

pin_project_lite::pin_project! {
    pub struct PendingRunnerGroup {
        waker: AtomicWaker,
        #[pin]
        group: FutureGroup<PendingRunner>,
    }
}

impl PendingRunnerGroup {
    pub fn new() -> Self {
        Self {
            waker: AtomicWaker::new(),
            group: FutureGroup::new(),
        }
    }

    pub fn insert(&mut self, context: PendingRunnerContext, consumer_rx: PendingRunnerReceiver) {
        self.group.insert(PendingRunner {
            context,
            consumer_rx: consumer_rx.fuse(),
        });
        self.waker.wake();
    }
}

impl Stream for PendingRunnerGroup {
    type Item = PendingRunnerOutput;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        this.waker.register(cx.waker());
        if this.group.is_empty() {
            return Poll::Pending;
        }

        match this.group.poll_next(cx) {
            Poll::Ready(Some(output)) => Poll::Ready(Some(output)),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}
