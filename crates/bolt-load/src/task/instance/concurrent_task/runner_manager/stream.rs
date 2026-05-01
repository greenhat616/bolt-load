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
    }
}

impl<S: Stream> RunnerTaggedStream<S> {
    pub fn new(runner_id: RunnerId, stream: S) -> Self {
        Self { runner_id, stream }
    }
}

pub struct RunnerTaggedStreamItem<T> {
    pub runner_id: RunnerId,
    pub item: T,
}

impl<T> RunnerTaggedStreamItem<T> {
    pub fn new(runner_id: RunnerId, item: T) -> Self {
        Self { runner_id, item }
    }
}

impl<S: Stream> Stream for RunnerTaggedStream<S> {
    type Item = RunnerTaggedStreamItem<S::Item>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        match this.stream.poll_next(cx) {
            Poll::Ready(item) => Poll::Ready(item.map(|item| RunnerTaggedStreamItem {
                runner_id: *this.runner_id,
                item,
            })),
            Poll::Pending => Poll::Pending,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.stream.size_hint()
    }
}
