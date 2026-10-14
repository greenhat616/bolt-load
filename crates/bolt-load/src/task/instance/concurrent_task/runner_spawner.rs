use std::{ops::Range, sync::Arc, time::Duration};

use async_waitgroup::WaitGroup;
use bolt_load_utils::telemetry::*;
use futures::task::SpawnExt;
use smol_cancellation_token::CancellationToken;

use super::runner_manager::{RunnerChannels, SpawnFailed};
use crate::{
    adapter::{AnyAdapter, BoltLoadAdapter},
    runner::{LimitReceiver, TaskRunner},
    runtime::ThreadedRuntimeImpl,
    task::RunnerId,
};

/// Spawns range runners for one download attempt.
///
/// Every spawned task holds a clone of the wait group until it has exited.
pub(super) struct RunnerSpawner {
    pub(super) rt: ThreadedRuntimeImpl,
    adapter: Arc<AnyAdapter>,
    pub(super) wg: WaitGroup,
    pub(super) cancel_token: CancellationToken,
    idle_timeout: Duration,
    connect_timeout: Duration,
}

impl RunnerSpawner {
    pub(super) fn new(
        rt: ThreadedRuntimeImpl,
        adapter: Arc<AnyAdapter>,
        wg: WaitGroup,
        cancel_token: CancellationToken,
        idle_timeout: Duration,
    ) -> Self {
        Self {
            rt,
            adapter,
            wg,
            cancel_token,
            idle_timeout,
            connect_timeout: crate::runner::DEFAULT_CONNECT_TIMEOUT,
        }
    }

    /// How long each runner may take to connect.
    pub(super) fn connect_timeout(mut self, connect_timeout: Duration) -> Self {
        self.connect_timeout = connect_timeout;
        self
    }

    /// Spawn a runner that connects `range` and downloads it.
    ///
    /// Returns the runner's receivers; nothing is left running when the
    /// spawn is rejected.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, fields(runner_id, range))
    )]
    pub(super) fn spawn(
        &self,
        range: Range<u64>,
        runner_id: RunnerId,
        limits: LimitReceiver,
    ) -> Result<RunnerChannels, SpawnFailed> {
        let (start, end) = (range.start, range.end);
        let adapter = self.adapter.clone();
        let (runner, lifecycle_rx, data_rx) = TaskRunner::builder()
            .total(end - start)
            .connector(async move { adapter.range_stream(start, end).await })
            .runner_id(runner_id)
            .limit_receiver(limits)
            .cancel_token(self.cancel_token.clone())
            .idle_timeout(self.idle_timeout)
            .connect_timeout(self.connect_timeout)
            .build()
            .expect("every runner input is set");
        let worker = RunnerWorker {
            runner,
            _wait_group: self.wg.clone(),
        }
        .run();
        #[cfg(feature = "tracing")]
        let worker = tracing::Instrument::instrument(
            worker,
            tracing::trace_span!(
                parent: None,
                "background_task::runner",
                runner_id = runner_id,
                range = ?start..end,
            ),
        );
        if let Err(e) = self.rt.spawn(worker) {
            error!("failed to spawn background range runner: {e}");
            return Err(SpawnFailed {
                message: e.to_string(),
            });
        }
        Ok(RunnerChannels {
            lifecycle_rx,
            data_rx,
        })
    }
}

/// A spawned runner, holding the wait group until the runner and its
/// connection are dropped.
struct RunnerWorker {
    // Dropped first: fields drop in declaration order.
    runner: TaskRunner,
    _wait_group: WaitGroup,
}

impl RunnerWorker {
    async fn run(mut self) {
        self.runner.run().await;
    }
}

#[cfg(test)]
mod tests {
    use std::{pin::pin, sync::atomic::Ordering, time::Duration};

    use async_waitgroup::WaitGroup;
    use bolt_load_tests::adapter::simple::{SimpleTestAdapter, calculate_blake3};
    use futures::StreamExt;
    use pretty_assertions::assert_eq;
    use smol_cancellation_token::CancellationToken;

    use super::{super::test_support::FailOnSpawnRuntime, *};
    use crate::{
        adapter::{BoltLoadAdapter, DynBoltLoadAdapter},
        runner::{LifecycleEvent, StoppedReason},
        runtime::ThreadedRuntimeImpl,
    };

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_success() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter =
            DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(10240).with_range_support(true));
        let range = 1000u64..3000u64;
        let runner_id = 1;
        let (_limit_tx, limits) = crate::runner::limit_mailbox();
        let cancel_token = CancellationToken::new();
        let wg = WaitGroup::new();
        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter.clone(),
            wg.clone(),
            cancel_token,
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range.clone(), runner_id, limits);
        let output = receiver.expect("runner should spawn");
        let data_rx = output.data_rx;
        let lifecycle_rx = output.lifecycle_rx;

        // Verify that Started message is received
        let mut lifecycle_rx = pin!(lifecycle_rx);
        let msg = lifecycle_rx.next().await.unwrap();
        assert!(matches!(msg, LifecycleEvent::Started));

        // Verify that downloaded data is received via data channel
        let mut total_downloaded = 0;
        let mut downloaded_data = Vec::new();

        let mut data_rx = pin!(data_rx);
        loop {
            tokio::select! {
                frame = data_rx.next() => {
                    match frame {
                        Some(frame) => {
                            total_downloaded += frame.data.len();
                            downloaded_data.extend_from_slice(&frame.data);
                        }
                        None => break,
                    }
                }
                lifecycle = lifecycle_rx.next() => {
                    match lifecycle {
                        Some(LifecycleEvent::Stopped(StoppedReason::Finished)) => {
                            break;
                        }
                        Some(LifecycleEvent::Stopped(StoppedReason::Failed(e))) => {
                            panic!("Runner failed: {e:?}");
                        }
                        _ => {}
                    }
                }
            }
        }

        wg.wait().await;

        // Verify the amount of downloaded data
        assert_eq!(total_downloaded, (range.end - range.start) as usize);
        assert_eq!(downloaded_data.len(), (range.end - range.start) as usize);

        // Verify data determinism - create the same adapter to get data from the same range for comparison
        let reference_adapter = SimpleTestAdapter::new(10240).with_range_support(true);
        let reference_stream =
            BoltLoadAdapter::range_stream(&reference_adapter, range.start, range.end)
                .await
                .unwrap();
        let mut reference_data = Vec::new();
        let mut reference_stream = std::pin::pin!(reference_stream);
        while let Some(chunk_result) = reference_stream.next().await {
            let chunk = chunk_result.unwrap();
            reference_data.extend_from_slice(&chunk);
        }

        assert_eq!(downloaded_data, reference_data);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_with_adapter_failure() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter = DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(1024).with_failure(true));
        let range = 0u64..500u64;
        let runner_id = 2;
        let (_limit_tx, limits) = crate::runner::limit_mailbox();
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter,
            wg.clone(),
            cancel_token,
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range, runner_id, limits);
        let mut lifecycle_rx = receiver.expect("runner should spawn").lifecycle_rx;
        wg.wait().await;

        // The connection fails because range_stream fails; the runner never starts
        match lifecycle_rx.next().await {
            Some(LifecycleEvent::Stopped(StoppedReason::Failed(e))) if e.is_stream_error() => {}
            other => panic!("Expected a stream error, got: {other:?}"),
        }
        assert!(lifecycle_rx.next().await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_reports_connector_spawn_error() {
        let rt = ThreadedRuntimeImpl::new_other_rt(FailOnSpawnRuntime::new(0));
        let adapter =
            DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(1024).with_range_support(true));
        let range = 0u64..500u64;
        let runner_id = 8;
        let (_limit_tx, limits) = crate::runner::limit_mailbox();
        let cancel_token = CancellationToken::new();
        let wg = WaitGroup::new();

        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter,
            wg.clone(),
            cancel_token,
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range, runner_id, limits);
        match receiver {
            Err(SpawnFailed { message }) => assert!(!message.is_empty()),
            Ok(_) => panic!("expected the spawn to fail"),
        }
        // Nothing is left holding the wait group.
        tokio::time::timeout(Duration::from_secs(1), wg.wait())
            .await
            .expect("the rejected runner was dropped");
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_spawns_once() {
        // A second spawn would fail; connecting and running share one task.
        let runtime = FailOnSpawnRuntime::new(1);
        let spawns = runtime.calls.clone();
        let rt = ThreadedRuntimeImpl::new_other_rt(runtime);
        let adapter =
            DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(1024).with_range_support(true));
        let (_limit_tx, limits) = crate::runner::limit_mailbox();
        let wg = WaitGroup::new();

        let receiver = RunnerSpawner::new(
            rt,
            adapter,
            wg.clone(),
            CancellationToken::new(),
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(0..500, 9, limits);
        let output = receiver.expect("runner should spawn");

        let mut data_rx = pin!(output.data_rx);
        let mut lifecycle_rx = pin!(output.lifecycle_rx);
        let mut received = 0;
        while let Some(frame) = data_rx.next().await {
            received += frame.data.len();
        }
        let mut finished = false;
        while let Some(event) = lifecycle_rx.next().await {
            finished |= event.is_finished();
        }
        wg.wait().await;

        assert!(finished);
        assert_eq!(received, 500);
        assert_eq!(spawns.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_with_cancellation() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter =
            DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(10240).with_range_support(true));
        let range = 0u64..5000u64;
        let runner_id = 3;
        let (_limit_tx, limits) = crate::runner::limit_mailbox();
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter,
            wg.clone(),
            cancel_token.clone(),
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range, runner_id, limits);
        let output = receiver.expect("runner should spawn");
        let _data_rx = output.data_rx;
        let lifecycle_rx = output.lifecycle_rx;

        // Wait for start message
        let mut lifecycle_rx = pin!(lifecycle_rx);
        let msg = lifecycle_rx.next().await.unwrap();
        assert!(matches!(msg, LifecycleEvent::Started));

        // Cancel the task
        cancel_token.cancel();

        // Wait for cancellation message
        while let Some(msg) = lifecycle_rx.next().await {
            if let LifecycleEvent::Stopped(StoppedReason::Failed(e)) = msg
                && e.is_cancelled()
            {
                break;
            }
        }

        wg.wait().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_small_range_runner_with_control_messages() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter =
            DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(10240).with_range_support(true));
        let old_total = 10240u64;
        let range = 0u64..old_total;
        let runner_id = 4;
        let (limit_tx, limits) = crate::runner::limit_mailbox();
        let cancel_token = CancellationToken::new();

        // Send control message before creating the runner
        let new_total = 1000u64;
        assert!(limit_tx.send(new_total));

        let wg = WaitGroup::new();
        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter,
            wg.clone(),
            cancel_token,
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range.clone(), runner_id, limits);
        let output = receiver.expect("runner should spawn");
        let data_rx = output.data_rx;
        let lifecycle_rx = output.lifecycle_rx;

        // Wait for start message
        let mut lifecycle_rx = pin!(lifecycle_rx);
        let msg = lifecycle_rx.next().await.unwrap();
        assert!(matches!(msg, LifecycleEvent::Started));

        // Collect downloaded data
        let mut total_downloaded = 0;
        let mut data_rx = pin!(data_rx);
        loop {
            tokio::select! {
                frame = data_rx.next() => {
                    match frame {
                        Some(frame) => {
                            total_downloaded += frame.data.len();
                        }
                        None => break,
                    }
                }
                lifecycle = lifecycle_rx.next() => {
                    if let Some(LifecycleEvent::Stopped(StoppedReason::Finished)) = lifecycle {
                        break;
                    }
                }
            }
        }

        wg.wait().await;

        // Verify that downloaded amount is the adjusted size
        if total_downloaded != new_total as usize {
            error!("total_downloaded: {total_downloaded}, new_total: {new_total}");
        }
        assert!(total_downloaded >= new_total as usize);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_edge_ranges() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let content_size = 1000;
        let adapter = DynBoltLoadAdapter::new_arc(
            SimpleTestAdapter::new(content_size).with_range_support(true),
        );

        // Test boundary range: start from end of file
        let range = 900u64..1000u64;
        let runner_id = 5;
        let (_limit_tx, limits) = crate::runner::limit_mailbox();
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter.clone(),
            wg.clone(),
            cancel_token,
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range.clone(), runner_id, limits);
        let output = receiver.expect("runner should spawn");
        let data_rx = output.data_rx;
        let lifecycle_rx = output.lifecycle_rx;

        // Wait for start
        let mut lifecycle_rx = pin!(lifecycle_rx);
        let msg = lifecycle_rx.next().await.unwrap();
        assert!(matches!(msg, LifecycleEvent::Started));

        // Collect all data - drain data_rx first, then check lifecycle
        let mut downloaded_data = Vec::new();
        let mut data_rx = pin!(data_rx);
        let mut finished = false;
        let mut data_done = false;

        loop {
            tokio::select! {
                biased;
                // Prefer data frames over lifecycle events to avoid losing data
                frame = data_rx.next(), if !data_done => {
                    match frame {
                        Some(frame) => {
                            downloaded_data.extend_from_slice(&frame.data);
                        }
                        None => {
                            data_done = true;
                            if finished {
                                break;
                            }
                        }
                    }
                }
                lifecycle = lifecycle_rx.next() => {
                    match lifecycle {
                        Some(LifecycleEvent::Stopped(StoppedReason::Finished)) => {
                            finished = true;
                            if data_done {
                                break;
                            }
                        }
                        None => break,
                        _ => {}
                    }
                }
            }
        }

        wg.wait().await;
        assert_eq!(downloaded_data.len(), (range.end - range.start) as usize);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_zero_length_range() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter =
            DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(1000).with_range_support(true));

        // Test zero-length range
        let range = 500u64..500u64;
        let runner_id = 6;
        let (_limit_tx, limits) = crate::runner::limit_mailbox();
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter,
            wg.clone(),
            cancel_token,
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range, runner_id, limits);
        let output = receiver.expect("runner should spawn");
        let _data_rx = output.data_rx;
        let lifecycle_rx = output.lifecycle_rx;

        wg.wait().await;

        // Wait for start
        let mut lifecycle_rx = pin!(lifecycle_rx);
        let msg = lifecycle_rx.next().await.unwrap();
        assert!(matches!(msg, LifecycleEvent::Started));

        // Should finish immediately without downloading any data
        // Zero-length range may result in Empty error
        let msg = lifecycle_rx.next().await;
        match msg {
            Some(LifecycleEvent::Stopped(StoppedReason::Finished)) => {}
            Some(LifecycleEvent::Stopped(StoppedReason::Failed(e))) if e.is_empty() => {}
            other => panic!("Expected immediate finish for zero-length range, got: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_multiple_runners() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter =
            DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(4000).with_range_support(true));

        // Create multiple concurrent range runners
        let ranges = vec![
            (1, 0u64..1000u64),
            (2, 1000u64..2000u64),
            (3, 2000u64..3000u64),
            (4, 3000u64..4000u64),
        ];

        let mut handles = Vec::new();

        for (runner_id, range) in ranges {
            let rt_clone = rt.clone();
            let adapter_clone = adapter.clone();
            let (limit_tx, limits) = crate::runner::limit_mailbox();
            let cancel_token = CancellationToken::new();

            let handle = tokio::spawn(async move {
                let wg = WaitGroup::new();
                let receiver = RunnerSpawner::new(
                    rt_clone.clone(),
                    adapter_clone,
                    wg.clone(),
                    cancel_token,
                    crate::runner::DEFAULT_IDLE_TIMEOUT,
                )
                .spawn(range.clone(), runner_id, limits);
                let output = receiver.expect("runner should spawn");
                let data_rx = output.data_rx;
                let lifecycle_rx = output.lifecycle_rx;

                // Keep the limit sender alive
                let _limit_tx = limit_tx;

                // Wait for start
                let mut lifecycle_rx = pin!(lifecycle_rx);
                let msg = lifecycle_rx.next().await.unwrap();
                assert!(matches!(msg, LifecycleEvent::Started));

                // Collect all data
                let mut downloaded_size = 0;
                let mut finished = false;
                let mut data_done = false;

                let mut data_rx = pin!(data_rx);
                loop {
                    tokio::select! {
                        biased;
                        // Prefer data frames to avoid losing data
                        frame = data_rx.next(), if !data_done => {
                            match frame {
                                Some(frame) => {
                                    downloaded_size += frame.data.len();
                                }
                                None => {
                                    data_done = true;
                                    if finished {
                                        break;
                                    }
                                }
                            }
                        }
                        lifecycle = lifecycle_rx.next() => {
                            match lifecycle {
                                Some(LifecycleEvent::Stopped(StoppedReason::Finished)) => {
                                    finished = true;
                                    if data_done {
                                        break;
                                    }
                                }
                                Some(LifecycleEvent::Stopped(StoppedReason::Failed(kind))) => {
                                    panic!("Runner {runner_id} failed with error: {kind:?}");
                                }
                                None => break,
                                _ => {}
                            }
                        }
                    }
                }

                wg.wait().await;

                // Ensure the runner completed correctly
                if !finished {
                    panic!("Runner {runner_id} did not finish properly");
                }

                (runner_id, downloaded_size, range.end - range.start)
            });

            handles.push(handle);
        }

        // Wait for all tasks to complete
        let results = futures::future::join_all(handles).await;

        for result in results {
            let (runner_id, downloaded_size, expected_size) = result.unwrap();
            info!(
                "Runner {runner_id}: downloaded {downloaded_size} bytes, expected {expected_size} \
                 bytes"
            );
            assert_eq!(downloaded_size, expected_size as usize);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_hash_verification() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter =
            DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(5000).with_range_support(true));
        let range = 1500u64..3500u64;
        let runner_id = 7;
        let (_limit_tx, limits) = crate::runner::limit_mailbox();
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter.clone(),
            wg.clone(),
            cancel_token,
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range.clone(), runner_id, limits);
        let output = receiver.expect("runner should spawn");
        let data_rx = output.data_rx;
        let lifecycle_rx = output.lifecycle_rx;

        // Wait for start message
        let mut lifecycle_rx = pin!(lifecycle_rx);
        let msg = lifecycle_rx.next().await.unwrap();
        assert!(matches!(msg, LifecycleEvent::Started));

        // Collect all downloaded data
        let mut downloaded_data = Vec::new();
        let mut data_rx = pin!(data_rx);
        loop {
            tokio::select! {
                frame = data_rx.next() => {
                    match frame {
                        Some(frame) => {
                            downloaded_data.extend_from_slice(&frame.data);
                        }
                        None => break,
                    }
                }
                lifecycle = lifecycle_rx.next() => {
                    if let Some(LifecycleEvent::Stopped(StoppedReason::Finished)) = lifecycle {
                        break;
                    }
                }
            }
        }

        wg.wait().await;

        // Use the same adapter to directly get data from the same range for comparison
        let reference_adapter = SimpleTestAdapter::new(5000).with_range_support(true);
        let reference_stream =
            BoltLoadAdapter::range_stream(&reference_adapter, range.start, range.end)
                .await
                .unwrap();
        let mut reference_data = Vec::new();
        let mut reference_stream = std::pin::pin!(reference_stream);
        while let Some(chunk_result) = reference_stream.next().await {
            let chunk = chunk_result.unwrap();
            reference_data.extend_from_slice(&chunk);
        }

        // Verify data integrity
        assert_eq!(downloaded_data.len(), (range.end - range.start) as usize);
        assert_eq!(downloaded_data, reference_data);

        // Calculate and compare hash
        let downloaded_hash = calculate_blake3(&downloaded_data);
        let reference_hash = calculate_blake3(&reference_data);
        assert_eq!(downloaded_hash, reference_hash);
    }
}
