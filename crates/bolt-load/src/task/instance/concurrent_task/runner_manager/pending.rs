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
    runner::{ConnectionError, DataFrameReceiver, LifecycleReceiver},
    task::RunnerId,
};

pub type RunnerBuilderOutput = (DataFrameReceiver, LifecycleReceiver);

pub type PendingRunnerReceiver = oneshot::Receiver<Result<RunnerBuilderOutput, PendingRunnerError>>;

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
    #[snafu(display("the connection failed: {source}"))]
    Connection { source: ConnectionError },
}

pub struct PendingRunnerOutput {
    pub context: PendingRunnerContext,
    pub result: Result<RunnerBuilderOutput, PendingRunnerError>,
}

impl Future for PendingRunner {
    type Output = PendingRunnerOutput;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        match this.consumer_rx.poll_unpin(cx) {
            Poll::Ready(Ok(Ok(output))) => Poll::Ready(PendingRunnerOutput {
                context: this.context.clone(),
                result: Ok(output),
            }),
            Poll::Ready(Ok(Err(error))) => Poll::Ready(PendingRunnerOutput {
                context: this.context.clone(),
                result: Err(error),
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
