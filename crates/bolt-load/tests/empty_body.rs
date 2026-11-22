//! An empty body is an empty file (#153), however the server frames it.
//!
//! IDM 6.43b16 was seen to complete the first five responses with a 0-byte
//! file; `204 No Content` was not tried on it. Each runs a whole task on
//! every client.

#![cfg(all(feature = "reqwest", feature = "ureq2", feature = "tokio"))]

mod common;

use std::time::Duration;

use bolt_load::{
    runtime::ThreadedRuntimeImpl,
    task::{DownloadMode, Task},
};
use bolt_load_tests::adapter::http_server::gzip;
use common::Client;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use url::Url;

/// How the server frames an empty body.
#[derive(Clone, Copy, Debug)]
enum Framing {
    /// `Content-Length: 0`.
    Length,
    /// A chunked body with only the last chunk.
    Chunked,
    /// No length: the server closes the connection.
    Close,
    /// gzip of nothing, with its coded length.
    GzipLength,
    /// gzip of nothing, chunked.
    GzipChunked,
    /// `204 No Content`, which has no body. A full download accepts any
    /// status outside 4xx and 5xx.
    NoContent,
}

impl Framing {
    const ALL: [Self; 6] = [
        Self::Length,
        Self::Chunked,
        Self::Close,
        Self::GzipLength,
        Self::GzipChunked,
        Self::NoContent,
    ];

    fn is_coded(self) -> bool {
        matches!(self, Self::GzipLength | Self::GzipChunked)
    }

    /// The response to `GET`, or to `HEAD` without the body.
    fn response(self, head: bool) -> Vec<u8> {
        let coded = gzip(b"").unwrap();
        let (headers, body) = match self {
            Self::Length => ("Content-Length: 0\r\n".to_owned(), Vec::new()),
            Self::Chunked => (
                "Transfer-Encoding: chunked\r\n".to_owned(),
                b"0\r\n\r\n".to_vec(),
            ),
            Self::Close => ("Connection: close\r\n".to_owned(), Vec::new()),
            Self::GzipLength => (
                format!(
                    "Content-Encoding: gzip\r\nContent-Length: {}\r\n",
                    coded.len()
                ),
                coded.to_vec(),
            ),
            Self::GzipChunked => (
                "Content-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n".to_owned(),
                [
                    format!("{:x}\r\n", coded.len()).as_bytes(),
                    &coded,
                    b"\r\n0\r\n\r\n",
                ]
                .concat(),
            ),
            Self::NoContent => (String::new(), Vec::new()),
        };
        let status = match self {
            Self::NoContent => "204 No Content",
            _ => "200 OK",
        };
        let mut response =
            format!("HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\n{headers}\r\n")
                .into_bytes();
        if !head {
            response.extend_from_slice(&body);
        }
        response
    }
}

/// Answer every request on 127.0.0.1 as `framing` says.
async fn serve(framing: Framing) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            tokio::spawn(respond(socket, framing));
        }
    });
    (port, handle)
}

async fn respond(mut socket: TcpStream, framing: Framing) {
    let mut buffer = Vec::new();
    loop {
        let head_len = loop {
            if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                break end + 4;
            }
            let mut chunk = [0; 1024];
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        };
        let head = buffer.starts_with(b"HEAD ");
        buffer.drain(..head_len);
        if socket.write_all(&framing.response(head)).await.is_err() {
            return;
        }
        if matches!(framing, Framing::Close) {
            let _ = socket.shutdown().await;
            return;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_body_is_an_empty_file() {
    // Without the feature, the task cannot decode gzip.
    let framings = Framing::ALL
        .into_iter()
        .filter(|framing| !framing.is_coded() || cfg!(feature = "gzip"));
    for framing in framings {
        for client in Client::ALL {
            let (port, server) = serve(framing).await;
            let url = Url::parse(&format!("http://127.0.0.1:{port}/empty.bin")).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("empty.bin");
            let task = Task::builder(client.adapter(url))
                .runtime(ThreadedRuntimeImpl::new_tokio_rt())
                .save_to(&path)
                .spawn()
                .unwrap();
            let result = tokio::time::timeout(Duration::from_secs(30), task)
                .await
                .unwrap_or_else(|_| panic!("{framing:?} on {client:?} did not end"));
            server.abort();

            let output = result
                .unwrap_or_else(|error| panic!("{framing:?} on {client:?} failed: {error:?}"));
            assert_eq!(
                output.mode,
                DownloadMode::Singleton,
                "{framing:?} on {client:?}"
            );
            assert_eq!(output.progress.downloaded, 0, "{framing:?} on {client:?}");
            assert_eq!(output.progress.total, Some(0), "{framing:?} on {client:?}");
            assert_eq!(
                std::fs::read(&path).unwrap(),
                b"",
                "{framing:?} on {client:?}"
            );
        }
    }
}
