use std::{
    io::{BufWriter, Seek, Write},
    sync::Arc,
};

use rand::Rng;
use tempfile::tempfile;
use tokio::net::TcpListener;

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

#[allow(dead_code)]
pub async fn create_http_server() -> anyhow::Result<(u16, tokio::task::JoinHandle<()>)> {
    create_http_server_with_file_size(1024 * 1024).await
}

#[allow(dead_code)]
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

    let app = axum::Router::new()
        .route("/no_range", axum::routing::get(no_range_handler))
        .route("/range", axum::routing::get(range_handler))
        .with_state(holder);

    let handle = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .await
            .unwrap()
    });

    Ok((port, handle))
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use reqwest::{
        Client, StatusCode,
        header::{CONTENT_LENGTH, CONTENT_RANGE, RANGE},
    };

    use super::*;

    struct ServerGuard(tokio::task::JoinHandle<()>);

    impl Drop for ServerGuard {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn server_starts_and_serves_full_content() -> Result<()> {
        let (port, handle) = create_http_server().await?;
        let _guard = ServerGuard(handle);

        let client = Client::new();
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

        let client = Client::new();
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

        let client = Client::new();
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
}
