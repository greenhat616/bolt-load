//! Range faults never complete with wrong content (G1).
//!
//! Every fault of the fault server runs a whole task on each client. The
//! task either writes the exact content or fails with the typed error the
//! fault calls for. Faults that show ranges cannot carry the file complete
//! over one connection, and the status says why.

#![cfg(all(feature = "reqwest", feature = "ureq2", feature = "tokio"))]

use std::time::Duration;

use bolt_load::{
    adapter::{AdapterError, AnyAdapter, DynBoltLoadAdapter, UnretryableError},
    runner::TaskError,
    runtime::ThreadedRuntimeImpl,
    task::{DownloadError, DownloadMode, FallbackReason, Task},
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Client {
    Reqwest,
    /// reqwest with its gzip decoder, which hides `Content-Encoding`.
    DecodingReqwest,
    Ureq2,
}

impl Client {
    const ALL: [Self; 3] = [Self::Reqwest, Self::DecodingReqwest, Self::Ureq2];

    fn adapter(self, url: Url) -> Box<AnyAdapter> {
        match self {
            Client::Reqwest | Client::DecodingReqwest => {
                let mut client = reqwest::Client::builder().no_proxy();
                if self == Client::Reqwest {
                    client = client.no_gzip().no_brotli().no_zstd().no_deflate();
                }
                DynBoltLoadAdapter::new_box(
                    client
                        .build()
                        .unwrap()
                        .into_reqwest_adapter((reqwest::Method::GET, url)),
                )
            }
            Client::Ureq2 => {
                DynBoltLoadAdapter::new_box(ureq2::Agent::new().into_ureq_adapter(("get", url)))
            }
        }
    }
}

/// How a task on `client` must end under `fault`.
enum Outcome {
    /// With the content; `fallback` says whether over one connection.
    Completes { fallback: Option<FallbackReason> },
    /// With the error [`is_expected_failure`] accepts.
    Fails,
}

fn outcome(fault: RangeFault, client: Client) -> Outcome {
    match (fault, client) {
        (RangeFault::IgnoreRange | RangeFault::ReplacedAfterShortRange, _) => Outcome::Completes {
            fallback: Some(FallbackReason::RangeIgnored),
        },
        (RangeFault::Gzip, _) | (RangeFault::EncodedRepresentation, Client::DecodingReqwest) => {
            Outcome::Completes {
                fallback: Some(FallbackReason::ContentCoded),
            }
        }
        (RangeFault::Truncated, _) => Outcome::Completes { fallback: None },
        _ => Outcome::Fails,
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

/// Whether `error` is the failure `fault` calls for.
fn is_expected_failure(fault: RangeFault, error: &DownloadError) -> bool {
    let Some(source) = unretryable(error) else {
        return false;
    };
    match fault {
        RangeFault::ShiftedStart => matches!(
            source,
            UnretryableError::RangeMismatch { requested, got: Some(got) }
                if got.first == requested.start + 1
        ),
        RangeFault::WrongTotal => matches!(
            source,
            UnretryableError::RangeMismatch { got: Some(got), .. }
                if got.complete == Some(SIZE as u64 + 1)
        ),
        RangeFault::Unsatisfiable => matches!(
            source,
            UnretryableError::RangeNotSatisfiable { total: Some(total) } if *total == SIZE as u64
        ),
        RangeFault::Congested => matches!(
            source,
            UnretryableError::Congestion { status: 503, retry_after: Some(delay) }
                if *delay == Duration::from_secs(CONGESTED_RETRY_AFTER_SECS)
        ),
        // A client that does not decode is left with the coded body, which
        // is not the file. Decoding it is T23.
        RangeFault::EncodedRepresentation => {
            matches!(source, UnretryableError::ContentCoded { coding } if coding == "gzip")
        }
        _ => false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn range_faults_never_complete_with_wrong_content() {
    let content = Bytes::from(create_deterministic_content(SIZE));

    for fault in RangeFault::ALL {
        // The fallback must write only what the full response carries.
        let expected = if fault == RangeFault::ReplacedAfterShortRange {
            RangeFault::replacement(&content)
        } else {
            content.clone()
        };
        for client in Client::ALL {
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
            let status = task.status();
            let result = tokio::time::timeout(Duration::from_secs(60), task)
                .await
                .unwrap_or_else(|_| panic!("{fault:?} on {client:?} did not end"));
            server.abort();

            match (result, outcome(fault, client)) {
                (Ok(output), Outcome::Completes { fallback }) => {
                    assert_eq!(
                        calculate_blake3(&std::fs::read(&path).unwrap()),
                        calculate_blake3(&expected),
                        "{fault:?} on {client:?} completed with wrong content"
                    );
                    let info = status.borrow().info.clone().unwrap();
                    assert_eq!(info.fallback, fallback, "{fault:?} on {client:?}");
                    let mode = match fallback {
                        Some(_) => DownloadMode::Singleton,
                        None => DownloadMode::Concurrent,
                    };
                    assert_eq!(output.mode, mode, "{fault:?} on {client:?}");
                    assert_eq!(info.mode, mode, "{fault:?} on {client:?}");
                    if fallback.is_some() {
                        assert_eq!(output.progress.total, Some(expected.len() as u64));
                    }
                }
                (Ok(_), Outcome::Fails) => {
                    panic!("{fault:?} on {client:?} should fail")
                }
                (Err(error), Outcome::Fails) => assert!(
                    is_expected_failure(fault, &error),
                    "{fault:?} on {client:?} failed with {error:?}"
                ),
                (Err(error), Outcome::Completes { .. }) => {
                    panic!("{fault:?} on {client:?} should complete, failed with {error:?}")
                }
            }
        }
    }
}
