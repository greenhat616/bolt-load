//! The status contract of a task: the states follow the status graph, the
//! terminal status comes last and only after teardown, and a slow reader
//! never stalls the transfer.

use std::time::Duration;

use bolt_load_tests::adapter::{
    fault::Gate,
    simple::{SimpleTestAdapter, SimpleTestAdapterBuilder},
};
use pretty_assertions::assert_eq;
use smol_cancellation_token::CancellationToken;
use tempfile::TempDir;

use crate::{
    adapter::{AdapterError, AnyBytesStream, BoltLoadAdapter, BoltLoadAdapterMeta},
    task::{
        DownloadError, DownloadMode, TaskState,
        test_support::{assert_transitions, record, spawn, tmp_path, watchdog},
    },
};

const SIZE: usize = 256 * 1024;
const MODES: [DownloadMode; 2] = [DownloadMode::Singleton, DownloadMode::Concurrent];

fn adapter_builder() -> SimpleTestAdapterBuilder {
    SimpleTestAdapterBuilder::new()
        .content_size(SIZE)
        .support_range(true)
}

#[tokio::test(flavor = "multi_thread")]
async fn test_status_sequence_success() {
    for mode in MODES {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("sequence.bin");
        let token = CancellationToken::new();
        let task = spawn(mode, adapter_builder().build().unwrap(), &dest, &token);

        let statuses = record(task.status()).await;
        let output = watchdog(task).await.unwrap();

        assert_transitions(&statuses);
        let last = statuses.last().unwrap();
        assert_eq!(last.state, TaskState::Finished, "{mode:?}");
        assert_eq!(last.progress.total, Some(SIZE as u64), "{mode:?}");
        assert_eq!(last.progress.downloaded, SIZE as u64, "{mode:?}");
        let info = last.info.as_ref().unwrap();
        assert_eq!(info.mode, mode, "{mode:?}");
        assert_eq!(info.path, dest, "{mode:?}");
        assert_eq!(output.path, dest, "{mode:?}");
        assert_eq!(output.mode, mode, "{mode:?}");
        assert_eq!(output.progress, last.progress, "{mode:?}");
        if let Some(first) = statuses.iter().find(|s| s.state == TaskState::Downloading) {
            // The entry snapshot, unless it was coalesced with a sample.
            assert!(first.speed.download >= 0.0, "{mode:?}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cancel_during_metadata_ends_cancelled() {
    for mode in MODES {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("meta.bin");
        let gate = Gate::new();
        let adapter = adapter_builder().meta_gate(gate.clone()).build().unwrap();
        let token = CancellationToken::new();
        let task = spawn(mode, adapter.clone(), &dest, &token);
        let rx = task.status();

        watchdog(gate.entered()).await;
        assert_eq!(task.snapshot().state, TaskState::Initializing, "{mode:?}");
        token.cancel();
        let result = watchdog(task).await;

        assert!(
            matches!(result, Err(DownloadError::Cancelled)),
            "{result:?}"
        );
        let statuses = record(rx).await;
        assert_transitions(&statuses);
        let last = statuses.last().unwrap();
        assert_eq!(last.state, TaskState::Cancelled, "{mode:?}");
        assert!(last.info.is_none(), "{mode:?}: initialization never ended");
        assert_eq!(adapter.active_requests(), 0, "{mode:?}");
        assert!(!dest.exists() && !tmp_path(&dest).exists(), "{mode:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cancel_wins_over_ready_metadata() {
    for mode in MODES {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("meta.bin");
        let adapter = adapter_builder().build().unwrap();
        let token = CancellationToken::new();
        token.cancel();
        let task = spawn(mode, adapter.clone(), &dest, &token);

        let rx = task.status();
        let result = watchdog(task).await;

        assert!(
            matches!(result, Err(DownloadError::Cancelled)),
            "{result:?}"
        );
        assert_eq!(rx.borrow().state, TaskState::Cancelled, "{mode:?}");
        assert_eq!(adapter.call_count(), 0, "{mode:?}: no request may be sent");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_finished_status_observes_committed_file() {
    for mode in MODES {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("committed.bin");
        let token = CancellationToken::new();
        let task = spawn(mode, adapter_builder().build().unwrap(), &dest, &token);

        let mut rx = task.status();
        let terminal = watchdog(rx.wait_for(|s| s.is_terminal())).await.unwrap();
        let observed = (dest.exists(), tmp_path(&dest).exists());
        watchdog(task).await.unwrap();

        assert_eq!(terminal.state, TaskState::Finished, "{mode:?}");
        assert_eq!(
            observed,
            (true, false),
            "{mode:?}: Finished must be published after the file is committed"
        );
    }
}

/// Reports an unknown size (`content_size == 0`) while streaming the content.
#[derive(Clone)]
struct UnknownSizeAdapter(SimpleTestAdapter);

impl BoltLoadAdapter for UnknownSizeAdapter {
    async fn is_range_stream_available(&self) -> bool {
        true
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

/// An unknown size selects the singleton mode even when ranges are
/// available, and the final total is the downloaded size.
#[tokio::test(flavor = "multi_thread")]
async fn test_unknown_total_downloads_in_singleton_mode() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("unknown.bin");
    let token = CancellationToken::new();
    let adapter = UnknownSizeAdapter(adapter_builder().build().unwrap());
    let task = spawn(DownloadMode::Concurrent, adapter, &dest, &token);

    let statuses = record(task.status()).await;
    let output = watchdog(task).await.unwrap();

    assert_transitions(&statuses);
    for status in &statuses {
        if status.state == TaskState::Downloading {
            assert_eq!(status.progress.total, None, "total stays unknown");
        }
    }
    assert_eq!(output.mode, DownloadMode::Singleton);
    assert_eq!(output.progress.total, Some(SIZE as u64));
    assert_eq!(output.progress.downloaded, SIZE as u64);
    assert_eq!(output.progress.downloaded_chunks, vec![0..SIZE as u64]);
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
    let task = spawn(DownloadMode::Singleton, slow_adapter(), &dest, &token);

    let result = watchdog(task).await;
    assert!(result.is_ok(), "{result:?}");
}

/// A reader that does not read while the transfer runs does not slow it,
/// and then reads the newest status (S5).
#[tokio::test(flavor = "multi_thread")]
async fn test_slow_reader_sees_the_newest_status() {
    for mode in MODES {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("lossy.bin");
        let token = CancellationToken::new();
        let task = spawn(mode, slow_adapter(), &dest, &token);
        let mut rx = task.status();

        // Nobody reads while the transfer runs; it must still complete.
        watchdog(async {
            while !dest.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        watchdog(task).await.unwrap();

        assert!(rx.has_changed(), "{mode:?}");
        assert_eq!(
            rx.borrow_and_update().state,
            TaskState::Finished,
            "{mode:?}"
        );
        assert!(
            watchdog(rx.changed()).await.is_err(),
            "{mode:?}: the samples were coalesced into the terminal status"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_periodic_progress_preserves_snapshot_order() {
    for mode in MODES {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("order.bin");
        let token = CancellationToken::new();
        let task = spawn(mode, slow_adapter(), &dest, &token);

        let statuses = record(task.status()).await;
        watchdog(task).await.unwrap();

        assert_transitions(&statuses);
        let downloaded: Vec<u64> = statuses
            .iter()
            .filter(|s| matches!(s.state, TaskState::Downloading | TaskState::Finished))
            .map(|s| s.progress.downloaded)
            .collect();
        assert!(
            downloaded.len() > 2,
            "{mode:?}: expected periodic samples, got {downloaded:?}"
        );
        assert!(
            downloaded.is_sorted(),
            "{mode:?}: progress went backwards: {downloaded:?}"
        );
        assert!(
            statuses
                .iter()
                .any(|s| s.state == TaskState::Downloading && s.speed.download > 0.0),
            "{mode:?}: samples carry a speed"
        );
    }
}

/// The terminal status is published even when nobody observes the task.
#[tokio::test(flavor = "multi_thread")]
async fn test_terminal_status_without_receivers() {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("unobserved.bin");
    let token = CancellationToken::new();
    let task = spawn(
        DownloadMode::Concurrent,
        adapter_builder().build().unwrap(),
        &dest,
        &token,
    );
    let output = watchdog(task).await.unwrap();
    assert_eq!(output.progress.downloaded, SIZE as u64);
}
