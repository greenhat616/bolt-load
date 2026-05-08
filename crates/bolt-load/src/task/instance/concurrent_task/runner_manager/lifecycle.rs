use std::{
    collections::HashMap,
    pin::Pin,
    task::{Context, Poll},
};

use futures::{Stream, task::AtomicWaker};
use futures_concurrency::stream::{StreamGroup, stream_group::Key};
use pin_project_lite::pin_project;

use super::stream::{RunnerStreamEvent, RunnerTaggedStream, RunnerTaggedStreamItem};
use crate::{
    runner::{LifecycleEvent, LifecycleReceiver},
    task::RunnerId,
};

pin_project! {
    /// Aggregates lifecycle events from multiple runners into a single stream.
    ///
    /// Similar to `RunnerNotification` but specifically for the new lifecycle channel
    /// architecture. Each runner has its own lifecycle receiver that emits events
    /// (Connecting, Ready, Started, Stopped). This aggregator multiplexes all of
    /// them into a single stream for the RunnerManager to consume.
    pub struct LifecycleAggregator {
        #[pin]
        group: StreamGroup<RunnerTaggedStream<LifecycleReceiver>>,
        map: HashMap<RunnerId, Key>,
        is_closed: bool,
        waker: AtomicWaker,
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
            is_closed: false,
            waker: AtomicWaker::new(),
        }
    }

    /// Add a lifecycle receiver for a runner.
    pub fn add(&mut self, runner_id: RunnerId, receiver: LifecycleReceiver) {
        let key = self
            .group
            .insert(RunnerTaggedStream::new(runner_id, receiver));
        self.map.insert(runner_id, key);
        self.waker.wake();
    }

    /// Remove a lifecycle receiver for a runner.
    pub fn remove(&mut self, runner_id: RunnerId) {
        if let Some(key) = self.map.remove(&runner_id) {
            self.group.remove(key);
        }
    }

    /// Check if the aggregator is closed.
    pub fn is_closed(&self) -> bool {
        self.is_closed
    }

    /// Close the aggregator, signaling no more events will be received.
    pub fn close(&mut self) {
        self.is_closed = true;
        self.waker.wake();
    }

    /// Reopen the aggregator for receiving events.
    pub fn reopen(&mut self) {
        self.is_closed = false;
        self.waker.wake();
    }

    /// Get the number of runners being tracked.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Check if there are no runners being tracked.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

pub enum LifecycleStreamEvent {
    Event(RunnerTaggedStreamItem<LifecycleEvent>),
    Closed(RunnerId),
}

impl Stream for LifecycleAggregator {
    type Item = LifecycleStreamEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        this.waker.register(cx.waker());

        if this.map.is_empty() {
            if *this.is_closed {
                return Poll::Ready(None);
            }
            return Poll::Pending;
        }

        match this.group.poll_next(cx) {
            Poll::Ready(Some(event)) => match event {
                RunnerStreamEvent::Item(item) => {
                    Poll::Ready(Some(LifecycleStreamEvent::Event(item)))
                }
                RunnerStreamEvent::Closed(runner_id) => {
                    this.map.remove(&runner_id);
                    Poll::Ready(Some(LifecycleStreamEvent::Closed(runner_id)))
                }
            },
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

#[cfg(test)]
mod tests {
    use std::{
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll},
    };

    use async_ringbuf::{AsyncHeapRb, traits::*};
    use futures::task::{ArcWake, waker_ref};

    use super::*;
    use crate::runner::{LIFECYCLE_CHANNEL_CAPACITY, LifecycleReceiver, LifecycleSender};

    #[derive(Default)]
    struct CountWaker(AtomicUsize);

    impl ArcWake for CountWaker {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            arc_self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn create_lifecycle_channel() -> (LifecycleSender, LifecycleReceiver) {
        AsyncHeapRb::new(LIFECYCLE_CHANNEL_CAPACITY).split()
    }

    #[tokio::test]
    async fn test_lifecycle_aggregator_basic() {
        let mut aggregator = LifecycleAggregator::new();
        let (mut tx, rx) = create_lifecycle_channel();
        let runner_id = 1;

        aggregator.add(runner_id, rx);
        assert_eq!(aggregator.len(), 1);
        assert!(!aggregator.is_empty());

        // Send a lifecycle event
        tx.push(LifecycleEvent::Started).await.unwrap();

        // Poll for the event
        use futures::StreamExt;
        let event = aggregator.next().await;
        assert!(matches!(
            event,
            Some(LifecycleStreamEvent::Event(RunnerTaggedStreamItem {
                runner_id: id,
                item: LifecycleEvent::Started
            })) if id == runner_id
        ));
    }

    #[tokio::test]
    async fn test_lifecycle_aggregator_multiple_runners() {
        let mut aggregator = LifecycleAggregator::new();

        let (mut tx1, rx1) = create_lifecycle_channel();
        let (mut tx2, rx2) = create_lifecycle_channel();

        aggregator.add(1, rx1);
        aggregator.add(2, rx2);
        assert_eq!(aggregator.len(), 2);

        // Send events from both runners
        tx1.push(LifecycleEvent::Started).await.unwrap();
        tx2.push(LifecycleEvent::Started).await.unwrap();

        // Both events should be received (order may vary)
        use futures::StreamExt;
        let event1 = aggregator.next().await.unwrap();
        let event2 = aggregator.next().await.unwrap();

        let runner_ids: Vec<_> = [&event1, &event2]
            .into_iter()
            .filter_map(|e| match e {
                LifecycleStreamEvent::Event(RunnerTaggedStreamItem {
                    runner_id: id,
                    item: LifecycleEvent::Started,
                }) => Some(*id),
                _ => None,
            })
            .collect();

        assert!(runner_ids.contains(&1));
        assert!(runner_ids.contains(&2));
    }

    #[tokio::test]
    async fn test_lifecycle_aggregator_remove() {
        let mut aggregator = LifecycleAggregator::new();
        let (mut tx, rx) = create_lifecycle_channel();

        aggregator.add(1, rx);
        assert_eq!(aggregator.len(), 1);

        aggregator.remove(1);
        assert_eq!(aggregator.len(), 0);
        assert!(aggregator.is_empty());

        // Sender should now be disconnected
        let result = tx.push(LifecycleEvent::Started).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_lifecycle_aggregator_add_wakes_pending_poller() {
        let mut aggregator = LifecycleAggregator::new();
        let waker = Arc::new(CountWaker::default());
        let waker_ref = waker_ref(&waker);
        let mut cx = Context::from_waker(&waker_ref);

        assert!(matches!(
            Pin::new(&mut aggregator).poll_next(&mut cx),
            Poll::Pending
        ));

        let (_tx, rx) = create_lifecycle_channel();
        aggregator.add(1, rx);

        assert_eq!(waker.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_lifecycle_aggregator_closed_empty_returns_none() {
        let mut aggregator = LifecycleAggregator::new();
        let waker = Arc::new(CountWaker::default());
        let waker_ref = waker_ref(&waker);
        let mut cx = Context::from_waker(&waker_ref);

        aggregator.close();

        assert!(matches!(
            Pin::new(&mut aggregator).poll_next(&mut cx),
            Poll::Ready(None)
        ));
    }
}
