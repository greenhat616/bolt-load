//! Range faults never complete with wrong content (G1).
//!
//! Every fault of the fault server runs a whole task on both adapters. The
//! task either writes the exact content or fails with the typed error the
//! fault calls for.

#![cfg(all(feature = "reqwest", feature = "ureq2", feature = "tokio"))]

use std::time::Duration;

use bolt_load::{
    adapter::{AdapterError, AnyAdapter, DynBoltLoadAdapter, UnretryableError},
    runner::TaskError,
    runtime::ThreadedRuntimeImpl,
    task::{DownloadError, DownloadMode, Task},
};
use bolt_load_adapter::{reqwest::IntoReqwestAdapter, ureq2::IntoUreqAdapter};
use bolt_load_tests::adapter::{
    fault::{CONGESTED_RETRY_AFTER_SECS, RangeFault},
    http_server::create_range_fault_server,
    simple::{calculate_blake3, create_deterministic_content},
};
use bytes::Bytes;
use url::Url;

const SIZE: usize = 256 * 1024;

#[derive(Clone, Copy, Debug)]
enum Client {
    Reqwest,
    Ureq2,
}

impl Client {
    fn adapter(self, url: Url) -> Box<AnyAdapter> {
        match self {
            Client::Reqwest => {
                // As the adapter README asks: a decoding client would hide
                // `Content-Encoding` from the range check.
                let client = reqwest::Client::builder()
                    .no_proxy()
                    .no_gzip()
                    .no_brotli()
                    .no_zstd()
                    .no_deflate()
                    .build()
                    .unwrap();
                DynBoltLoadAdapter::new_box(
                    client.into_reqwest_adapter((reqwest::Method::GET, url)),
                )
            }
            Client::Ureq2 => {
                DynBoltLoadAdapter::new_box(ureq2::Agent::new().into_ureq_adapter(("get", url)))
            }
        }
    }
}

/// The adapter error a failed transfer ended with.
fn stream_error(error: &DownloadError) -> Option<&AdapterError> {
    match error.as_transfer()? {
        TaskError::StreamError { source } => Some(source),
        _ => None,
    }
}

fn unretryable(error: &DownloadError) -> Option<&UnretryableError> {
    match stream_error(error)? {
        AdapterError::Unretryable { source } => Some(source),
        AdapterError::Retryable { .. } => None,
    }
}

/// Whether `error` is the failure `fault` calls for on `client`.
fn is_expected_failure(fault: RangeFault, client: Client, error: &DownloadError) -> bool {
    let Some(source) = unretryable(error) else {
        return false;
    };
    match (fault, client) {
        // Until the single-connection fallback (T5) lands.
        (RangeFault::IgnoreRange, _) => matches!(source, UnretryableError::RangeIgnored),
        (RangeFault::ShiftedStart, _) => matches!(
            source,
            UnretryableError::RangeMismatch { requested, got: Some(got) }
                if got.first == requested.start + 1
        ),
        (RangeFault::WrongTotal, _) => matches!(
            source,
            UnretryableError::RangeMismatch { got: Some(got), .. }
                if got.complete == Some(SIZE as u64 + 1)
        ),
        (RangeFault::Unsatisfiable, _) => matches!(
            source,
            UnretryableError::RangeNotSatisfiable { total: Some(total) } if *total == SIZE as u64
        ),
        (RangeFault::Congested, _) => matches!(
            source,
            UnretryableError::Congestion { status: 503, retry_after: Some(delay) }
                if *delay == Duration::from_secs(CONGESTED_RETRY_AFTER_SECS)
        ),
        // Neither client decodes, so both see `Content-Encoding`.
        (RangeFault::Gzip | RangeFault::EncodedRepresentation, _) => {
            matches!(source, UnretryableError::RangeMismatch { got: Some(_), .. })
        }
        (RangeFault::Truncated, _) => false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn range_faults_never_complete_with_wrong_content() {
    let content = Bytes::from(create_deterministic_content(SIZE));
    let expected_hash = calculate_blake3(&content);

    for fault in RangeFault::ALL {
        for client in [Client::Reqwest, Client::Ureq2] {
            // A fresh server per run, so a one-off fault fires on each.
            let (port, server) = create_range_fault_server(content.clone()).await.unwrap();
            let url = Url::parse(&format!("http://127.0.0.1:{port}{}", fault.path())).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("out.bin");
            let task = Task::builder(client.adapter(url))
                .runtime(ThreadedRuntimeImpl::new_tokio_rt())
                .save_to(&path)
                .spawn()
                .unwrap();
            let result = tokio::time::timeout(Duration::from_secs(60), task)
                .await
                .unwrap_or_else(|_| panic!("{fault:?} on {client:?} did not end"));
            server.abort();

            match result {
                Ok(output) => {
                    assert_eq!(
                        calculate_blake3(&std::fs::read(&path).unwrap()),
                        expected_hash,
                        "{fault:?} on {client:?} completed with wrong content"
                    );
                    assert!(
                        fault == RangeFault::Truncated,
                        "{fault:?} on {client:?} should fail"
                    );
                    assert_eq!(output.mode, DownloadMode::Concurrent, "{client:?}");
                }
                Err(error) => assert!(
                    is_expected_failure(fault, client, &error),
                    "{fault:?} on {client:?} failed with {error:?}"
                ),
            }
        }
    }
}
