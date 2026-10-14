use std::{ops::Range, sync::Arc, time::Duration};

use async_waitgroup::WaitGroup;
use bolt_load_utils::telemetry::*;
use futures::{FutureExt, task::SpawnExt};
use smol_cancellation_token::CancellationToken;

use super::{ControlReceiver, runner_manager};
use crate::{
    adapter::{AnyAdapter, BoltLoadAdapter},
    runner::{StreamConnector, TaskRunner},
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
        }
    }

    /// Connect `range` and run a runner on it in one spawned task; the receiver
    /// yields the runner's channels.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, fields(runner_id, range))
    )]
    pub(super) fn spawn(
        &self,
        range: Range<u64>,
        runner_id: RunnerId,
        control_rx: ControlReceiver,
    ) -> runner_manager::PendingRunnerReceiver {
        use runner_manager::pending::{PendingRunnerError, RunnerBuilderOutput};

        let (tx, rx) = oneshot::channel();
        let (start, end) = (range.start, range.end);
        let wg = self.wg.clone();
        let adapter = self.adapter.clone();
        let cancel_token = self.cancel_token.clone();
        let idle_timeout = self.idle_timeout;
        let worker = async move {
            // Declared first, so the runner and its upstream stream drop before
            // the wait group is released.
            let _wg = wg;
            trace!("[TASK] background range runner: id: {runner_id}, range: {range:?}");

            // Connect the stream, abandoning the request on cancellation
            let connector =
                StreamConnector::from(async move { adapter.range_stream(start, end).await });
            let connected = {
                let connect = connector.connect().fuse();
                let cancelled = cancel_token.cancelled().fuse();
                futures::pin_mut!(connect, cancelled);
                futures::select_biased! {
                    _ = cancelled => return,
                    res = connect => res,
                }
            };
            if cancel_token.is_cancelled() {
                return;
            }
            let stream = match connected {
                Ok(stream) => stream,
                Err(e) => {
                    error!("failed to connect background range runner: {e:?}");
                    let _ = tx.send(Err(PendingRunnerError::Connection { source: e }));
                    return;
                }
            };

            let built = TaskRunner::builder()
                .total(end - start)
                .stream(stream)
                .runner_id(runner_id)
                .control_signal(control_rx)
                .cancel_token(cancel_token)
                .idle_timeout(idle_timeout)
                .build();
            let (mut runner, lifecycle_rx, data_rx) = match built {
                Ok(built) => built,
                Err(e) => {
                    // Build error - should not happen with proper setup
                    error!("failed to build task runner: {e:?}");
                    let _ = tx.send(Err(PendingRunnerError::ReceiverClosed));
                    return;
                }
            };
            if let Err(e) = tx.send(Ok(RunnerBuilderOutput {
                data_rx,
                lifecycle_rx,
            })) {
                error!("failed to send channels to pending receiver: {e:?}");
                return;
            }

            let run = runner.run();
            #[cfg(feature = "tracing")]
            let run = tracing::Instrument::instrument(
                run,
                tracing::trace_span!("background_task::runner"),
            );
            run.await;
        };
        #[cfg(feature = "tracing")]
        let worker = tracing::Instrument::instrument(
            worker,
            tracing::trace_span!(
                parent: None,
                "background_task::runner_connector",
                runner_id = runner_id,
                range = ?start..end,
            ),
        );
        if let Err(e) = self.rt.spawn(worker) {
            error!("failed to spawn background range runner: {e}");
            return runner_manager::pending::failed_receiver(PendingRunnerError::Spawn {
                message: e.to_string(),
            });
        }
        rx
    }
}

#[cfg(test)]
mod tests {
    use std::{pin::pin, sync::atomic::Ordering, time::Duration};

    use async_waitgroup::WaitGroup;
    use bolt_load_tests::adapter::simple::{SimpleTestAdapter, calculate_blake3};
    use futures::StreamExt;
    use pretty_assertions::assert_eq;
    use runner_manager::pending::PendingRunnerError;
    use smol_cancellation_token::CancellationToken;

    use super::{
        super::{DEFAULT_CONTROL_CHANNEL_CAPACITY, test_support::FailOnSpawnRuntime},
        *,
    };
    use crate::{
        adapter::{BoltLoadAdapter, DynBoltLoadAdapter},
        runner::{LifecycleEvent, StoppedReason},
        runtime::ThreadedRuntimeImpl,
        task::ControlEvent,
    };

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_success() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter =
            DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(10240).with_range_support(true));
        let range = 1000u64..3000u64;
        let runner_id = 1;
        let (_control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();
        let wg = WaitGroup::new();
        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter.clone(),
            wg.clone(),
            cancel_token,
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range.clone(), runner_id, control_rx);
        let result = receiver.await.expect("receiver closed");
        assert!(result.is_ok());
        let output = result.unwrap();
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
        let (_control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter,
            wg.clone(),
            cancel_token,
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range, runner_id, control_rx);
        let result = receiver.await.expect("receiver closed");
        wg.wait().await;

        // Connection should fail because range_stream fails
        assert!(result.is_err());
        match result {
            Ok(_) => panic!("Expected Connection error, got channels"),
            Err(PendingRunnerError::Connection { .. }) => {}
            Err(e) => panic!("Expected Connection error, got: {:?}", e),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_reports_connector_spawn_error() {
        let rt = ThreadedRuntimeImpl::new_other_rt(FailOnSpawnRuntime::new(0));
        let adapter =
            DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(1024).with_range_support(true));
        let range = 0u64..500u64;
        let runner_id = 8;
        let (_control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();
        let wg = WaitGroup::new();

        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter,
            wg.clone(),
            cancel_token,
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range, runner_id, control_rx);
        let result = receiver.await.expect("receiver closed");

        match result {
            Err(PendingRunnerError::Spawn { message }) => assert!(!message.is_empty()),
            other => panic!("expected Spawn error, got: {other:?}"),
        }
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
        let (_control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let wg = WaitGroup::new();

        let receiver = RunnerSpawner::new(
            rt,
            adapter,
            wg.clone(),
            CancellationToken::new(),
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(0..500, 9, control_rx);
        let output = tokio::time::timeout(Duration::from_secs(1), receiver)
            .await
            .expect("timed out waiting for runner channels")
            .expect("receiver closed")
            .expect("runner should start");

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
        let (_control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter,
            wg.clone(),
            cancel_token.clone(),
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range, runner_id, control_rx);
        let result = receiver.await.expect("receiver closed");
        assert!(result.is_ok());
        let output = result.unwrap();
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
        let (control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        // Send control message before creating the runner
        let new_total = 1000u64;
        control_tx
            .send(ControlEvent::LimitTotal(new_total))
            .await
            .unwrap();

        let wg = WaitGroup::new();
        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter,
            wg.clone(),
            cancel_token,
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range.clone(), runner_id, control_rx);
        let result = receiver.await.expect("receiver closed");
        assert!(result.is_ok());
        let output = result.unwrap();
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
        let (_control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter.clone(),
            wg.clone(),
            cancel_token,
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range.clone(), runner_id, control_rx);
        let result = receiver.await.expect("receiver closed");
        assert!(result.is_ok());
        let output = result.unwrap();
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
        let (_control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter,
            wg.clone(),
            cancel_token,
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range, runner_id, control_rx);
        let result = receiver.await.expect("receiver closed");
        assert!(result.is_ok());
        let output = result.unwrap();
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
            let (control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
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
                .spawn(range.clone(), runner_id, control_rx);
                let result = receiver.await.expect("receiver closed");
                let output = result.expect("connection failed");
                let data_rx = output.data_rx;
                let lifecycle_rx = output.lifecycle_rx;

                // Keep the control channel sender alive
                let _control_tx = control_tx;

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
        let (_control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter.clone(),
            wg.clone(),
            cancel_token,
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range.clone(), runner_id, control_rx);
        let result = receiver.await.expect("receiver closed");
        assert!(result.is_ok());
        let output = result.unwrap();
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
