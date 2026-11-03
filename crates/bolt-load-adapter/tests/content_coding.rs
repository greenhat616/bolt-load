//! A content-coded range response never reaches the task as other bytes,
//! and fails at once instead of being retried.

#![cfg(all(feature = "reqwest", feature = "ureq2"))]

use bolt_load_adapter::{reqwest::IntoReqwestAdapter, ureq2::IntoUreqAdapter};
use bolt_load_core::adapter::{AdapterError, AnyBytesStream, BoltLoadAdapter, UnretryableError};
use bolt_load_tests::adapter::{fault::RangeFault, http_server::create_range_fault_server};
use futures::StreamExt;
use url::Url;

/// The unretryable error a range stream ends with, before or during its body.
async fn failure(stream: Result<AnyBytesStream, AdapterError>) -> UnretryableError {
    let error = match stream {
        Ok(mut stream) => loop {
            match stream.next().await {
                Some(Ok(_)) => {}
                Some(Err(error)) => break error,
                None => panic!("the encoded range must fail"),
            }
        },
        Err(error) => error,
    };
    match error {
        AdapterError::Unretryable { source } => source,
        other => panic!("expected an unretryable error, got {other:?}"),
    }
}

/// Bytes that do not compress, so gzip makes them longer.
fn incompressible(len: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

#[tokio::test]
async fn an_encoded_range_fails_whether_or_not_the_client_decodes() {
    // Compressible, so the decoded body is far longer than the encoded one.
    let content = "bolt-load ".repeat(8 * 1024);
    let (port, server) = create_range_fault_server(content.clone().into())
        .await
        .unwrap();
    let url = Url::parse(&format!(
        "http://127.0.0.1:{port}{}",
        RangeFault::EncodedRepresentation.path()
    ))
    .unwrap();

    // The dev-dependency enables reqwest's `gzip`, so this client decodes and
    // drops `Content-Encoding`: only the body length gives it away.
    let decoding = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .into_reqwest_adapter((reqwest::Method::GET, url.clone()));
    let total = decoding.retrieve_meta().await.unwrap().content_size;
    assert!(
        total < content.len() as u64,
        "the meta names the encoded length"
    );
    assert!(matches!(
        failure(decoding.range_stream(0, total).await).await,
        UnretryableError::RangeBodyTooLong { got } if got.first == 0 && got.last == total - 1
    ));

    // Clients that do not decode see the header.
    let plain = reqwest::Client::builder()
        .no_proxy()
        .no_gzip()
        .build()
        .unwrap()
        .into_reqwest_adapter((reqwest::Method::GET, url.clone()));
    let ureq = ureq2::Agent::new().into_ureq_adapter(("get", url));
    for (client, error) in [
        ("reqwest", failure(plain.range_stream(0, total).await).await),
        ("ureq2", failure(ureq.range_stream(0, total).await).await),
    ] {
        assert!(
            matches!(error, UnretryableError::RangeMismatch { got: Some(_), .. }),
            "{client}: {error:?}"
        );
    }
    server.abort();
}

#[tokio::test]
async fn a_decoded_range_of_incompressible_data_fails_at_once() {
    let content = incompressible(64 * 1024);
    let (port, server) = create_range_fault_server(content.clone().into())
        .await
        .unwrap();
    let url = Url::parse(&format!(
        "http://127.0.0.1:{port}{}",
        RangeFault::EncodedRepresentation.path()
    ))
    .unwrap();
    let decoding = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .into_reqwest_adapter((reqwest::Method::GET, url));
    let total = decoding.retrieve_meta().await.unwrap().content_size;
    assert!(
        total > content.len() as u64,
        "gzip makes incompressible data longer"
    );

    // The whole encoded representation decodes, to fewer bytes than it has.
    assert!(matches!(
        failure(decoding.range_stream(0, total).await).await,
        UnretryableError::RangeBodyTooShort { got, received }
            if got.last == total - 1 && received == content.len() as u64
    ));
    // A range from the middle cannot be decoded at all; retrying would not
    // change that.
    assert!(matches!(
        failure(decoding.range_stream(total / 2, total).await).await,
        UnretryableError::Io { .. }
    ));
    server.abort();
}
