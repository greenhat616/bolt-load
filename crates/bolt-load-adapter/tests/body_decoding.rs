//! A body that cannot be decoded fails at once; a broken connection is
//! retried, whether or not the client decodes.

#![cfg(feature = "reqwest")]

use bolt_load_adapter::reqwest::IntoReqwestAdapter;
use bolt_load_core::adapter::{AdapterError, BoltLoadAdapter, UnretryableError};
use futures::StreamExt;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use url::Url;

/// A gzip member header with no deflate data after it.
const GZIP_HEADER: &[u8] = b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\xff";

/// Answer every connection with `head` and `body`, then close it. A
/// `Content-Length` larger than `body` cuts the body off.
async fn serve(head: String, body: &'static [u8]) -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let head = head.clone();
            tokio::spawn(async move {
                let mut request = [0u8; 4096];
                let _ = socket.read(&mut request).await;
                socket.write_all(head.as_bytes()).await.unwrap();
                socket.write_all(body).await.unwrap();
                socket.shutdown().await.unwrap();
            });
        }
    });
    url
}

fn head(content_length: usize, gzip: bool) -> String {
    let encoding = if gzip {
        "Content-Encoding: gzip\r\n"
    } else {
        ""
    };
    format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {content_length}\r\n{encoding}Connection: \
         close\r\n\r\n"
    )
}

/// The error the whole-resource stream ends with.
async fn body_error(url: Url) -> AdapterError {
    // Decodes, as the dev-dependency enables reqwest's `gzip`.
    let adapter = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .into_reqwest_adapter((reqwest::Method::GET, url));
    let mut stream = adapter.full_stream().await.unwrap();
    loop {
        match stream.next().await {
            Some(Ok(_)) => {}
            Some(Err(error)) => return error,
            None => panic!("the body must fail"),
        }
    }
}

#[tokio::test]
async fn an_undecodable_body_is_unretryable() {
    let invalid = serve(head(8, true), b"not gzip").await;
    let unfinished = serve(head(GZIP_HEADER.len(), true), GZIP_HEADER).await;
    for (case, url) in [
        ("invalid header", invalid),
        ("unfinished stream", unfinished),
    ] {
        let error = body_error(url).await;
        assert!(
            matches!(
                error,
                AdapterError::Unretryable {
                    source: UnretryableError::BodyUndecodable { .. }
                }
            ),
            "{case}: {error:?}"
        );
    }
}

#[tokio::test]
async fn a_body_cut_off_is_retryable_even_when_decoded() {
    let encoded = serve(head(100, true), GZIP_HEADER).await;
    let plain = serve(head(100, false), b"0123456789").await;
    for (case, url) in [("gzip", encoded), ("identity", plain)] {
        let error = body_error(url).await;
        assert!(
            matches!(error, AdapterError::Retryable { .. }),
            "{case}: {error:?}"
        );
    }
}
