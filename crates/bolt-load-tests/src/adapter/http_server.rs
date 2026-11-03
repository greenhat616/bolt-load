use std::{
    io::{BufWriter, Seek, Write},
    ops::Bound,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use axum::{
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use rand::RngExt;
use tempfile::tempfile;
use tokio::net::TcpListener;

use super::fault::{CONGESTED_RETRY_AFTER_SECS, RangeFault};

/// Create a random file with given size, useful for testing
pub fn create_random_file(size: usize) -> std::io::Result<std::fs::File> {
    let mut file = tempfile()?;
    let mut writer = BufWriter::new(file.try_clone()?);
    let mut rng = rand::rng();
    let mut buffer = [0; 1024];
    let mut remaining_size = size;
    while remaining_size > 0 {
        let bytes_to_write = std::cmp::min(remaining_size, buffer.len());
        rng.fill(&mut buffer[..bytes_to_write]);
        writer.write_all(&buffer[..bytes_to_write])?;
        remaining_size -= bytes_to_write;
    }
    // ensure buffered data hits disk before we read/serve it
    writer.flush()?;
    file.sync_all()?;
    // reset the file pointer to the beginning
    file.seek(std::io::SeekFrom::Start(0))?;
    Ok(file)
}

#[derive(Clone)]
struct FileHolder(Arc<tokio::sync::Mutex<tokio::fs::File>>);

pub const DEFAULT_FILE_SIZE: usize = 1024 * 1024;

/// The `Retry-After` seconds that `/status/429` and `/status/503` send.
pub const STATUS_RETRY_AFTER_SECS: u64 = 7;

pub async fn create_http_server() -> anyhow::Result<(u16, tokio::task::JoinHandle<()>)> {
    create_http_server_with_file_size(DEFAULT_FILE_SIZE).await
}

pub async fn create_http_server_with_file_size(
    file_size: usize,
) -> anyhow::Result<(u16, tokio::task::JoinHandle<()>)> {
    use axum::{extract::State, response::IntoResponse};
    use axum_extra::TypedHeader;
    use tokio::io::AsyncSeekExt;

    let file = tokio::task::spawn_blocking(move || create_random_file(file_size)).await??;
    let holder = FileHolder(Arc::new(tokio::sync::Mutex::new(
        tokio::fs::File::from_std(file),
    )));
    let port =
        portpicker::pick_unused_port().ok_or(anyhow::anyhow!("Failed to pick an unused port"))?;
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;

    /// a handler send without range
    async fn no_range_handler(
        State(holder): State<FileHolder>,
    ) -> impl axum::response::IntoResponse {
        let mut file = holder.0.lock().await;
        match file.seek(std::io::SeekFrom::Start(0)).await {
            Ok(_) => {
                let file_size = file.metadata().await.unwrap().len();
                let reader = tokio_util::io::ReaderStream::new(file.try_clone().await.unwrap());
                let body = axum::body::Body::from_stream(reader);
                let headers = [
                    (
                        axum::http::header::CONTENT_TYPE,
                        "text/plain; charset=utf-8",
                    ),
                    (axum::http::header::CONTENT_LENGTH, &format!("{file_size}")),
                    (
                        axum::http::header::CONTENT_DISPOSITION,
                        "attachment; filename=\"test.txt\"",
                    ),
                ];
                (headers, body).into_response()
            }
            Err(e) => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                e.to_string().into_response(),
            )
                .into_response(),
        }
    }

    /// a handler mock range stream
    async fn range_handler(
        State(holder): State<FileHolder>,
        range: Option<TypedHeader<axum_extra::headers::Range>>,
    ) -> impl axum::response::IntoResponse {
        let file = holder.0.lock().await;
        let mut file_cloned = file.try_clone().await.unwrap();
        file_cloned.seek(std::io::SeekFrom::Start(0)).await.unwrap();
        let body = axum_range::KnownSize::file(file_cloned).await.unwrap();
        let range = range.map(|TypedHeader(range)| range);
        let ranged = axum_range::Ranged::new(range, body);
        ranged.into_response()
    }

    /// a handler answering with the status in the path
    async fn status_handler(
        axum::extract::Path(code): axum::extract::Path<u16>,
    ) -> axum::response::Response {
        let Ok(status) = axum::http::StatusCode::from_u16(code) else {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        };
        let mut response = status.into_response();
        if matches!(code, 429 | 503) {
            response.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from(STATUS_RETRY_AFTER_SECS),
            );
        }
        response
    }

    let app = axum::Router::new()
        .route("/no_range", axum::routing::get(no_range_handler))
        .route("/range", axum::routing::get(range_handler))
        .route("/status/{code}", axum::routing::get(status_handler))
        .with_state(holder);

    let handle = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .await
            .unwrap()
    });

    Ok((port, handle))
}

#[derive(Clone)]
struct FaultState {
    content: Bytes,
    /// `content` as gzip, for [`RangeFault::EncodedRepresentation`].
    encoded: Bytes,
    /// Whether [`RangeFault::Truncated`] already cut a response.
    truncated: Arc<AtomicBool>,
}

/// Serve `content` at every [`RangeFault::path`], misbehaving on range
/// requests as the fault says. HEAD and `GET` without `Range` answer
/// normally, with `Accept-Ranges: bytes` so that clients try ranges.
pub async fn create_range_fault_server(
    content: Bytes,
) -> anyhow::Result<(u16, tokio::task::JoinHandle<()>)> {
    let port =
        portpicker::pick_unused_port().ok_or(anyhow::anyhow!("Failed to pick an unused port"))?;
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let state = FaultState {
        encoded: gzip(&content)?,
        content,
        truncated: Arc::new(AtomicBool::new(false)),
    };
    let app = axum::Router::new()
        .route("/{fault}", axum::routing::get(fault_handler))
        .with_state(state);
    let handle = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .await
            .unwrap()
    });
    Ok((port, handle))
}

/// `content` as a gzip stream.
pub fn gzip(content: &[u8]) -> std::io::Result<Bytes> {
    use std::io::Write;

    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(content)?;
    Ok(encoder.finish()?.into())
}

/// The first range of a `Range: bytes=…` header, as `(first, last)`.
fn requested_range(headers: &HeaderMap, len: u64) -> Option<(u64, u64)> {
    use axum_extra::headers::{Header, Range};

    let range = Range::decode(&mut headers.get_all(header::RANGE).iter()).ok()?;
    let (start, end) = range.satisfiable_ranges(len).next()?;
    let first = match start {
        Bound::Included(first) => first,
        Bound::Excluded(first) => first + 1,
        Bound::Unbounded => 0,
    };
    let last = match end {
        Bound::Included(last) => last.min(len - 1),
        Bound::Excluded(end) => end.min(len) - 1,
        Bound::Unbounded => len - 1,
    };
    (first <= last).then_some((first, last))
}

fn partial_headers(first: u64, last: u64, total: u64) -> [(header::HeaderName, String); 2] {
    [
        (
            header::CONTENT_RANGE,
            format!("bytes {first}-{last}/{total}"),
        ),
        (header::CONTENT_LENGTH, (last - first + 1).to_string()),
    ]
}

fn partial(content: &Bytes, first: u64, last: u64, total: u64) -> Response {
    let body = content.slice(first as usize..=last as usize);
    (
        StatusCode::PARTIAL_CONTENT,
        partial_headers(first, last, total),
        body,
    )
        .into_response()
}

/// A 206 for `first..=last` whose body fails halfway, which aborts the
/// connection before `Content-Length` is reached.
fn truncated(content: &Bytes, first: u64, last: u64, total: u64) -> Response {
    let body = content.slice(first as usize..=last as usize);
    let stream = async_stream::stream! {
        yield Ok(body.slice(..body.len() / 2));
        // Pending once, so the server flushes the headers and the first half
        // before the failure drops them.
        tokio::task::yield_now().await;
        yield Err(std::io::Error::other("truncated by the fault server"));
    };
    (
        StatusCode::PARTIAL_CONTENT,
        partial_headers(first, last, total),
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}

async fn fault_handler(
    axum::extract::Path(fault): axum::extract::Path<String>,
    axum::extract::State(state): axum::extract::State<FaultState>,
    method: axum::http::Method,
    headers: HeaderMap,
) -> Response {
    let Some(fault) = RangeFault::from_name(&fault) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let encoded = fault == RangeFault::EncodedRepresentation;
    let content = if encoded {
        &state.encoded
    } else {
        &state.content
    };
    let len = content.len() as u64;
    // The GET route also serves HEAD, which never gets a fault.
    let mut response = if method == axum::http::Method::HEAD || !headers.contains_key(header::RANGE)
    {
        content.clone().into_response()
    } else {
        let range = requested_range(&headers, len);
        let is_probe = range == Some((0, 0));
        match (fault, range) {
            (RangeFault::IgnoreRange, _) => content.clone().into_response(),
            (RangeFault::Congested, _) => (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, CONGESTED_RETRY_AFTER_SECS.to_string())],
            )
                .into_response(),
            (RangeFault::Unsatisfiable, _) | (_, None) => (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{len}"))],
            )
                .into_response(),
            (RangeFault::ShiftedStart, Some((first, last))) if !is_probe => {
                let shifted = if first + 1 < len {
                    first + 1
                } else {
                    first - 1
                };
                partial(content, shifted, last.max(shifted), len)
            }
            (RangeFault::WrongTotal, Some((first, last))) if !is_probe => {
                partial(content, first, last, len + 1)
            }
            (RangeFault::Gzip, Some((first, last))) => {
                let mut response = partial(content, first, last, len);
                response
                    .headers_mut()
                    .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
                response
            }
            (RangeFault::Truncated, Some((first, last)))
                if !is_probe && !state.truncated.swap(true, Ordering::SeqCst) =>
            {
                truncated(content, first, last, len)
            }
            (_, Some((first, last))) => partial(content, first, last, len),
        }
    };
    if encoded {
        response
            .headers_mut()
            .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use reqwest::{
        Client, StatusCode,
        header::{
            ACCEPT_RANGES, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, RANGE, RETRY_AFTER,
        },
    };

    use super::*;

    struct ServerGuard(tokio::task::JoinHandle<()>);

    impl Drop for ServerGuard {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    /// Never decodes, whatever features the build enables.
    fn local_client() -> Result<Client> {
        Ok(Client::builder()
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_zstd()
            .no_deflate()
            .build()?)
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn server_starts_and_serves_full_content() -> Result<()> {
        let (port, handle) = create_http_server().await?;
        let _guard = ServerGuard(handle);

        let client = local_client()?;
        let resp = client
            .get(format!("http://127.0.0.1:{port}/no_range"))
            .send()
            .await?;

        assert_eq!(resp.status(), StatusCode::OK);

        let headers = resp.headers();
        assert_eq!(
            headers.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()),
            Some("1048576")
        );

        let body = resp.bytes().await?;
        assert_eq!(body.len(), 1024 * 1024);

        Ok(())
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn server_supports_range_header() -> Result<()> {
        let (port, handle) = create_http_server().await?;
        let _guard = ServerGuard(handle);

        let client = local_client()?;
        let resp = client
            .get(format!("http://127.0.0.1:{port}/range"))
            .header(RANGE, "bytes=0-99")
            .send()
            .await?;

        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);

        let headers = resp.headers();
        let content_range = headers
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            content_range.starts_with("bytes 0-99/"),
            "unexpected Content-Range header: {content_range}"
        );

        let body = resp.bytes().await?;
        assert_eq!(body.len(), 100);

        Ok(())
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn no_range_endpoint_ignores_range_header() -> Result<()> {
        let (port, handle) = create_http_server().await?;
        let _guard = ServerGuard(handle);

        let client = local_client()?;
        let resp = client
            .get(format!("http://127.0.0.1:{port}/no_range"))
            .header(RANGE, "bytes=0-99")
            .send()
            .await?;

        // Should return 200 OK, not 206 Partial Content
        assert_eq!(resp.status(), StatusCode::OK);

        let headers = resp.headers();
        // Should not have Content-Range header
        assert!(
            headers.get(CONTENT_RANGE).is_none(),
            "no_range endpoint should not return Content-Range header"
        );

        // Should return full content length
        assert_eq!(
            headers.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()),
            Some("1048576")
        );

        // Should return full content, not just the requested range
        let body = resp.bytes().await?;
        assert_eq!(body.len(), 1024 * 1024);

        Ok(())
    }

    #[tokio::test]
    async fn range_fault_server_misbehaves_as_told() -> Result<()> {
        let content = Bytes::from_static(b"0123456789");
        let (port, handle) = create_range_fault_server(content.clone()).await?;
        let _guard = ServerGuard(handle);
        let client = local_client()?;
        let get = |fault: RangeFault, range: Option<&'static str>| {
            let mut request = client.get(format!("http://127.0.0.1:{port}{}", fault.path()));
            if let Some(range) = range {
                request = request.header(RANGE, range);
            }
            request.send()
        };
        let header = |resp: &reqwest::Response, name| {
            resp.headers()
                .get(name)
                .map(|v| v.to_str().unwrap().to_owned())
        };

        let encoded = gzip(&content)?;
        for fault in RangeFault::ALL {
            let resp = get(fault, None).await?;
            assert_eq!(resp.status(), StatusCode::OK, "{fault:?}");
            assert_eq!(header(&resp, ACCEPT_RANGES).as_deref(), Some("bytes"));
            let body = resp.bytes().await?;
            if fault == RangeFault::EncodedRepresentation {
                assert_eq!(body, encoded);
            } else {
                assert_eq!(body, content, "{fault:?}");
            }
            // HEAD with `Range` gets no fault either.
            let resp = client
                .head(format!("http://127.0.0.1:{port}{}", fault.path()))
                .header(RANGE, "bytes=2-4")
                .send()
                .await?;
            assert_eq!(resp.status(), StatusCode::OK, "HEAD {fault:?}");
        }

        let resp = get(RangeFault::IgnoreRange, Some("bytes=2-4")).await?;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.bytes().await?, content);

        let resp = get(RangeFault::ShiftedStart, Some("bytes=2-4")).await?;
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            header(&resp, CONTENT_RANGE).as_deref(),
            Some("bytes 3-4/10")
        );
        assert_eq!(resp.bytes().await?, "34");
        // One-byte ranges shift too, but not the probe.
        for (range, got) in [
            ("bytes=2-2", "bytes 3-3/10"),
            ("bytes=9-9", "bytes 8-9/10"),
            ("bytes=0-0", "bytes 0-0/10"),
        ] {
            let resp = get(RangeFault::ShiftedStart, Some(range)).await?;
            assert_eq!(
                header(&resp, CONTENT_RANGE).as_deref(),
                Some(got),
                "{range}"
            );
        }

        let resp = get(RangeFault::WrongTotal, Some("bytes=0-0")).await?;
        assert_eq!(
            header(&resp, CONTENT_RANGE).as_deref(),
            Some("bytes 0-0/10")
        );
        let resp = get(RangeFault::WrongTotal, Some("bytes=2-4")).await?;
        assert_eq!(
            header(&resp, CONTENT_RANGE).as_deref(),
            Some("bytes 2-4/11")
        );

        let resp = get(RangeFault::Unsatisfiable, Some("bytes=2-4")).await?;
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(header(&resp, CONTENT_RANGE).as_deref(), Some("bytes */10"));

        let resp = get(RangeFault::Congested, Some("bytes=2-4")).await?;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            header(&resp, RETRY_AFTER),
            Some(CONGESTED_RETRY_AFTER_SECS.to_string())
        );

        let resp = get(RangeFault::Gzip, Some("bytes=2-4")).await?;
        assert_eq!(header(&resp, CONTENT_ENCODING).as_deref(), Some("gzip"));
        assert_eq!(
            header(&resp, CONTENT_RANGE).as_deref(),
            Some("bytes 2-4/10")
        );

        let resp = get(RangeFault::EncodedRepresentation, Some("bytes=2-4")).await?;
        assert_eq!(header(&resp, CONTENT_ENCODING).as_deref(), Some("gzip"));
        assert_eq!(
            header(&resp, CONTENT_RANGE),
            Some(format!("bytes 2-4/{}", encoded.len()))
        );
        assert_eq!(resp.bytes().await?, encoded.slice(2..5));

        Ok(())
    }

    #[tokio::test]
    async fn range_fault_server_cuts_the_first_range_response() -> Result<()> {
        // Large enough that the headers and the first half leave the server
        // before the body fails.
        let content = Bytes::from(vec![7u8; DEFAULT_FILE_SIZE]);
        let (port, handle) = create_range_fault_server(content.clone()).await?;
        let _guard = ServerGuard(handle);
        let url = format!("http://127.0.0.1:{port}{}", RangeFault::Truncated.path());
        let last = DEFAULT_FILE_SIZE - 1;
        // A fresh client each time, so no request reuses the cut connection.
        let get = |range: String| async {
            local_client()?
                .get(&url)
                .header(RANGE, range)
                .send()
                .await
                .map_err(anyhow::Error::from)
        };

        // The probe is never cut.
        assert_eq!(get("bytes=0-0".into()).await?.bytes().await?.len(), 1);
        let resp = get(format!("bytes=0-{last}")).await?;
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert!(resp.bytes().await.is_err(), "the body must be cut off");
        let resp = get(format!("bytes=0-{last}")).await?;
        assert_eq!(resp.bytes().await?, content);

        Ok(())
    }
}
