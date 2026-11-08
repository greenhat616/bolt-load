use std::{
    collections::HashMap,
    pin::Pin,
    task::{Context, Poll},
};

use bolt_load_utils::telemetry::*;
use futures::{Stream, task::AtomicWaker};
use futures_concurrency::stream::{StreamGroup, stream_group::Key};
use pin_project_lite::pin_project;

use super::{
    AttemptFailure, ChunkState, RunnerFailureKind, RunnerManager, RunnerTick, SideFailed,
    SideFailure, SpawnFailed, StopReason, charges_range, is_attempt_retryable, is_restart,
    stream::{RunnerStreamEvent, RunnerTaggedStream, RunnerTaggedStreamItem},
};
use crate::{
    runner::{LifecycleEvent, LifecycleReceiver, StoppedReason, TaskError},
    task::RunnerId,
};

pin_project! {
    /// Aggregates lifecycle events from multiple runners into a single stream.
    ///
    /// Each runner has its own lifecycle receiver that emits `Started` once it
    /// is connected and `Stopped` when it ends. This aggregator multiplexes all
    /// of them into a single stream for the RunnerManager to consume.
    pub struct LifecycleAggregator {
        #[pin]
        group: StreamGroup<RunnerTaggedStream<LifecycleReceiver>>,
        map: HashMap<RunnerId, Key>,
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

    /// Get the number of runners being tracked.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Check if there are no runners being tracked.
    #[cfg(test)]
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

        // Never ends: a runner may be added at any time.
        if this.map.is_empty() {
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
            Poll::Ready(None) | Poll::Pending => Poll::Pending,
        }
    }
}

impl RunnerManager {
    /// Poll the next runner lifecycle event; a rejected spawn comes first.
    #[inline]
    pub(super) fn poll_lifecycle(&mut self, cx: &mut Context<'_>) -> Poll<RunnerTick> {
        if let Some(tick) = self.take_spawn_failure() {
            return Poll::Ready(tick);
        }
        Pin::new(&mut self.lifecycle_aggregator)
            .poll_next(cx)
            .map(|event| {
                let event = event.expect("the lifecycle aggregator never ends");
                self.handle_lifecycle_stream_event(event)
            })
    }

    /// Handle `runner_id` stopping for `reason`, as its lifecycle channel
    /// would report it: a test drives the loop's own handling with it.
    #[cfg(test)]
    pub(in super::super) fn report_stopped(
        &mut self,
        runner_id: RunnerId,
        reason: StoppedReason,
    ) -> RunnerTick {
        self.handle_lifecycle(RunnerTaggedStreamItem {
            runner_id,
            item: LifecycleEvent::Stopped(reason),
        })
    }

    /// Handle `runner_id` reporting that it connected, as its lifecycle
    /// channel would report it.
    #[cfg(test)]
    pub(in super::super) fn report_started(&mut self, runner_id: RunnerId) -> RunnerTick {
        self.handle_lifecycle(RunnerTaggedStreamItem {
            runner_id,
            item: LifecycleEvent::Started,
        })
    }

    /// Handle `runner_id` failing with `error`, see [`Self::report_stopped`].
    #[cfg(test)]
    pub(in super::super) fn fail_runner(
        &mut self,
        runner_id: RunnerId,
        error: TaskError,
    ) -> RunnerTick {
        self.report_stopped(runner_id, StoppedReason::Failed(error))
    }

    fn handle_lifecycle(&mut self, event: RunnerTaggedStreamItem<LifecycleEvent>) -> RunnerTick {
        let runner_id = event.runner_id;
        // A beaten side is no part of any chunk any more (design §9.6).
        if self.stop_reason(runner_id) == Some(StopReason::Loser) {
            return match event.item {
                LifecycleEvent::Started => {
                    trace!("runner {runner_id} started after it was beaten");
                    RunnerTick::downloading()
                }
                LifecycleEvent::Stopped(_) => self.handle_loser_ended(runner_id),
            };
        }
        let connecting = self.is_connecting(runner_id);
        let reason = match event.item {
            LifecycleEvent::Started => {
                trace!("runner {runner_id} started");
                debug_assert!(connecting, "runner {runner_id} started twice");
                if self.chunk_planner.partner_chunk(runner_id).is_some() {
                    self.chunk_planner
                        .mark_partner_running(runner_id)
                        .expect("a hedge runner has a pair");
                } else {
                    self.chunk_planner
                        .mark_running(runner_id)
                        .expect("chunk planner should not fail");
                }
                let data_rx = self
                    .connecting_data
                    .remove(&runner_id)
                    .expect("a connecting runner has a data receiver");
                self.data_aggregator.register(runner_id, data_rx);
                return RunnerTick::downloading();
            }
            LifecycleEvent::Stopped(reason) => reason,
        };
        if self.has_other_side(runner_id) {
            let (error, retryable) = match reason {
                // A side completes only after its frames were taken, and
                // the first one decides the pair.
                StoppedReason::Finished | StoppedReason::Failed(TaskError::ExceededTotalSize) => {
                    debug_assert!(false, "runner {runner_id} finished undecided");
                    (TaskError::ChannelClosed, true)
                }
                StoppedReason::Failed(error) => {
                    let retryable = is_attempt_retryable(&error);
                    (error, retryable)
                }
            };
            return self.handle_side_failed(runner_id, connecting, error, retryable);
        }
        let stopped_slow = self.stop_reason(runner_id) == Some(StopReason::Slow);
        self.meters.remove(&runner_id);
        // Remove control channel when runner stops
        self.release_runner(runner_id);
        if connecting {
            let error = match reason {
                StoppedReason::Failed(error) => error,
                StoppedReason::Finished => {
                    debug_assert!(false, "runner {runner_id} finished before starting");
                    TaskError::ChannelClosed
                }
            };
            return self.handle_connect_failed(runner_id, error);
        }
        // The survivor of an undecided pair completes only after its
        // frames were taken, and the first one decides the pair.
        let undecided = self
            .chunk_planner
            .get_runner_state(runner_id)
            .is_some_and(ChunkState::is_undecided);
        let kind = match reason {
            StoppedReason::Finished | StoppedReason::Failed(TaskError::ExceededTotalSize)
                if undecided =>
            {
                debug_assert!(false, "runner {runner_id} finished undecided");
                TaskError::ChannelClosed
            }
            StoppedReason::Finished | StoppedReason::Failed(TaskError::ExceededTotalSize) => {
                trace!("runner {runner_id} finished");
                return if self.mark_runner_completed(runner_id) {
                    RunnerTick::finished()
                } else {
                    RunnerTick::downloading()
                };
            }
            StoppedReason::Failed(kind) => kind,
        };
        if stopped_slow && kind.is_cancelled() {
            return self.handle_stopped_slow(runner_id);
        }
        error!("runner {runner_id} failed: {kind:?}");
        if kind.is_cancelled() {
            let released = self
                .chunk_planner
                .mark_failed(runner_id)
                .expect("chunk planner should not fail");
            trace!(
                "runner {runner_id} cancelled, released range: {:?}",
                released.range
            );
            return RunnerTick::downloading();
        }
        self.runner_outcome_sampler
            .record_stream_closed(if kind.is_retryable() {
                RunnerFailureKind::Retryable
            } else {
                RunnerFailureKind::Unretryable
            });
        let retryable = is_attempt_retryable(&kind);
        RunnerTick::with_failure(self.fail_attempt(runner_id, kind, retryable, false))
    }

    /// Release the chunk of `runner_id`, which ended its attempt with
    /// `error`, and describe the failure; `slow` when the loop stopped it.
    ///
    /// The runner's own bytes classify it: of a hedged attempt, those of
    /// its side only, and the side that failed first counts too (design
    /// §9.6).
    fn fail_attempt(
        &mut self,
        runner_id: RunnerId,
        error: TaskError,
        retryable: bool,
        slow: bool,
    ) -> AttemptFailure {
        let own = self.chunk_planner.owner_bytes(runner_id);
        let first = self
            .chunk_planner
            .get_runner_state(runner_id)
            .and_then(|state| state.hedge.as_ref())
            .and_then(|hedge| hedge.first);
        let released = self
            .chunk_planner
            .mark_failed(runner_id)
            .expect("chunk planner should not fail");
        trace!(
            "runner {runner_id} failed, released range: {:?}",
            released.range
        );
        let own = own.unwrap_or(released.received);
        if slow {
            AttemptFailure::slow_of_attempt(runner_id, released, own, first)
        } else {
            AttemptFailure::of_attempt(runner_id, error, retryable, released, own, first)
        }
    }

    /// Whether `runner_id` is a side of an undecided pair whose other side
    /// still runs (design §9.6).
    fn has_other_side(&self, runner_id: RunnerId) -> bool {
        self.chunk_planner.partner_chunk(runner_id).is_some()
            || self
                .chunk_planner
                .get_runner_state(runner_id)
                .is_some_and(|state| state.partner().is_some())
    }

    /// A side of an undecided pair failed with `error` while the other side
    /// runs on (design §9.6): the other side keeps the chunk, nothing is
    /// released or backed off, and the pair's budget waits for the other
    /// side. The side's slot is reclaimed.
    fn handle_side_failed(
        &mut self,
        runner_id: RunnerId,
        connecting: bool,
        error: TaskError,
        retryable: bool,
    ) -> RunnerTick {
        // The original side delivered every byte accepted so far.
        let own = self.chunk_planner.owner_bytes(runner_id).unwrap_or(0);
        let survivor = if self.chunk_planner.partner_chunk(runner_id).is_some() {
            self.chunk_planner.drop_partner(runner_id)
        } else {
            self.chunk_planner.hand_over(runner_id)
        }
        .expect("a side of an undecided pair has a pair");
        self.meters.remove(&runner_id);
        self.release_runner(runner_id);
        if error.is_cancelled() {
            trace!("runner {runner_id} cancelled while hedged");
            self.chunk_planner
                .record_side_failure(survivor, SideFailure::default());
            return RunnerTick::downloading();
        }
        warn!("hedged runner {runner_id} failed, runner {survivor} runs on: {error:?}");
        self.chunk_planner.record_side_failure(
            survivor,
            SideFailure {
                restart: retryable && is_restart(&error, own),
                charged: charges_range(&error, retryable, own),
            },
        );
        if connecting {
            self.runner_outcome_sampler
                .record_connect_failed(if retryable {
                    RunnerFailureKind::Retryable
                } else {
                    RunnerFailureKind::Unretryable
                });
        } else {
            self.runner_outcome_sampler
                .record_stream_closed(if error.is_retryable() {
                    RunnerFailureKind::Retryable
                } else {
                    RunnerFailureKind::Unretryable
                });
        }
        let attempts = self
            .chunk_planner
            .get_runner_state(survivor)
            .map_or(1, |state| state.attempts.saturating_add(1));
        RunnerTick::with_side_failure(SideFailed {
            runner_id,
            error,
            retryable,
            received: own,
            attempts,
        })
    }

    /// A beaten side confirmed it ended: only its slot is reclaimed, once
    /// (design §9.6). Nothing of it is reported, whatever it ended with.
    fn handle_loser_ended(&mut self, runner_id: RunnerId) -> RunnerTick {
        trace!("beaten runner {runner_id} ended");
        self.meters.remove(&runner_id);
        self.release_runner(runner_id);
        RunnerTick::downloading()
    }

    /// Handle a runner that stopped on [`RunnerManager::stop_runner`]: its
    /// range is released from the bytes the planner accepted, as an
    /// [`AttemptFailure::slow`].
    ///
    /// The bytes it delivered that were not accepted yet are dropped with
    /// its data receiver. A chunk the runner completed before it noticed
    /// the stop is finished: no failure, and nothing is released.
    ///
    /// The concurrency strategy does not count the stop as an outcome: the
    /// loop decided it, the server did not fail.
    fn handle_stopped_slow(&mut self, runner_id: RunnerId) -> RunnerTick {
        let complete = self
            .chunk_planner
            .get_runner_state(runner_id)
            .is_some_and(|state| state.is_complete());
        if complete {
            trace!("runner {runner_id} completed its chunk before it stopped");
            return if self.mark_runner_completed(runner_id) {
                RunnerTick::finished()
            } else {
                RunnerTick::downloading()
            };
        }
        let failure = self.fail_attempt(runner_id, TaskError::Timeout, true, true);
        debug!(
            "runner {runner_id} stopped for being slow, released range: {:?}",
            failure.range
        );
        RunnerTick::with_failure(failure)
    }

    /// Handle a runner that stopped before it connected; its range is
    /// released and it never counts as finished.
    fn handle_connect_failed(&mut self, runner_id: RunnerId, error: TaskError) -> RunnerTick {
        if error.is_cancelled() {
            self.chunk_planner
                .mark_failed(runner_id)
                .expect("chunk planner should not fail");
            trace!("runner {runner_id} cancelled while connecting");
            return RunnerTick::downloading();
        }
        error!("runner {runner_id} failed to connect: {error:?}");
        let retryable = is_attempt_retryable(&error);
        self.runner_outcome_sampler
            .record_connect_failed(if retryable {
                RunnerFailureKind::Retryable
            } else {
                RunnerFailureKind::Unretryable
            });
        RunnerTick::with_failure(self.fail_attempt(runner_id, error, retryable, false))
    }

    /// Handle a runner whose spawn was rejected.
    #[cold]
    fn handle_spawn_failed(&mut self, runner_id: RunnerId, failed: SpawnFailed) -> RunnerTick {
        error!("failed to spawn runner {runner_id}: {}", failed.message);
        let error = TaskError::Other {
            message: failed.message,
        };
        if self.stop_reason(runner_id) == Some(StopReason::Loser) {
            return self.handle_loser_ended(runner_id);
        }
        if self.has_other_side(runner_id) {
            // An unretryable connect failure, as for any runner (design
            // §9.6, M4).
            return self.handle_side_failed(runner_id, true, error, false);
        }
        self.release_runner(runner_id);
        self.handle_connect_failed(runner_id, error)
    }

    /// Handle a runner whose stream closed without sending `Stopped`.
    fn handle_runner_lost(&mut self, runner_id: RunnerId) -> RunnerTick {
        warn!("runner {runner_id} lifecycle stream closed unexpectedly");
        if self.stop_reason(runner_id) == Some(StopReason::Loser) {
            return self.handle_loser_ended(runner_id);
        }
        let connecting = self.is_connecting(runner_id);
        if self.has_other_side(runner_id) {
            return self.handle_side_failed(runner_id, connecting, TaskError::ChannelClosed, true);
        }
        self.meters.remove(&runner_id);
        if connecting {
            self.runner_outcome_sampler
                .record_connect_failed(RunnerFailureKind::Retryable);
        } else {
            self.runner_outcome_sampler
                .record_stream_closed(RunnerFailureKind::Retryable);
        }
        let failure = self.fail_attempt(runner_id, TaskError::ChannelClosed, true, false);
        self.release_runner(runner_id);
        trace!(
            "runner {runner_id} lost, released range: {:?}",
            failure.range
        );
        if self.chunk_planner.is_complete() {
            return RunnerTick::finished();
        }
        RunnerTick::with_failure(failure)
    }

    fn handle_lifecycle_stream_event(&mut self, event: LifecycleStreamEvent) -> RunnerTick {
        match event {
            LifecycleStreamEvent::Event(tagged) => self.handle_lifecycle(tagged),
            LifecycleStreamEvent::Closed(runner_id) => self.handle_runner_lost(runner_id),
        }
    }

    /// The next rejected spawn, which the lifecycle reports before anything else.
    fn take_spawn_failure(&mut self) -> Option<RunnerTick> {
        let (runner_id, failed) = self.spawn_failures.pop_front()?;
        Some(self.handle_spawn_failed(runner_id, failed))
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
    fn test_lifecycle_aggregator_never_ends() {
        let mut aggregator = LifecycleAggregator::new();
        let waker = Arc::new(CountWaker::default());
        let waker_ref = waker_ref(&waker);
        let mut cx = Context::from_waker(&waker_ref);

        assert!(Pin::new(&mut aggregator).poll_next(&mut cx).is_pending());
    }
}
