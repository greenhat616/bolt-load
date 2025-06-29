use std::{
    collections::HashMap,
    pin::Pin,
    task::{Context, Poll},
};

use async_channel::Receiver;
use futures::{Stream, StreamExt, stream::BoxStream};
use futures_concurrency::stream::{StreamGroup, stream_group::Key};
use pin_project_lite::pin_project;

use crate::{runner::RunnerMessage, task::RunnerId};

pin_project! {
    pub struct RunnerNotification {
        #[pin]
        group: StreamGroup<BoxStream<'static, RunnerMessage>>,
        map: HashMap<RunnerId, Key>,
    }
}

impl RunnerNotification {
    pub fn new() -> Self {
        Self {
            group: StreamGroup::new(),
            map: HashMap::new(),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            group: StreamGroup::with_capacity(capacity),
            map: HashMap::with_capacity(capacity),
        }
    }

    pub fn add(&mut self, runner_id: RunnerId, receiver: Receiver<RunnerMessage>) {
        let key = self.group.insert(receiver.boxed());
        self.map.insert(runner_id, key);
    }

    pub fn remove(&mut self, runner_id: RunnerId) {
        if let Some(key) = self.map.remove(&runner_id) {
            self.group.remove(key);
        }
    }
}

impl Stream for RunnerNotification {
    type Item = RunnerMessage;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        this.group.poll_next(cx)
    }
}
