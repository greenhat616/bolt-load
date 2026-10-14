use std::{
    collections::HashMap,
    pin::Pin,
    task::{Context, Poll},
};

use bolt_load_utils::telemetry::*;
use bytes::Bytes;
use futures::{Stream, task::AtomicWaker};
use futures_concurrency::stream::{StreamGroup, stream_group::Key};
use pin_project_lite::pin_project;

use super::{
    DownloadedChunk, RunnerManager, RunnerTick,
    stream::{RunnerStreamEvent, RunnerTaggedStream, RunnerTaggedStreamItem},
};
use crate::{
    runner::{DataFrame, DataFrameReceiver},
    task::RunnerId,
};

pin_project! {
    /// Aggregates data from multiple runners into a single stream.
    ///
    /// Unlike `LifecycleAggregator`, data receivers are registered dynamically
    /// when the runner starts. Each data item is tagged
    /// with its source runner ID for proper attribution.
    ///
    /// This supports:
    /// - Dynamic registration of data receivers (called when Started event arrives)
    /// - Proper cleanup when receivers close
    /// - Tracking runner ID for each data item
    pub struct DataAggregator {
        #[pin]
        group: StreamGroup<RunnerTaggedStream<DataFrameReceiver>>,
        map: HashMap<RunnerId, Key>,
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
            waker: AtomicWaker::new(),
        }
    }

    /// Register a data receiver for a runner.
    /// This is typically called when a `Started` lifecycle event is received.
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

    #[cfg(test)]
    /// Check if a runner is registered.
    pub fn contains(&self, runner_id: RunnerId) -> bool {
        self.map.contains_key(&runner_id)
    }

    #[cfg(test)]
    /// Get the number of runners registered.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[cfg(test)]
    /// Check if there are no runners registered.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[derive(Debug)]
pub enum DataStreamEvent {
    Data(RunnerTaggedStreamItem<DataFrame>),
    Closed(RunnerId),
}

impl Stream for DataAggregator {
    type Item = DataStreamEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        this.waker.register(cx.waker());

        // Never ends: a runner may be registered at any time.
        if this.map.is_empty() {
            return Poll::Pending;
        }

        match this.group.poll_next(cx) {
            Poll::Ready(Some(event)) => match event {
                RunnerStreamEvent::Item(item) => Poll::Ready(Some(DataStreamEvent::Data(item))),
                RunnerStreamEvent::Closed(runner_id) => {
                    this.map.remove(&runner_id);
                    Poll::Ready(Some(DataStreamEvent::Closed(runner_id)))
                }
            },
            Poll::Ready(None) | Poll::Pending => Poll::Pending,
        }
    }
}

impl RunnerManager {
    /// Poll the next data frame, returning the part of it the planner accepted.
    #[inline]
    pub(super) fn poll_data(&mut self, cx: &mut Context<'_>) -> Poll<RunnerTick> {
        Pin::new(&mut self.data_aggregator)
            .poll_next(cx)
            .map(
                |event| match event.expect("the data aggregator never ends") {
                    DataStreamEvent::Data(item) => RunnerTick::with_chunk(
                        self.handle_data_frame(item.runner_id, item.item.data),
                    ),
                    DataStreamEvent::Closed(runner_id) => {
                        trace!("runner {runner_id} data stream closed");
                        self.data_aggregator.unregister(runner_id);
                        RunnerTick::downloading()
                    }
                },
            )
    }

    /// Handle data frames from runners, returning the chunk ready for writing.
    fn handle_data_frame(&mut self, runner_id: RunnerId, mut data_frame: Bytes) -> DownloadedChunk {
        let bytes_len = data_frame.len();
        *self.meters.entry(runner_id).or_insert(0) += bytes_len;

        let fixed_downloaded_range = self
            .chunk_planner
            .update_progress(runner_id, bytes_len as u64)
            .expect("chunk planner should not fail");
        let picked_size = fixed_downloaded_range.end - fixed_downloaded_range.start;
        data_frame.truncate(picked_size as usize);

        DownloadedChunk {
            range: fixed_downloaded_range,
            bytes: data_frame,
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
        tx.push(DataFrame { data: data.clone() }).await.unwrap();

        // Poll for the data
        use futures::StreamExt;
        let event = aggregator.next().await.unwrap();
        match event {
            DataStreamEvent::Data(item) => {
                assert_eq!(item.runner_id, runner_id);
                assert_eq!(item.item.data, data);
            }
            other => panic!("expected Data event, got {other:?}"),
        }
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
        })
        .await
        .unwrap();
        tx2.push(DataFrame {
            data: Bytes::from("data2"),
        })
        .await
        .unwrap();

        use futures::StreamExt;
        let mut runner_ids = Vec::new();
        for _ in 0..2 {
            match aggregator.next().await.unwrap() {
                DataStreamEvent::Data(item) => runner_ids.push(item.runner_id),
                other => panic!("expected Data event, got {other:?}"),
            }
        }
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
            })
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_data_aggregator_dynamic_registration() {
        let mut aggregator = DataAggregator::new();

        // Start with no runners
        assert!(aggregator.is_empty());

        // Dynamically register runners (simulating Started events)
        let (mut tx1, rx1) = create_data_channel();
        aggregator.register(1, rx1);

        let (_tx2, rx2) = create_data_channel();
        aggregator.register(2, rx2);

        assert_eq!(aggregator.len(), 2);

        // Send and receive data
        tx1.push(DataFrame {
            data: Bytes::from("runner1"),
        })
        .await
        .unwrap();

        use futures::StreamExt;
        match aggregator.next().await.unwrap() {
            DataStreamEvent::Data(item) => assert_eq!(item.runner_id, 1),
            other => panic!("expected Data event, got {other:?}"),
        }

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
        })
        .await
        .unwrap();
        drop(tx);

        use futures::StreamExt;

        // Should receive the data
        match aggregator.next().await.unwrap() {
            DataStreamEvent::Data(item) => assert_eq!(item.runner_id, 1),
            other => panic!("expected Data event, got {other:?}"),
        }

        // After sender closes, stream emits Closed
        match aggregator.next().await.unwrap() {
            DataStreamEvent::Closed(id) => assert_eq!(id, 1),
            other => panic!("expected Closed event, got {other:?}"),
        }
    }
}
