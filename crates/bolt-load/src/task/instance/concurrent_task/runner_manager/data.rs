use std::{
    collections::HashMap,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use async_ringbuf::traits::{Consumer, Observer};
use bolt_load_utils::telemetry::*;
use bytes::Bytes;
use futures::{Stream, task::AtomicWaker};
use futures_concurrency::stream::{StreamGroup, stream_group::Key};
use pin_project_lite::pin_project;

use super::{
    DownloadedChunk, RunnerManager, RunnerTick, StopReason,
    stream::{RunnerStreamEvent, RunnerTaggedStream, RunnerTaggedStreamItem},
};
use crate::{
    runner::{DataFrame, DataFrameReceiver},
    task::RunnerId,
};

/// A runner's data receiver, shared by the aggregator, which polls it, and
/// the arbitration of a hedged pair, which may take its queued frames first
/// (design §9.6).
#[derive(Clone)]
struct SharedReceiver(Arc<Mutex<DataFrameReceiver>>);

impl Stream for SharedReceiver {
    type Item = DataFrame;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut receiver = self.0.lock().expect("a data receiver is never poisoned");
        Pin::new(&mut *receiver).poll_next(cx)
    }
}

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
        group: StreamGroup<RunnerTaggedStream<SharedReceiver>>,
        map: HashMap<RunnerId, (Key, SharedReceiver)>,
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
        let receiver = SharedReceiver(Arc::new(Mutex::new(receiver)));
        let tagged_stream = RunnerTaggedStream::new(runner_id, receiver.clone());
        let key = self.group.insert(tagged_stream);
        self.map.insert(runner_id, (key, receiver));
        self.waker.wake();
    }

    /// Unregister a data receiver for a runner, dropping it with the frames
    /// it holds.
    /// This is typically called when a `Stopped` lifecycle event is received.
    pub fn unregister(&mut self, runner_id: RunnerId) {
        if let Some((key, _)) = self.map.remove(&runner_id) {
            self.group.remove(key);
        }
    }

    /// Take the frames `runner_id` queued up to its first non-empty one,
    /// which is returned; the empty ones before it are dropped. `None` when
    /// it has no non-empty frame queued, or no receiver registered.
    ///
    /// The frames left stay for the aggregator, in order.
    pub fn take_first_data(&mut self, runner_id: RunnerId) -> Option<Bytes> {
        let (_, receiver) = self.map.get(&runner_id)?;
        let mut receiver = receiver
            .0
            .lock()
            .expect("a data receiver is never poisoned");
        while let Some(frame) = receiver.try_pop() {
            if !frame.data.is_empty() {
                return Some(frame.data);
            }
        }
        None
    }

    /// Whether `runner_id` has frames queued that the aggregator has not
    /// taken yet; the frames stay where they are.
    pub fn has_queued(&self, runner_id: RunnerId) -> bool {
        self.map.get(&runner_id).is_some_and(|(_, receiver)| {
            !receiver
                .0
                .lock()
                .expect("a data receiver is never poisoned")
                .is_empty()
        })
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
                    DataStreamEvent::Data(item) => {
                        self.on_data_frame(item.runner_id, item.item.data)
                    }
                    DataStreamEvent::Closed(runner_id) => {
                        trace!("runner {runner_id} data stream closed");
                        self.data_aggregator.unregister(runner_id);
                        RunnerTick::downloading()
                    }
                },
            )
    }

    /// Handle a data frame of `runner_id`, deciding its hedged pair first
    /// when it is the first non-empty frame since the fork (design §9.6).
    ///
    /// The decision comes before the frame reaches the meters, the planner
    /// or the writer. The original side wins a tie: when the hedge's first
    /// frame is taken while the original, connected, still has a non-empty
    /// frame queued, that frame is taken and handled instead. The beaten
    /// side is stopped and its frames dropped; a frame of a beaten side is
    /// never accepted. An empty frame decides nothing.
    pub(in super::super) fn on_data_frame(
        &mut self,
        runner_id: RunnerId,
        data_frame: Bytes,
    ) -> RunnerTick {
        if self.stop_reason(runner_id) == Some(StopReason::Loser) {
            trace!("dropping a frame of beaten runner {runner_id}");
            return RunnerTick::downloading();
        }
        if let Some(chunk) = self.chunk_planner.partner_chunk(runner_id) {
            if data_frame.is_empty() {
                return RunnerTick::downloading();
            }
            let original = chunk.runner_id.expect("an undecided pair has an owner");
            // Only a connected original's frames are registered.
            if let Some(queued) = self.data_aggregator.take_first_data(original) {
                debug!("runner {original} wins its pair over hedge {runner_id}: a frame queued");
                self.chunk_planner
                    .drop_partner(runner_id)
                    .expect("the hedge is a partner");
                self.chunk_planner.decide(original);
                self.stop_loser(runner_id);
                return RunnerTick::with_chunk(self.handle_data_frame(original, queued));
            }
            debug!("hedge {runner_id} wins its pair over runner {original}");
            self.chunk_planner
                .hand_over(original)
                .expect("the original owns the pair");
            self.chunk_planner.decide(runner_id);
            self.stop_loser(original);
            return RunnerTick::with_chunk(self.handle_data_frame(runner_id, data_frame));
        }
        if !data_frame.is_empty()
            && let Some(state) = self.chunk_planner.get_runner_state(runner_id)
            && state.is_undecided()
        {
            if let Some(partner) = state.partner() {
                debug!(
                    "runner {runner_id} wins its pair over hedge {}",
                    partner.runner_id
                );
                self.chunk_planner
                    .drop_partner(partner.runner_id)
                    .expect("the hedge is a partner");
                self.stop_loser(partner.runner_id);
            }
            self.chunk_planner.decide(runner_id);
        }
        RunnerTick::with_chunk(self.handle_data_frame(runner_id, data_frame))
    }

    /// Handle data frames from runners, returning the chunk ready for writing.
    fn handle_data_frame(&mut self, runner_id: RunnerId, mut data_frame: Bytes) -> DownloadedChunk {
        let bytes_len = data_frame.len();
        *self.meters.entry(runner_id).or_insert(0) += bytes_len;

        // The progress comes before the write, which the loop dispatches
        // next: only `downloaded` runs ahead. The written bytes follow the
        // writer's completions (design §11.1).
        let fixed_downloaded_range = self
            .chunk_planner
            .update_progress(runner_id, bytes_len as u64)
            .expect("chunk planner should not fail");
        let picked_size = fixed_downloaded_range.end - fixed_downloaded_range.start;
        data_frame.truncate(picked_size as usize);

        DownloadedChunk {
            runner_id,
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

    /// Counts its wake-ups.
    #[derive(Default)]
    struct WakeCount(std::sync::atomic::AtomicUsize);

    impl WakeCount {
        fn get(&self) -> usize {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl futures::task::ArcWake for WakeCount {
        fn wake_by_ref(this: &std::sync::Arc<Self>) {
            this.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn frame(byte: u8) -> DataFrame {
        DataFrame {
            data: Bytes::from(vec![byte; 4]),
        }
    }

    /// The data of the next event, polled with `waker`.
    fn poll_data(aggregator: &mut DataAggregator, waker: &std::task::Waker) -> Poll<Option<Bytes>> {
        let mut cx = Context::from_waker(waker);
        Pin::new(aggregator)
            .poll_next(&mut cx)
            .map(|event| match event {
                Some(DataStreamEvent::Data(item)) => Some(item.item.data),
                other => panic!("expected data, got {other:?}"),
            })
    }

    /// The receiver shared with the arbitration (design §9.6) keeps its
    /// wake-ups and its order: frames taken from outside leave the rest in
    /// order; once the aggregator found it empty, a new frame wakes it; and
    /// a producer blocked on a full channel resumes once a frame is taken
    /// from outside.
    #[tokio::test]
    async fn test_frames_taken_by_the_arbitration_keep_wake_ups_and_order() {
        let (mut sender, receiver) = AsyncHeapRb::<DataFrame>::new(2).split();
        let mut aggregator = DataAggregator::new();
        aggregator.register(1, receiver);
        let consumer_wakes = std::sync::Arc::new(WakeCount::default());
        let consumer = futures::task::waker(consumer_wakes.clone());

        // Taken from outside: empty frames are skipped, the rest stays.
        sender.try_push(DataFrame { data: Bytes::new() }).unwrap();
        sender.try_push(frame(1)).unwrap();
        assert_eq!(aggregator.take_first_data(1), Some(Bytes::from(vec![1; 4])));
        assert_eq!(aggregator.take_first_data(1), None);
        assert!(poll_data(&mut aggregator, &consumer).is_pending());
        let seen = consumer_wakes.get();
        sender.try_push(frame(2)).unwrap();
        assert!(
            consumer_wakes.get() > seen,
            "a new frame wakes the aggregator"
        );
        assert_eq!(
            poll_data(&mut aggregator, &consumer),
            Poll::Ready(Some(Bytes::from(vec![2; 4])))
        );

        // A producer waits for room, which a frame taken from outside
        // makes.
        sender.try_push(frame(3)).unwrap();
        sender.try_push(frame(4)).unwrap();
        let producer_wakes = std::sync::Arc::new(WakeCount::default());
        let producer = futures::task::waker(producer_wakes.clone());
        let mut push = std::pin::pin!(sender.push(frame(5)));
        let mut cx = Context::from_waker(&producer);
        assert!(
            push.as_mut().poll(&mut cx).is_pending(),
            "the channel is full"
        );
        assert_eq!(aggregator.take_first_data(1), Some(Bytes::from(vec![3; 4])));
        assert!(producer_wakes.get() > 0, "the producer is woken");
        assert!(matches!(push.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));

        // The rest comes in order.
        for byte in [4, 5] {
            assert_eq!(
                poll_data(&mut aggregator, &consumer),
                Poll::Ready(Some(Bytes::from(vec![byte; 4])))
            );
        }
        assert!(poll_data(&mut aggregator, &consumer).is_pending());
    }
}
