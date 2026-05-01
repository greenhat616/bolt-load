use std::{
    collections::HashMap,
    pin::Pin,
    task::{Context, Poll},
};

use futures::{Stream, task::AtomicWaker};
use futures_concurrency::stream::{StreamGroup, stream_group::Key};
use pin_project_lite::pin_project;

use super::stream::{RunnerTaggedStream, RunnerTaggedStreamItem};
use crate::{
    runner::{DataFrame, DataFrameReceiver},
    task::RunnerId,
};

pin_project! {
    /// Aggregates data from multiple runners into a single stream.
    ///
    /// Unlike `LifecycleAggregator`, data receivers are registered dynamically
    /// when the `Ready` lifecycle event is received. Each data item is tagged
    /// with its source runner ID for proper attribution.
    ///
    /// This supports:
    /// - Dynamic registration of data receivers (called when Ready event arrives)
    /// - Proper cleanup when receivers close
    /// - Tracking runner ID for each data item
    pub struct DataAggregator {
        #[pin]
        group: StreamGroup<RunnerTaggedStream<DataFrameReceiver>>,
        map: HashMap<RunnerId, Key>,
        is_closed: bool,
        waker: AtomicWaker,
    }
}

impl Default for DataAggregator {
    fn default() -> Self {
        Self::new()
    }
}

impl DataAggregator {
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

    /// Register a data receiver for a runner.
    /// This is typically called when a `Ready` lifecycle event is received.
    pub fn register(&mut self, runner_id: RunnerId, receiver: DataFrameReceiver) {
        let tagged_stream = RunnerTaggedStream::new(runner_id, receiver);
        let key = self.group.insert(tagged_stream);
        self.map.insert(runner_id, key);
        self.waker.wake();
    }

    /// Unregister a data receiver for a runner.
    /// This is typically called when a `Stopped` lifecycle event is received.
    pub fn unregister(&mut self, runner_id: RunnerId) {
        if let Some(key) = self.map.remove(&runner_id) {
            self.group.remove(key);
        }
    }

    /// Check if a runner is registered.
    pub fn contains(&self, runner_id: RunnerId) -> bool {
        self.map.contains_key(&runner_id)
    }

    /// Check if the aggregator is closed.
    pub fn is_closed(&self) -> bool {
        self.is_closed
    }

    /// Close the aggregator, signaling no more data will be received.
    pub fn close(&mut self) {
        self.is_closed = true;
        self.waker.wake();
    }

    /// Reopen the aggregator for receiving data.
    pub fn reopen(&mut self) {
        self.is_closed = false;
        self.waker.wake();
    }

    /// Get the number of runners registered.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Check if there are no runners registered.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl Stream for DataAggregator {
    type Item = RunnerTaggedStreamItem<DataFrame>;

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

#[cfg(test)]
mod tests {
    use async_ringbuf::{AsyncHeapRb, traits::*};
    use bytes::Bytes;

    use super::*;
    use crate::runner::{DATA_FRAME_CHANNEL_CAPACITY, DataFrame, DataFrameSender};

    fn create_data_channel() -> (DataFrameSender, DataFrameReceiver) {
        AsyncHeapRb::<DataFrame>::new(DATA_FRAME_CHANNEL_CAPACITY).split()
    }

    #[tokio::test]
    async fn test_data_aggregator_basic() {
        let mut aggregator = DataAggregator::new();
        let (mut tx, rx) = create_data_channel();
        let runner_id = 1;

        aggregator.register(runner_id, rx);
        assert_eq!(aggregator.len(), 1);
        assert!(aggregator.contains(runner_id));

        // Send data
        let data = Bytes::from("test data");
        tx.push(DataFrame {
            data: data.clone(),
            start_nanos: Some(12345),
        })
        .await
        .unwrap();

        // Poll for the data
        use futures::StreamExt;
        let item = aggregator.next().await.unwrap();
        assert_eq!(item.runner_id, runner_id);
        assert_eq!(item.item.data, data);
        assert_eq!(item.item.start_nanos, Some(12345));
    }

    #[tokio::test]
    async fn test_data_aggregator_multiple_runners() {
        let mut aggregator = DataAggregator::new();

        let (mut tx1, rx1) = create_data_channel();
        let (mut tx2, rx2) = create_data_channel();

        aggregator.register(1, rx1);
        aggregator.register(2, rx2);
        assert_eq!(aggregator.len(), 2);

        // Send data from both runners
        tx1.push(DataFrame {
            data: Bytes::from("data1"),
            start_nanos: None,
        })
        .await
        .unwrap();
        tx2.push(DataFrame {
            data: Bytes::from("data2"),
            start_nanos: None,
        })
        .await
        .unwrap();

        use futures::StreamExt;
        let item1 = aggregator.next().await.unwrap();
        let item2 = aggregator.next().await.unwrap();

        // Both items should be received (order may vary)
        let runner_ids: Vec<_> = [&item1, &item2].iter().map(|i| i.runner_id).collect();
        assert!(runner_ids.contains(&1));
        assert!(runner_ids.contains(&2));
    }

    #[tokio::test]
    async fn test_data_aggregator_unregister() {
        let mut aggregator = DataAggregator::new();
        let (mut tx, rx) = create_data_channel();

        aggregator.register(1, rx);
        assert!(aggregator.contains(1));

        aggregator.unregister(1);
        assert!(!aggregator.contains(1));
        assert!(aggregator.is_empty());

        // Sender should now be disconnected
        let result = tx
            .push(DataFrame {
                data: Bytes::from("test"),
                start_nanos: None,
            })
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_data_aggregator_dynamic_registration() {
        let mut aggregator = DataAggregator::new();

        // Start with no runners
        assert!(aggregator.is_empty());

        // Dynamically register runners (simulating Ready events)
        let (mut tx1, rx1) = create_data_channel();
        aggregator.register(1, rx1);

        let (_tx2, rx2) = create_data_channel();
        aggregator.register(2, rx2);

        assert_eq!(aggregator.len(), 2);

        // Send and receive data
        tx1.push(DataFrame {
            data: Bytes::from("runner1"),
            start_nanos: None,
        })
        .await
        .unwrap();

        use futures::StreamExt;
        let item = aggregator.next().await.unwrap();
        assert_eq!(item.runner_id, 1);

        // Unregister runner 1 (simulating Stopped event)
        aggregator.unregister(1);
        assert_eq!(aggregator.len(), 1);
        assert!(!aggregator.contains(1));
        assert!(aggregator.contains(2));
    }

    #[tokio::test]
    async fn test_data_aggregator_sender_close() {
        let mut aggregator = DataAggregator::new();
        let (mut tx, rx) = create_data_channel();

        aggregator.register(1, rx);

        // Send some data then close
        tx.push(DataFrame {
            data: Bytes::from("data"),
            start_nanos: None,
        })
        .await
        .unwrap();
        drop(tx);

        use futures::StreamExt;

        // Should receive the data
        let item = aggregator.next().await.unwrap();
        assert_eq!(item.runner_id, 1);

        // After sender closes, stream returns None for that runner
        // The aggregator should handle this gracefully
    }
}
