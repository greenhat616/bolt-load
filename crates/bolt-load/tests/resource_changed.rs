//! A resource that changes during the download (design §11.2, D12, D14):
//! the task downloads it again from scratch once, with the metadata
//! retrieved again, and fails when it changes again. Every range carries
//! the validator of the task's own metadata. A server that ignores ranges
//! still takes the fallback to one connection.

#![cfg(all(feature = "reqwest", feature = "ureq2", feature = "tokio"))]

mod common;

use std::{
    collections::BTreeSet,
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use bolt_load::{
    adapter::{
        AdapterError, AnyBytesStream, BoltLoadAdapter, BoltLoadAdapterMeta, FullResponse,
        UnretryableError,
    },
    runtime::ThreadedRuntimeImpl,
    task::{DownloadError, DownloadMode, FallbackReason, Task, TaskOutput, TaskStatus},
};
use bolt_load_tests::adapter::{
    fault::{Gate, RangeFault},
    http_server::{RequestLog, create_range_fault_server_with_log},
    simple::{calculate_blake3, create_deterministic_content},
};
use bytes::Bytes;
use common::Client;
use reqwest::Method;
use url::Url;

const SIZE: usize = 256 * 1024;
const TIMEOUT: Duration = Duration::from_secs(60);

struct Run {
    result: Result<TaskOutput, DownloadError>,
    statuses: Vec<Arc<TaskStatus>>,
    log: RequestLog,
    file: Option<Vec<u8>>,
}

/// A task on `client` against `fault`, with every status it published.
async fn run(fault: RangeFault, client: Client, content: &Bytes) -> Run {
    let (port, server, log) = create_range_fault_server_with_log(content.clone(), None)
        .await
        .unwrap();
    let url = Url::parse(&format!("http://127.0.0.1:{port}{}", fault.path())).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.bin");
    let task = Task::builder(client.adapter(url))
        .runtime(ThreadedRuntimeImpl::new_tokio_rt())
        .save_to(&path)
        .spawn()
        .unwrap();
    let mut status = task.status();
    let recorder = tokio::spawn(async move {
        let mut statuses = vec![status.borrow_and_update().clone()];
        while status.changed().await.is_ok() {
            statuses.push(status.borrow_and_update().clone());
        }
        statuses
    });
    let result = tokio::time::timeout(TIMEOUT, task)
        .await
        .unwrap_or_else(|_| panic!("{fault:?} on {client:?} did not end"));
    let statuses = recorder.await.unwrap();
    server.abort();
    Run {
        result,
        statuses,
        log,
        file: std::fs::read(&path).ok(),
    }
}

/// The `If-Range` values the task sent, in order, without repeats.
fn sent(log: &RequestLog) -> Vec<String> {
    let mut sent = Vec::new();
    for value in log
        .requests()
        .into_iter()
        .filter_map(|request| request.if_range)
    {
        if sent.last() != Some(&value) {
            sent.push(value);
        }
    }
    sent
}

/// How many `bytes=0-0` probes the task sent: one per metadata retrieval
/// by GET, so a second one shows a download from scratch.
fn probes(log: &RequestLog) -> usize {
    log.requests()
        .iter()
        .filter(|request| request.method == Method::GET && request.range == Some((0, 0)))
        .count()
}

fn assert_file(run: &Run, expected: &[u8], context: &str) {
    assert_eq!(
        calculate_blake3(run.file.as_deref().expect("the file")),
        calculate_blake3(expected),
        "{context}"
    );
}

/// Replaced after the first range: the task downloads the new version
/// from scratch and writes exactly it. Nothing of the first run's written
/// ranges survives: every status of the new version stays within it.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::single_range_in_vec_init, reason = "a list holding one range")]
async fn a_resource_changed_mid_download_is_downloaded_again() {
    let content = Bytes::from(create_deterministic_content(SIZE));
    let replacement = RangeFault::replacement(&content);
    let len = replacement.len() as u64;
    for client in Client::ALL {
        let run = run(RangeFault::ChangedMidDownload, client, &content).await;
        let output = run.result.as_ref().unwrap_or_else(|e| panic!("{client:?}: {e}"));
        assert_file(&run, &replacement, &format!("{client:?}"));
        assert_eq!(output.progress.downloaded_chunks, [0..len], "{client:?}");
        assert_eq!(sent(&run.log), ["\"v1\"", "\"v2\""], "{client:?}");

        let mut downloaded = 0;
        for status in run.statuses.iter().filter(|status| {
            status
                .info
                .as_ref()
                .is_some_and(|info| info.meta.etag.as_deref() == Some("\"v2\""))
        }) {
            assert!(status.progress.downloaded >= downloaded, "{client:?}");
            downloaded = status.progress.downloaded;
            assert!(
                status
                    .progress
                    .downloaded_chunks
                    .iter()
                    .all(|range| range.end <= len),
                "{client:?}: {:?}",
                status.progress.downloaded_chunks
            );
        }
        let info = run.statuses.last().unwrap().info.clone().unwrap();
        assert_eq!(info.meta.content_size, len, "{client:?}");
        assert_eq!(info.mode, DownloadMode::Concurrent, "{client:?}");
    }
}

/// The download from scratch starts from nothing: while the server holds
/// every range of the new version, the task's status names the new
/// version with nothing downloaded or written. The ranges are held, so no
/// later status can stand in for that one.
#[tokio::test(flavor = "multi_thread")]
async fn the_download_from_scratch_starts_from_zero() {
    let content = Bytes::from(create_deterministic_content(SIZE));
    let replacement = RangeFault::replacement(&content);
    for client in Client::ALL {
        let gate = Gate::new();
        let (port, server, _log) =
            create_range_fault_server_with_log(content.clone(), Some(gate.clone()))
                .await
                .unwrap();
        let url = Url::parse(&format!(
            "http://127.0.0.1:{port}{}",
            RangeFault::ChangedMidDownload.path()
        ))
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.bin");
        let task = Task::builder(client.adapter(url))
            .runtime(ThreadedRuntimeImpl::new_tokio_rt())
            .save_to(&path)
            .spawn()
            .unwrap();
        tokio::time::timeout(TIMEOUT, gate.entered())
            .await
            .unwrap_or_else(|_| panic!("{client:?}: no range of the new version"));
        let mut status = task.status();
        let new = tokio::time::timeout(
            TIMEOUT,
            status.wait_for(|status| {
                status
                    .info
                    .as_ref()
                    .is_some_and(|info| info.meta.etag.as_deref() == Some("\"v2\""))
            }),
        )
        .await
        .unwrap()
        .unwrap()
        .clone();
        assert_eq!(new.progress.downloaded, 0, "{client:?}");
        assert!(
            new.progress
                .downloaded_chunks
                .iter()
                .all(|range| range.is_empty()),
            "{client:?}: {:?}",
            new.progress.downloaded_chunks
        );
        gate.release();
        let result = tokio::time::timeout(TIMEOUT, task).await.unwrap();
        result.unwrap_or_else(|e| panic!("{client:?}: {e}"));
        assert_eq!(
            calculate_blake3(&std::fs::read(&path).unwrap()),
            calculate_blake3(&replacement),
            "{client:?}"
        );
        server.abort();
    }
}

/// Changed again during the second download: the task fails, after one
/// download from scratch only.
#[tokio::test(flavor = "multi_thread")]
async fn a_resource_that_changes_again_fails_the_task() {
    let content = Bytes::from(create_deterministic_content(SIZE));
    for client in Client::ALL {
        let run = run(RangeFault::ChangedEveryRange, client, &content).await;
        assert!(
            matches!(run.result, Err(DownloadError::ResourceChanged { .. })),
            "{client:?}: {:?}",
            run.result
        );
        let sent: BTreeSet<_> = sent(&run.log).into_iter().collect();
        assert_eq!(
            sent,
            BTreeSet::from(["\"v1\"".to_owned(), "\"v2\"".to_owned()]),
            "{client:?}"
        );
        assert!(run.file.is_none(), "{client:?}");
    }
}

/// Changed, and the new version's server ignores ranges: the download from
/// scratch falls back to one connection, and writes the new version.
#[tokio::test(flavor = "multi_thread")]
async fn a_changed_resource_that_ignores_ranges_falls_back() {
    let content = Bytes::from(create_deterministic_content(SIZE));
    let replacement = RangeFault::replacement(&content);
    for client in Client::ALL {
        let run = run(RangeFault::ChangedThenIgnoresRange, client, &content).await;
        let output = run.result.as_ref().unwrap_or_else(|e| panic!("{client:?}: {e}"));
        assert_file(&run, &replacement, &format!("{client:?}"));
        assert_eq!(output.mode, DownloadMode::Singleton, "{client:?}");
        let info = run.statuses.last().unwrap().info.clone().unwrap();
        assert_eq!(
            info.fallback,
            Some(FallbackReason::RangeIgnored),
            "{client:?}"
        );
    }
}

/// A server that ignores ranges sends the validator the task sent: the
/// range is ignored, the resource unchanged, and the task falls back. The
/// validator is that of a GET, even when a HEAD names another.
#[tokio::test(flavor = "multi_thread")]
async fn ignored_ranges_with_the_same_validator_fall_back() {
    let content = Bytes::from(create_deterministic_content(SIZE));
    for fault in [
        RangeFault::IgnoreRangeWithValidators,
        RangeFault::IgnoreRangeWithHeadValidators,
    ] {
        for client in Client::ALL {
            let run = run(fault, client, &content).await;
            let output = run
                .result
                .as_ref()
                .unwrap_or_else(|e| panic!("{fault:?} {client:?}: {e}"));
            assert_file(&run, &content, &format!("{fault:?} {client:?}"));
            assert_eq!(output.mode, DownloadMode::Singleton, "{fault:?} {client:?}");
            assert_eq!(sent(&run.log), ["\"v1\""], "{fault:?} {client:?}");
        }
    }
}

/// A server that ignores `If-Range` and sends parts of the new version
/// with its validator, same size: the task notices the conflicting
/// validator, by `ETag` or by date, and downloads the new version from
/// scratch instead of mixing the two. Parts without a validator pass.
#[tokio::test(flavor = "multi_thread")]
async fn parts_of_another_version_are_not_mixed_in() {
    let content = Bytes::from(create_deterministic_content(SIZE));
    let changed = RangeFault::changed(&content);
    for fault in [
        RangeFault::ChangedIgnoringIfRange,
        RangeFault::ChangedByDateIgnoringIfRange,
    ] {
        for client in Client::ALL {
            let run = run(fault, client, &content).await;
            run.result
                .as_ref()
                .unwrap_or_else(|e| panic!("{fault:?} {client:?}: {e}"));
            assert_file(&run, &changed, &format!("{fault:?} {client:?}"));
            assert_eq!(probes(&run.log), 2, "{fault:?} {client:?}");
        }
    }
    for client in Client::ALL {
        let run = run(RangeFault::PartsWithoutValidators, client, &content).await;
        let output = run.result.as_ref().unwrap_or_else(|e| panic!("{client:?}: {e}"));
        assert_file(&run, &content, &format!("{client:?}"));
        assert_eq!(output.mode, DownloadMode::Concurrent, "{client:?}");
        assert_eq!(probes(&run.log), 1, "{client:?}");
        assert_eq!(sent(&run.log), ["\"v1\""], "{client:?}");
    }
}

/// A HEAD that names other validators than a GET does not make a stable
/// resource look changed: the task sends the GET's, once. A real change
/// afterwards is still noticed.
#[tokio::test(flavor = "multi_thread")]
async fn head_validators_that_differ_from_a_get_are_not_sent() {
    let content = Bytes::from(create_deterministic_content(SIZE));
    for client in Client::ALL {
        let run = run(RangeFault::HeadValidatorsDiffer, client, &content).await;
        let output = run.result.as_ref().unwrap_or_else(|e| panic!("{client:?}: {e}"));
        assert_file(&run, &content, &format!("{client:?}"));
        assert_eq!(output.mode, DownloadMode::Concurrent, "{client:?}");
        assert_eq!(probes(&run.log), 1, "{client:?}");
        assert_eq!(sent(&run.log), ["\"v1\""], "{client:?}");
    }
    let replacement = RangeFault::replacement(&content);
    for client in Client::ALL {
        let run = run(RangeFault::HeadValidatorsDifferThenChanged, client, &content).await;
        run.result
            .as_ref()
            .unwrap_or_else(|e| panic!("{client:?}: {e}"));
        assert_file(&run, &replacement, &format!("{client:?}"));
        assert_eq!(sent(&run.log), ["\"v1\"", "\"v2\""], "{client:?}");
    }
}

/// Two tasks from cloned builders share one adapter (design D14). Task A
/// has half of its first range held when the resource changes, same size.
/// Task B starts on the metadata A cached, notices the change, refreshes
/// the shared adapter and downloads the new version. A's next range still
/// carries A's own validator, so A notices the change too and downloads
/// the new version from scratch, instead of finishing a file mixed of both.
#[tokio::test(flavor = "multi_thread")]
async fn a_refresh_by_another_task_leaves_the_validator_of_a_task() {
    let content = Bytes::from(create_deterministic_content(SIZE));
    let changed = RangeFault::changed(&content);
    for client in Client::ALL {
        let gate = Gate::new();
        let (port, server, log) =
            create_range_fault_server_with_log(content.clone(), Some(gate.clone()))
                .await
                .unwrap();
        let url = Url::parse(&format!(
            "http://127.0.0.1:{port}{}",
            RangeFault::ReplacedWhileHeld.path()
        ))
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let builder =
            Task::builder(client.adapter(url)).runtime(ThreadedRuntimeImpl::new_tokio_rt());

        // One connection, so A's only range is the held one.
        let a = builder
            .clone()
            .max_connections(NonZeroUsize::MIN)
            .save_to(dir.path().join("a.bin"))
            .spawn()
            .unwrap();
        tokio::time::timeout(TIMEOUT, gate.entered())
            .await
            .unwrap_or_else(|_| panic!("{client:?}: A's range was not held"));

        let b = builder.save_to(dir.path().join("b.bin")).spawn().unwrap();
        let b = tokio::time::timeout(TIMEOUT, b).await.unwrap();
        b.unwrap_or_else(|e| panic!("{client:?}: B failed: {e}"));
        assert_eq!(
            calculate_blake3(&std::fs::read(dir.path().join("b.bin")).unwrap()),
            calculate_blake3(&changed),
            "{client:?}"
        );
        // A probed; B used what A cached, and probed after its refresh.
        assert_eq!(probes(&log), 2, "{client:?}");

        gate.release();
        let a = tokio::time::timeout(TIMEOUT, a).await.unwrap();
        match a {
            Ok(_) => assert_eq!(
                calculate_blake3(&std::fs::read(dir.path().join("a.bin")).unwrap()),
                calculate_blake3(&changed),
                "{client:?}: A finished a mixed file"
            ),
            Err(DownloadError::ResourceChanged { .. }) => {}
            Err(e) => panic!("{client:?}: A failed: {e}"),
        }
        server.abort();
    }
}

/// An adapter whose ranges always show a changed resource, and whose
/// second capability check waits at `gate`.
struct BlocksSecondCheck {
    checks: AtomicUsize,
    gate: Gate,
}

impl BoltLoadAdapter for BlocksSecondCheck {
    async fn is_range_stream_available(&self) -> bool {
        if self.checks.fetch_add(1, Ordering::SeqCst) > 0 {
            self.gate.pass().await;
        }
        true
    }

    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError> {
        Ok(BoltLoadAdapterMeta::new(SIZE as u64))
    }

    async fn full_stream(&self) -> Result<FullResponse, AdapterError> {
        Ok(FullResponse::new(Box::pin(futures::stream::empty())))
    }

    async fn range_stream(&self, _start: u64, _end: u64) -> Result<AnyBytesStream, AdapterError> {
        Err(UnretryableError::ResourceChanged.into())
    }
}

/// The download from scratch selects the mode again inside the task's
/// cancellation: an adapter that never answers its capability check does
/// not keep a cancelled task alive.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_task_does_not_wait_for_the_mode_of_a_restart() {
    let gate = Gate::new();
    let dir = tempfile::tempdir().unwrap();
    let task = Task::builder(BlocksSecondCheck {
        checks: AtomicUsize::new(0),
        gate: gate.clone(),
    })
    .runtime(ThreadedRuntimeImpl::new_tokio_rt())
    .save_to(dir.path().join("out.bin"))
    .spawn()
    .unwrap();
    tokio::time::timeout(TIMEOUT, gate.entered())
        .await
        .expect("the restart checks the mode again");
    task.cancel();
    let result = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("cancellation completes while the check waits");
    gate.release();
    assert!(
        matches!(result, Err(DownloadError::Cancelled)),
        "{result:?}"
    );
}
