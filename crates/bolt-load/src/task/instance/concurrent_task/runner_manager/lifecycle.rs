use std::{
    collections::HashMap,
    pin::Pin,
    task::{Context, Poll},
};

use futures::Stream;
use futures_concurrency::stream::{StreamGroup, stream_group::Key};
use pin_project_lite::pin_project;

use super::stream::{RunnerStreamEvent, RunnerTaggedStream, RunnerTaggedStreamItem};
use crate::{
    runner::{LifecycleEvent, LifecycleReceiver},
    task::RunnerId,
};

pin_project! {
    pub struct LifecycleAggregator {
        #[pin]
        group: StreamGroup<RunnerTaggedStream<LifecycleReceiver>>,
        map: HashMap<RunnerId, Key>,
    }
}

impl Default for LifecycleAggregator {
    fn default() -> Self {
        Self::new()
    }
}

impl LifecycleAggregator {
    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            group: StreamGroup::with_capacity(capacity),
            map: HashMap::with_capacity(capacity),
        }
    }

    pub fn add(&mut self, runner_id: RunnerId, receiver: LifecycleReceiver) {
        let key = self
            .group
            .insert(RunnerTaggedStream::new(runner_id, receiver));
        self.map.insert(runner_id, key);
    }

    pub fn remove(&mut self, runner_id: RunnerId) {
        if let Some(key) = self.map.remove(&runner_id) {
            self.group.remove(key);
        }
    }
}

#[derive(Debug)]
pub enum LifecycleStreamEvent {
    Event(RunnerTaggedStreamItem<LifecycleEvent>),
    Closed(RunnerId),
}

impl Stream for LifecycleAggregator {
    type Item = LifecycleStreamEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        if this.map.is_empty() {
            return Poll::Pending;
        }

        match this.group.as_mut().poll_next(cx) {
            Poll::Ready(Some(RunnerStreamEvent::Item(item))) => {
                Poll::Ready(Some(LifecycleStreamEvent::Event(item)))
            }
            Poll::Ready(Some(RunnerStreamEvent::Closed(runner_id))) => {
                this.map.remove(&runner_id);
                Poll::Ready(Some(LifecycleStreamEvent::Closed(runner_id)))
            }
            Poll::Ready(None) | Poll::Pending => Poll::Pending,
        }
    }
}
