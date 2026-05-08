use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures::Stream;
use pin_project_lite::pin_project;

use crate::task::RunnerId;

pin_project! {
    pub struct RunnerTaggedStream<S: Stream> {
        runner_id: RunnerId,
        #[pin]
        stream: S,
        closed_emitted: bool,
    }
}

impl<S: Stream> RunnerTaggedStream<S> {
    pub fn new(runner_id: RunnerId, stream: S) -> Self {
        Self {
            runner_id,
            stream,
            closed_emitted: false,
        }
    }
}

#[derive(Debug)]
pub struct RunnerTaggedStreamItem<T> {
    pub runner_id: RunnerId,
    pub item: T,
}

impl<T> RunnerTaggedStreamItem<T> {
    pub fn new(runner_id: RunnerId, item: T) -> Self {
        Self { runner_id, item }
    }
}

pub enum RunnerStreamEvent<T> {
    Item(RunnerTaggedStreamItem<T>),
    Closed(RunnerId),
}

impl<S: Stream> Stream for RunnerTaggedStream<S> {
    type Item = RunnerStreamEvent<S::Item>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        match this.stream.poll_next(cx) {
            Poll::Ready(Some(item)) => {
                Poll::Ready(Some(RunnerStreamEvent::Item(RunnerTaggedStreamItem {
                    runner_id: *this.runner_id,
                    item,
                })))
            }
            Poll::Ready(None) => {
                if !*this.closed_emitted {
                    *this.closed_emitted = true;
                    Poll::Ready(Some(RunnerStreamEvent::Closed(*this.runner_id)))
                } else {
                    Poll::Ready(None)
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.stream.size_hint()
    }
}
