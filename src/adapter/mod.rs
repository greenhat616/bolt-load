use async_trait::async_trait;
use futures::Stream;

#[cfg(feature = "reqwest")]
mod reqwest;

#[async_trait]
pub trait BoltLoadAdapter: Send + Sync {
    type Item: Send + Sync;
    type Stream: Stream<Item = Self::Item> + Unpin + Send;

    /// Check if the adapter supports range stream
    /// For compatibility, error should be returned as false.
    async fn is_range_stream_available(&self) -> bool {
        false
    }

    /// Suggest a filename from the adapter
    async fn suggest_filename(&self) -> Option<String> {
        None
    }

    /// Get the content size of the adapter
    async fn get_content_size(&self) -> std::io::Result<u64>;

    /// Get a full content stream from the adapter
    async fn full_stream(&self) -> std::io::Result<Self::Stream>;

    /// Get a range content stream from the adapter
    /// Note: the range is followed as [start, end)
    #[allow(unused_variables)]
    async fn range_stream(&self, start: u64, end: u64) -> std::io::Result<Self::Stream> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Range stream is not supported",
        ))
    }
}

pub type AnyStream<T> = Box<dyn Stream<Item = T> + Unpin + Send>;

// TODO: maybe the chunk should be zero copy
// pub trait BoltLoaderAdapterAnyStream =
//     BoltLoadAdapter<Box<dyn Stream<Item = Vec<u8>> + Send>, Vec<u8>>;

#[cfg(test)]
mod tests {
    use axum::response::IntoResponse;
    use rand::Rng;
    use std::{
        io::{BufWriter, Seek, Write},
        sync::Arc,
    };
    use tempfile::tempfile;
    use tokio::{io::AsyncSeekExt, net::TcpListener};

    pub fn create_random_file(size: usize) -> anyhow::Result<std::fs::File> {
        let mut file = tempfile()?;
        let mut writer = BufWriter::new(file.try_clone()?);
        let mut rng = rand::thread_rng();
        let mut buffer = [0; 1024];
        let mut remaining_size = size;
        while remaining_size > 0 {
            let bytes_to_write = std::cmp::min(remaining_size, buffer.len());
            rng.fill(&mut buffer[..bytes_to_write]);
            writer.write_all(&buffer[..bytes_to_write])?;
            remaining_size -= bytes_to_write;
        }
        // reset the file pointer to the beginning
        file.seek(std::io::SeekFrom::Start(0))?;
        Ok(file)
    }

    #[derive(Clone)]
    struct FileHolder(Arc<tokio::sync::Mutex<tokio::fs::File>>);

    pub async fn create_http_server() -> anyhow::Result<(u16, tokio::task::JoinHandle<()>)> {
        use axum::extract::State;
        use axum_extra::TypedHeader;

        let file = tokio::task::spawn_blocking(|| create_random_file(1024 * 1024)).await??;
        let holder = FileHolder(Arc::new(tokio::sync::Mutex::new(
            tokio::fs::File::from_std(file),
        )));
        let port = portpicker::pick_unused_port()
            .ok_or(anyhow::anyhow!("Failed to pick an unused port"))?;
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
                        (
                            axum::http::header::CONTENT_LENGTH,
                            &format!("{}", file_size),
                        ),
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
}
