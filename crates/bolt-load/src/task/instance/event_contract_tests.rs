//! Characterization tests for the event contract of task instances: the order
//! of [`TaskEvent`]s and what is observable when the terminal event arrives.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use bolt_load_tests::adapter::{
    fault::Gate,
    simple::{SimpleTestAdapter, SimpleTestAdapterBuilder},
};
use smol_cancellation_token::CancellationToken;
use tempfile::TempDir;

use super::{TaskEvent, TaskInstance, TaskInstanceError, TaskInstanceImpl};
use crate::{
    DOWNLOADING_TMP_EXTENSION,
    adapter::{AdapterError, AnyBytesStream, BoltLoadAdapter, BoltLoadAdapterMeta},
    runner::TaskError,
    runtime::ThreadedRuntimeImpl,
    task::DownloadMode,
};

const WATCHDOG: Duration = Duration::from_secs(30);
const SIZE: usize = 256 * 1024;
const MODES: [DownloadMode; 2] = [DownloadMode::Singleton, DownloadMode::Concurrent];

async fn watchdog<F: Future>(fut: F) -> F::Output {
    tokio::time::timeout(WATCHDOG, fut)
        .await
        .expect("watchdog: operation did not finish in time")
}

fn tmp_path(dest: &Path) -> PathBuf {
    let mut path = dest.as_os_str().to_os_string();
    path.push(DOWNLOADING_TMP_EXTENSION);
    PathBuf::from(path)
}

fn adapter_builder() -> SimpleTestAdapterBuilder {
    SimpleTestAdapterBuilder::new()
        .content_size(SIZE)
        .support_range(true)
}

fn start(
    mode: DownloadMode,
    adapter: impl BoltLoadAdapter + 'static,
    dest: &Path,
    token: &CancellationToken,
) -> (TaskInstanceImpl, async_channel::Receiver<TaskEvent>) {
    start_with_channel(mode, adapter, dest, token, async_channel::unbounded())
}

fn start_with_channel(
    mode: DownloadMode,
    adapter: impl BoltLoadAdapter + 'static,
    dest: &Path,
    token: &CancellationToken,
    (event_tx, event_rx): (
        async_channel::Sender<TaskEvent>,
        async_channel::Receiver<TaskEvent>,
    ),
) -> (TaskInstanceImpl, async_channel::Receiver<TaskEvent>) {
    let mut task = TaskInstanceImpl::new(mode, ThreadedRuntimeImpl::new_tokio_rt(), None);
    task.run(
        Arc::new(Box::new(adapter)),
        dest.to_path_buf(),
        event_tx,
        token.clone(),
    )
    .expect("task should start");
    (task, event_rx)
}

/// Collect every event until all senders are gone.
async fn collect(event_rx: async_channel::Receiver<TaskEvent>) -> Vec<TaskEvent> {
    watchdog(async {
        let mut events = Vec::new();
        while let Ok(event) = event_rx.recv().await {
            events.push(event);
        }
        events
    })
    .await
}

fn is_terminal(event: &TaskEvent) -> bool {
    matches!(event, TaskEvent::Finished(_) | TaskEvent::Failed(_))
}

fn assert_cancelled(event: &TaskEvent) {
    match event {
        TaskEvent::Failed(TaskInstanceError::Failed { source }) => {
            assert!(matches!(**source, TaskError::Cancelled), "got {source:?}")
        }
        other => panic!("expected Failed(Cancelled), got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_task_event_sequence_success() {
    for mode in MODES {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("sequence.bin");
        let token = CancellationToken::new();
        let (mut task, event_rx) = start(mode, adapter_builder().build().unwrap(), &dest, &token);

        let events = collect(event_rx).await;
        watchdog(task.wait()).await.unwrap();

        assert!(
            matches!(events.first(), Some(TaskEvent::Initializing)),
            "{mode:?}: {events:?}"
        );
        match events.get(1) {
            Some(TaskEvent::Downloading(progress)) => {
                assert_eq!(progress.speed(), 0.0, "{mode:?}");
                assert_eq!(progress.write_speed(), 0.0, "{mode:?}");
            }
            other => panic!("{mode:?}: expected the entry snapshot, got {other:?}"),
        }
        match events.last() {
            Some(TaskEvent::Finished(progress)) => {
                assert_eq!(progress.total, Some(SIZE as u64), "{mode:?}");
                assert_eq!(progress.downloaded, SIZE as u64, "{mode:?}");
            }
            other => panic!("{mode:?}: expected Finished last, got {other:?}"),
        }
        assert_eq!(
            events.iter().filter(|e| is_terminal(e)).count(),
            1,
            "{mode:?}: exactly one terminal event"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cancel_during_instance_metadata_emits_failed_cancelled() {
    for mode in MODES {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("meta.bin");
        let gate = Gate::new();
        let adapter = adapter_builder().meta_gate(gate.clone()).build().unwrap();
        let token = CancellationToken::new();
        let (mut task, event_rx) = start(mode, adapter.clone(), &dest, &token);

        watchdog(gate.entered()).await;
        token.cancel();
        let events = collect(event_rx).await;
        watchdog(task.wait()).await.unwrap();

        assert_eq!(events.len(), 2, "{mode:?}: {events:?}");
        assert!(matches!(events[0], TaskEvent::Initializing), "{mode:?}");
        assert_cancelled(&events[1]);
        assert_eq!(adapter.active_requests(), 0, "{mode:?}");
        assert!(!dest.exists() && !tmp_path(&dest).exists(), "{mode:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cancel_wins_over_ready_instance_metadata() {
    for mode in MODES {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("meta.bin");
        let adapter = adapter_builder().build().unwrap();
        let token = CancellationToken::new();
        token.cancel();
        let (mut task, event_rx) = start(mode, adapter.clone(), &dest, &token);

        let events = collect(event_rx).await;
        watchdog(task.wait()).await.unwrap();

        assert_eq!(events.len(), 2, "{mode:?}: {events:?}");
        assert!(matches!(events[0], TaskEvent::Initializing), "{mode:?}");
        assert_cancelled(&events[1]);
        assert_eq!(adapter.call_count(), 0, "{mode:?}: no request may be sent");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_finished_event_observes_committed_file() {
    for mode in MODES {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("committed.bin");
        let token = CancellationToken::new();
        let (mut task, event_rx) = start(mode, adapter_builder().build().unwrap(), &dest, &token);

        let observed = watchdog(async {
            while let Ok(event) = event_rx.recv().await {
                if let TaskEvent::Finished(_) = event {
                    return Some((dest.exists(), tmp_path(&dest).exists()));
                }
            }
            None
        })
        .await;
        watchdog(task.wait()).await.unwrap();

        assert_eq!(
            observed,
            Some((true, false)),
            "{mode:?}: Finished must be emitted after the file is published"
        );
    }
}

/// Reports an unknown size (`content_size == 0`) while streaming the content.
#[derive(Clone)]
struct UnknownSizeAdapter(SimpleTestAdapter);

#[async_trait::async_trait]
impl BoltLoadAdapter for UnknownSizeAdapter {
    async fn is_range_stream_available(&self) -> bool {
        false
    }

    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError> {
        let meta = self.0.retrieve_meta().await?;
        Ok(BoltLoadAdapterMeta {
            content_size: 0,
            ..meta
        })
    }

    async fn full_stream(&self) -> Result<AnyBytesStream, AdapterError> {
        self.0.full_stream().await
    }

    async fn range_stream(&self, start: u64, end: u64) -> Result<AnyBytesStream, AdapterError> {
        self.0.range_stream(start, end).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_singleton_unknown_total_final_progress() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("unknown.bin");
    let token = CancellationToken::new();
    let adapter = UnknownSizeAdapter(adapter_builder().support_range(false).build().unwrap());
    let (mut task, event_rx) = start(DownloadMode::Singleton, adapter, &dest, &token);

    let events = collect(event_rx).await;
    watchdog(task.wait()).await.unwrap();

    for event in &events {
        if let TaskEvent::Downloading(progress) = event {
            assert_eq!(
                progress.total(),
                None,
                "total stays unknown while downloading"
            );
        }
    }
    match events.last() {
        Some(TaskEvent::Finished(progress)) => {
            assert_eq!(progress.total, Some(SIZE as u64));
            assert_eq!(progress.downloaded, SIZE as u64);
            assert_eq!(progress.downloaded_chunks, vec![0..SIZE as u64]);
        }
        other => panic!("expected Finished last, got {other:?}"),
    }
    assert_eq!(std::fs::read(&dest).unwrap().len(), SIZE);
}

/// Throttled so that the transfer spans several progress samples.
fn slow_adapter() -> SimpleTestAdapter {
    adapter_builder()
        .max_speed(SIZE as u64 / 2)
        .build()
        .unwrap()
}

// The runner fails with `ChannelClosed` as soon as its control channel closes,
// so a transfer spanning several samples proves the sender outlives it.
#[tokio::test(flavor = "multi_thread")]
async fn test_singleton_control_sender_lives_until_runner_exit() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("control.bin");
    let token = CancellationToken::new();
    let (mut task, event_rx) = start(DownloadMode::Singleton, slow_adapter(), &dest, &token);

    let events = collect(event_rx).await;
    watchdog(task.wait()).await.unwrap();

    assert!(
        matches!(events.last(), Some(TaskEvent::Finished(_))),
        "{events:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_periodic_progress_full_channel_is_lossy_and_nonblocking() {
    for mode in MODES {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("lossy.bin");
        let token = CancellationToken::new();
        let (mut task, event_rx) = start_with_channel(
            mode,
            slow_adapter(),
            &dest,
            &token,
            async_channel::bounded(1),
        );

        assert!(matches!(
            watchdog(event_rx.recv()).await,
            Ok(TaskEvent::Initializing)
        ));
        assert!(matches!(
            watchdog(event_rx.recv()).await,
            Ok(TaskEvent::Downloading(_))
        ));
        // Nobody reads while the transfer runs; it must still complete.
        watchdog(async {
            while !dest.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;

        let events = collect(event_rx).await;
        watchdog(task.wait()).await.unwrap();
        assert!(
            matches!(events.last(), Some(TaskEvent::Finished(_))),
            "{mode:?}: {events:?}"
        );
        assert!(
            events.len() <= 2,
            "{mode:?}: samples beyond the channel capacity must be dropped: {events:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_periodic_progress_preserves_snapshot_order() {
    for mode in MODES {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("order.bin");
        let token = CancellationToken::new();
        let (mut task, event_rx) = start(mode, slow_adapter(), &dest, &token);

        let events = collect(event_rx).await;
        watchdog(task.wait()).await.unwrap();

        let downloaded: Vec<u64> = events
            .iter()
            .filter_map(|event| match event {
                TaskEvent::Downloading(progress) => Some(progress.downloaded()),
                TaskEvent::Finished(progress) => Some(progress.downloaded),
                _ => None,
            })
            .collect();
        assert!(
            downloaded.len() > 2,
            "{mode:?}: expected periodic samples, got {events:?}"
        );
        assert!(
            downloaded.is_sorted(),
            "{mode:?}: progress went backwards: {downloaded:?}"
        );
    }
}
