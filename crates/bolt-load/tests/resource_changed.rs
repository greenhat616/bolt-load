//! A resource that changes during the download (design §11.2, §14.23):
//! every range carries `If-Range`, and a changed resource fails the task
//! with `ResourceChanged`, leaving no file. It never takes the fallback to
//! one connection, which could not notice the change. A server that
//! ignores ranges with the validator sent still takes the fallback.
//!
//! Downloading the new version again is left to the layer above the task
//! (#195).

#![cfg(all(feature = "reqwest", feature = "ureq2", feature = "tokio"))]

mod common;

use std::{path::Path, time::Duration};

use bolt_load::{
    runtime::ThreadedRuntimeImpl,
    task::{DownloadError, DownloadMode, Task, TaskOutput},
};
use bolt_load_tests::adapter::{
    fault::RangeFault,
    http_server::{RequestLog, create_range_fault_server_with_log},
    simple::{calculate_blake3, create_deterministic_content},
};
use bytes::Bytes;
use common::Client;
use reqwest::Method;
use url::Url;

const SIZE: usize = 256 * 1024;

struct Run {
    result: Result<TaskOutput, DownloadError>,
    log: RequestLog,
    file: Option<Vec<u8>>,
}

/// A server for `fault`, its log, and a builder on `client` for it.
async fn serve(
    fault: RangeFault,
    client: Client,
    content: &Bytes,
) -> (
    tokio::task::JoinHandle<()>,
    RequestLog,
    bolt_load::task::TaskBuilder,
) {
    let (port, server, log) = create_range_fault_server_with_log(content.clone(), None)
        .await
        .unwrap();
    let url = Url::parse(&format!("http://127.0.0.1:{port}{}", fault.path())).unwrap();
    let builder = Task::builder(client.adapter(url)).runtime(ThreadedRuntimeImpl::new_tokio_rt());
    (server, log, builder)
}

/// Wait for `task`, saving to `path`, and read what it left there. The
/// temporary file is gone, whatever the outcome.
async fn finish(
    task: Task,
    path: &Path,
    context: &str,
) -> (Result<TaskOutput, DownloadError>, Option<Vec<u8>>) {
    let result = tokio::time::timeout(Duration::from_secs(60), task)
        .await
        .unwrap_or_else(|_| panic!("{context} did not end"));
    let mut tmp = path.to_owned().into_os_string();
    tmp.push(bolt_load::DOWNLOADING_TMP_EXTENSION);
    assert!(!Path::new(&tmp).exists(), "{context} left {tmp:?}");
    (result, std::fs::read(path).ok())
}

/// A task on `client` against `fault`.
async fn run(fault: RangeFault, client: Client, content: &Bytes) -> Run {
    let (server, log, builder) = serve(fault, client, content).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.bin");
    let task = builder.save_to(&path).spawn().unwrap();
    let (result, file) = finish(task, &path, &format!("{fault:?} on {client:?}")).await;
    server.abort();
    Run { result, log, file }
}

/// The `If-Range` of every range request other than the probe, in order,
/// without consecutive repeats: `None` for one that sent none.
fn sent(log: &RequestLog) -> Vec<Option<String>> {
    let mut sent = Vec::new();
    for request in log.requests() {
        let data =
            request.method == Method::GET && request.range.is_some_and(|range| range != (0, 0));
        if data && sent.last() != Some(&request.if_range) {
            sent.push(request.if_range);
        }
    }
    sent
}

/// Every range request sent `value`.
fn only(value: &str) -> Vec<Option<String>> {
    vec![Some(value.to_owned())]
}

/// How many `bytes=0-0` probes were sent.
fn probes(log: &RequestLog) -> usize {
    log.requests()
        .iter()
        .filter(|request| request.method == Method::GET && request.range == Some((0, 0)))
        .count()
}

fn assert_changed(run: &Run, context: &str) {
    assert!(
        matches!(run.result, Err(DownloadError::ResourceChanged { .. })),
        "{context}: {:?}",
        run.result
    );
    assert!(run.file.is_none(), "{context}: a file was left");
}

fn assert_file(run: &Run, expected: &[u8], context: &str) {
    assert_eq!(
        calculate_blake3(run.file.as_deref().expect("the file")),
        calculate_blake3(expected),
        "{context}"
    );
}

/// Replaced after the first range: the next range, conditional on the
/// first version, gets the new one, and the task fails, leaving no file.
#[tokio::test(flavor = "multi_thread")]
async fn a_resource_changed_mid_download_fails_the_task() {
    let content = Bytes::from(create_deterministic_content(SIZE));
    for fault in [
        RangeFault::ChangedMidDownload,
        RangeFault::HeadValidatorsDifferThenChanged,
    ] {
        for client in Client::ALL {
            let run = run(fault, client, &content).await;
            assert_changed(&run, &format!("{fault:?} {client:?}"));
            assert_eq!(sent(&run.log), only("\"v1\""), "{fault:?} {client:?}");
        }
    }
}

/// Changed, and the new version's server ignores ranges: the task fails
/// with `ResourceChanged`, and does not fall back to one connection.
#[tokio::test(flavor = "multi_thread")]
async fn a_changed_resource_that_ignores_ranges_does_not_fall_back() {
    let content = Bytes::from(create_deterministic_content(SIZE));
    for client in Client::ALL {
        let run = run(RangeFault::ChangedThenIgnoresRange, client, &content).await;
        assert_changed(&run, &format!("{client:?}"));
        assert_eq!(sent(&run.log), only("\"v1\""), "{client:?}");
    }
}

/// A server that ignores `If-Range` and sends parts of the new version
/// with its validator, same size: the task notices the conflicting
/// validator, by `ETag` or by date, and fails instead of mixing the two
/// versions. Parts without a validator pass.
#[tokio::test(flavor = "multi_thread")]
async fn parts_of_another_version_are_not_mixed_in() {
    let content = Bytes::from(create_deterministic_content(SIZE));
    for fault in [
        RangeFault::ChangedIgnoringIfRange,
        RangeFault::ChangedByDateIgnoringIfRange,
    ] {
        for client in Client::ALL {
            let run = run(fault, client, &content).await;
            assert_changed(&run, &format!("{fault:?} {client:?}"));
        }
    }
    for client in Client::ALL {
        let run = run(RangeFault::PartsWithoutValidators, client, &content).await;
        let output = run
            .result
            .as_ref()
            .unwrap_or_else(|e| panic!("{client:?}: {e}"));
        assert_file(&run, &content, &format!("{client:?}"));
        assert_eq!(output.mode, DownloadMode::Concurrent, "{client:?}");
        assert_eq!(sent(&run.log), only("\"v1\""), "{client:?}");
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
            assert_eq!(sent(&run.log), only("\"v1\""), "{fault:?} {client:?}");
        }
    }
}

/// A HEAD that names other validators than a GET does not make a stable
/// resource look changed: the task sends the GET's and finishes.
#[tokio::test(flavor = "multi_thread")]
async fn head_validators_that_differ_from_a_get_are_not_sent() {
    let content = Bytes::from(create_deterministic_content(SIZE));
    for client in Client::ALL {
        let run = run(RangeFault::HeadValidatorsDiffer, client, &content).await;
        let output = run
            .result
            .as_ref()
            .unwrap_or_else(|e| panic!("{client:?}: {e}"));
        assert_file(&run, &content, &format!("{client:?}"));
        assert_eq!(output.mode, DownloadMode::Concurrent, "{client:?}");
        assert_eq!(sent(&run.log), only("\"v1\""), "{client:?}");
    }
}

/// A probe the server rejects leaves the adapter without a validator, for
/// its whole life: ranges carry no `If-Range`, and the task completes, but
/// could not notice a change (design §14.23).
#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_probe_leaves_ranges_unconditional() {
    let content = Bytes::from(create_deterministic_content(SIZE));
    for client in Client::ALL {
        let run = run(RangeFault::RejectedProbe, client, &content).await;
        let output = run
            .result
            .as_ref()
            .unwrap_or_else(|e| panic!("{client:?}: {e}"));
        assert_file(&run, &content, &format!("{client:?}"));
        assert_eq!(output.mode, DownloadMode::Concurrent, "{client:?}");
        assert_eq!(sent(&run.log), [None], "{client:?}");
    }
}

/// Two tasks from cloned builders share one adapter, and with it the
/// validator it probed once: both send it with every range.
#[tokio::test(flavor = "multi_thread")]
async fn tasks_sharing_an_adapter_send_its_validator() {
    let content = Bytes::from(create_deterministic_content(SIZE));
    for client in Client::ALL {
        let (server, log, builder) =
            serve(RangeFault::HeadValidatorsDiffer, client, &content).await;
        let dir = tempfile::tempdir().unwrap();
        let paths = [dir.path().join("a.bin"), dir.path().join("b.bin")];
        let tasks = paths
            .iter()
            .map(|path| builder.clone().save_to(path).spawn().unwrap())
            .collect::<Vec<_>>();
        for (task, path) in tasks.into_iter().zip(&paths) {
            let (result, file) = finish(task, path, &format!("{client:?}")).await;
            result.unwrap_or_else(|e| panic!("{client:?}: {e}"));
            assert_eq!(
                calculate_blake3(&file.unwrap()),
                calculate_blake3(&content),
                "{client:?}"
            );
        }
        assert_eq!(probes(&log), 1, "{client:?}");
        assert_eq!(sent(&log), only("\"v1\""), "{client:?}");
        server.abort();
    }
}

/// A later task on the same adapter keeps the validator it probed before
/// the change, so it fails at once instead of downloading the new version.
/// This documents what #195 has to change: a new attempt needs fresh
/// metadata, from a fresh adapter for now.
#[tokio::test(flavor = "multi_thread")]
async fn a_later_task_on_the_same_adapter_keeps_the_old_validator() {
    let content = Bytes::from(create_deterministic_content(SIZE));
    for client in Client::ALL {
        let (server, log, builder) = serve(RangeFault::ChangedMidDownload, client, &content).await;
        let dir = tempfile::tempdir().unwrap();
        for name in ["first.bin", "later.bin"] {
            let path = dir.path().join(name);
            let task = builder.clone().save_to(&path).spawn().unwrap();
            let (result, file) = finish(task, &path, &format!("{client:?} {name}")).await;
            assert!(
                matches!(result, Err(DownloadError::ResourceChanged { .. })),
                "{client:?} {name}: {result:?}"
            );
            assert!(file.is_none(), "{client:?} {name}");
        }
        assert_eq!(probes(&log), 1, "{client:?}");
        assert_eq!(sent(&log), only("\"v1\""), "{client:?}");
        server.abort();
    }
}
