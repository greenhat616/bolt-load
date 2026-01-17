use std::{
    collections::HashMap,
    pin::Pin,
    task::{Context, Poll},
};

use futures::{Stream, StreamExt, stream::BoxStream, task::AtomicWaker};
use futures_concurrency::stream::{StreamGroup, stream_group::Key};
use pin_project_lite::pin_project;

use crate::{
    runner::{RunnerMessage, RunnerMessageConsumer},
    task::RunnerId,
};

// FIXME: handle potential rx closed error?
pin_project! {
    pub struct RunnerNotification {
        #[pin]
        group: StreamGroup<BoxStream<'static, RunnerMessage>>,
        map: HashMap<RunnerId, Key>,
        is_closed: bool,
        waker: AtomicWaker,
    }
}

impl Default for RunnerNotification {
    fn default() -> Self {
        Self::new()
    }
}

impl RunnerNotification {
    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            group: StreamGroup::with_capacity(capacity),
            map: HashMap::with_capacity(capacity),
            is_closed: false,
            waker: AtomicWaker::new(),
        }
    }

    pub fn add(&mut self, runner_id: RunnerId, consumer: RunnerMessageConsumer) {
        let key = self.group.insert(consumer.boxed());
        self.map.insert(runner_id, key);
    }

    pub fn remove(&mut self, runner_id: RunnerId) {
        if let Some(key) = self.map.remove(&runner_id) {
            self.group.remove(key);
        }
    }

    pub fn is_closed(&self) -> bool {
        self.is_closed
    }

    pub fn close(&mut self) {
        self.is_closed = true;
        self.waker.wake();
    }

    pub fn reopen(&mut self) {
        self.is_closed = false;
        self.waker.wake();
    }
}

impl Stream for RunnerNotification {
    type Item = RunnerMessage;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        this.waker.register(cx.waker());

        if this.map.is_empty() {
            return Poll::Pending;
        }

        match this.group.poll_next(cx) {
            Poll::Ready(Some(item)) => Poll::Ready(Some(item)),
            Poll::Ready(None) => {
                if *this.is_closed {
                    Poll::Ready(None)
                } else {
                    Poll::Pending
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
