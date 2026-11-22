//! Both adapters classify the same response the same way.

#![cfg(all(feature = "reqwest", feature = "ureq2"))]

use std::time::Duration;

use bolt_load_adapter::{reqwest::IntoReqwestAdapter, ureq2::IntoUreqAdapter};
use bolt_load_core::adapter::{AdapterError, BoltLoadAdapter, UnretryableError};
use bolt_load_http::classify_status;
use bolt_load_tests::adapter::http_server::{self, STATUS_RETRY_AFTER_SECS};
use url::Url;

const STATUSES: [u16; 13] = [
    401, 403, 404, 408, 410, 416, 429, 500, 503, 599, 409, 412, 418,
];

fn error<T>(result: Result<T, AdapterError>) -> AdapterError {
    match result {
        Ok(_) => panic!("an error status must not yield a body"),
        Err(e) => e,
    }
}

#[tokio::test]
async fn reqwest_and_ureq2_classify_statuses_alike() {
    let (port, server) = http_server::create_http_server().await.unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let agent = ureq2::Agent::new();

    for status in STATUSES {
        let url = Url::parse(&format!("http://127.0.0.1:{port}/status/{status}")).unwrap();
        let reqwest = client
            .clone()
            .into_reqwest_adapter((reqwest::Method::GET, url.clone()));
        let ureq = agent.clone().into_ureq_adapter(("get", url));

        // The server sends `Retry-After` with 429 and 503 only.
        let retry_after =
            matches!(status, 429 | 503).then_some(Duration::from_secs(STATUS_RETRY_AFTER_SECS));
        let expected = format!("{:?}", classify_status(status, retry_after));
        for (adapter, got) in [
            ("reqwest full", error(reqwest.full_stream().await)),
            ("reqwest range", error(reqwest.range_stream(0, 10).await)),
            ("ureq2 full", error(ureq.full_stream().await)),
            ("ureq2 range", error(ureq.range_stream(0, 10).await)),
        ] {
            assert_eq!(format!("{got:?}"), expected, "{adapter} on {status}");
        }
    }
    server.abort();
}

#[tokio::test]
async fn retry_after_reaches_the_congestion_error() {
    let (port, server) = http_server::create_http_server().await.unwrap();
    let url = Url::parse(&format!("http://127.0.0.1:{port}/status/503")).unwrap();
    let adapter = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .into_reqwest_adapter((reqwest::Method::GET, url));

    assert!(matches!(
        error(adapter.full_stream().await),
        AdapterError::Unretryable {
            source: UnretryableError::Congestion {
                status: 503,
                retry_after: Some(d),
            }
        } if d == Duration::from_secs(STATUS_RETRY_AFTER_SECS)
    ));
    server.abort();
}

#[tokio::test]
async fn a_body_cut_off_mid_stream_is_retryable_on_both() {
    use bolt_load_tests::adapter::{fault::RangeFault, http_server::create_range_fault_server};
    use futures::StreamExt;

    const SIZE: usize = 64 * 1024;
    for client in ["reqwest", "ureq2"] {
        // A fresh server, since only its first range response is cut.
        let (port, server) = create_range_fault_server(vec![0u8; SIZE].into())
            .await
            .unwrap();
        let url = Url::parse(&format!(
            "http://127.0.0.1:{port}{}",
            RangeFault::Truncated.path()
        ))
        .unwrap();
        let stream = if client == "reqwest" {
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .into_reqwest_adapter((reqwest::Method::GET, url))
                .range_stream(0, SIZE as u64)
                .await
        } else {
            ureq2::Agent::new()
                .into_ureq_adapter(("get", url))
                .range_stream(0, SIZE as u64)
                .await
        };
        let mut stream = stream.unwrap();
        let mut received = 0;
        let error = loop {
            match stream.next().await {
                Some(Ok(bytes)) => received += bytes.len(),
                Some(Err(e)) => break e,
                None => panic!("{client}: the body must be cut off"),
            }
        };
        assert_eq!(received, SIZE / 2, "{client}");
        assert!(
            matches!(error, AdapterError::Retryable { .. }),
            "{client}: {error:?}"
        );
        server.abort();
    }
}
