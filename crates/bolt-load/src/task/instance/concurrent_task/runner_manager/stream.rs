use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures::Stream;
use pin_project_lite::pin_project;

use crate::task::RunnerId;

#[derive(Debug)]
pub(super) struct RunnerTaggedStreamItem<T> {
    pub runner_id: RunnerId,
    pub item: T,
}

#[derive(Debug)]
pub(super) enum RunnerStreamEvent<T> {
    Item(RunnerTaggedStreamItem<T>),
    Closed(RunnerId),
}

pin_project! {
    pub(super) struct RunnerTaggedStream<S> {
        runner_id: RunnerId,
        #[pin]
        stream: S,
        closed: bool,
    }
}

impl<S> RunnerTaggedStream<S> {
    pub(super) fn new(runner_id: RunnerId, stream: S) -> Self {
        Self {
            runner_id,
            stream,
            closed: false,
        }
    }
}

impl<S> Stream for RunnerTaggedStream<S>
where
    S: Stream,
{
    type Item = RunnerStreamEvent<S::Item>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        if *this.closed {
            return Poll::Ready(None);
        }

        match this.stream.as_mut().poll_next(cx) {
            Poll::Ready(Some(item)) => {
                Poll::Ready(Some(RunnerStreamEvent::Item(RunnerTaggedStreamItem {
                    runner_id: *this.runner_id,
                    item,
                })))
            }
            Poll::Ready(None) => {
                *this.closed = true;
                Poll::Ready(Some(RunnerStreamEvent::Closed(*this.runner_id)))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
