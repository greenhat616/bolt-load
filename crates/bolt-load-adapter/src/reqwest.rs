use std::{pin::Pin, sync::Arc};

use async_once_cell::OnceCell;
use async_trait::async_trait;
use bolt_load_utils::http::ContentDisposition;
use futures::Stream;
use reqwest::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use url::Url;

use super::{
    AnyBytesStream, AnyStream, BoltLoadAdapter, BoltLoadAdapterMeta, RetryableError, StreamError,
    UnretryableError,
};

type BeforeRequestFn =
    Arc<dyn Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder + Send + Sync>;

struct Context {
    pub is_range_stream_available: bool,
    pub meta: BoltLoadAdapterMeta,
}

#[derive(Clone)]
struct Target(reqwest::Method, Url);

#[derive(Clone)]
#[non_exhaustive]
pub struct ReqwestAdapter {
    client: reqwest::Client,
    target: Target,
    context: Arc<OnceCell<Context>>,
    before_request: Option<BeforeRequestFn>,
}

pub trait IntoReqwestAdapter {
    fn into_reqwest_adapter(self, target: (reqwest::Method, Url)) -> ReqwestAdapter;
}

impl IntoReqwestAdapter for reqwest::Client {
    fn into_reqwest_adapter(self, target: (reqwest::Method, Url)) -> ReqwestAdapter {
        ReqwestAdapter {
            client: self,
            target: Target(target.0, target.1),
            context: Arc::new(OnceCell::new()),
            before_request: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct ReqwestError(#[from] reqwest::Error);

impl From<ReqwestError> for StreamError {
    fn from(ReqwestError(e): ReqwestError) -> Self {
        if let Some(status_code) = e.status()
            && status_code.is_client_error()
        {
            match status_code {
                reqwest::StatusCode::NOT_FOUND => {
                    return UnretryableError::new_io_error(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        e.to_string(),
                    ))
                    .into();
                }
                reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::UNAUTHORIZED => {
                    return UnretryableError::new_io_error(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        e.to_string(),
                    ))
                    .into();
                }
                reqwest::StatusCode::TOO_MANY_REQUESTS
                | reqwest::StatusCode::SERVICE_UNAVAILABLE => {
                    return UnretryableError::new_exceeded_request_limits(format!(
                        "HTTP Status Code: {} {}",
                        status_code,
                        status_code.canonical_reason().unwrap_or("unknown")
                    ))
                    .into();
                }
                _ => {
                    return RetryableError::new_io_error(std::io::Error::other(e.to_string()))
                        .into();
                }
            }
        }
        if e.is_builder() || e.is_body() {
            return UnretryableError::new_io_error(std::io::Error::other(e.to_string())).into();
        }

        // fallback to other errors
        RetryableError::new_io_error(std::io::Error::other(e.to_string())).into()
    }
}

impl ReqwestAdapter {
    async fn perform_head(&self) -> Result<reqwest::Response, StreamError> {
        let response = self
            .apply_before_request(self.client.head(self.target.1.clone()))
            .send()
            .await
            .map_err(ReqwestError::from)?;
        Ok(response.error_for_status().map_err(ReqwestError::from)?)
    }

    pub fn before_request(
        &mut self,
        f: impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder + Send + Sync + 'static,
    ) {
        self.before_request = Some(Arc::new(f));
    }

    #[inline]
    fn apply_before_request(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.before_request.as_ref() {
            Some(f) => f(builder),
            _ => builder,
        }
    }

    fn get_content_size(response: &reqwest::Response) -> Option<u64> {
        // First, try to parse the `Content-Range` header
        let content_size: Option<u64> = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split_once('/'))
            .and_then(|(_, end)| end.parse().ok());
        // Then, try to parse the `Content-Length` header
        match content_size {
            Some(size) => Some(size),
            None => response
                .content_length()
                .and_then(|len| if len > 0 { Some(len) } else { None })
                // fallback to just parse the CONTENT_LENGTH header
                .or_else(|| {
                    response
                        .headers()
                        .get(CONTENT_LENGTH)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse().ok())
                }),
        }
    }

    async fn suggest_filename(response: &reqwest::Response) -> Option<String> {
        response
            .headers()
            .get(CONTENT_DISPOSITION)
            .and_then(|v| ContentDisposition::from_raw(v).ok())
            .and_then(|d| d.get_filename().map(String::from))
    }

    async fn get_context(&self) -> Result<&Context, StreamError> {
        self.context.get_or_try_init(fetch_remote_meta(self)).await
    }
}

struct ReqwestStream<'a>(AnyStream<'a, Result<bytes::Bytes, reqwest::Error>>);

impl<'a> Stream for ReqwestStream<'a> {
    type Item = Result<bytes::Bytes, super::StreamError>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        Pin::new(&mut self.get_mut().0)
            .poll_next(cx)
            .map(|opt| opt.map(|res| res.map_err(ReqwestError::from).map_err(StreamError::from)))
    }
}

async fn fetch_remote_meta(adapter: &ReqwestAdapter) -> Result<Context, StreamError> {
    // Firstly, try to perform a `RANGE` GET request to test if the remote source supports range requests
    let Target(method, url) = adapter.target.clone();
    let response = adapter
        .apply_before_request(
            adapter
                .client
                .request(method.clone(), url.clone())
                .header(RANGE, "bytes=0-0"),
        )
        .send()
        .await
        .map_err(ReqwestError::from)?;

    if let Ok(response) = response.error_for_status()
        && let Some(size) = ReqwestAdapter::get_content_size(&response)
    {
        return Ok(Context {
            is_range_stream_available: true,
            meta: BoltLoadAdapterMeta {
                content_size: size,
                filename: ReqwestAdapter::suggest_filename(&response).await,
            },
        });
    }

    // Try to perform a `HEAD` request to get the meta
    let response = adapter.perform_head().await?;
    return Ok(Context {
        is_range_stream_available: false,
        meta: BoltLoadAdapterMeta {
            content_size: ReqwestAdapter::get_content_size(&response).unwrap_or_default(),
            filename: ReqwestAdapter::suggest_filename(&response).await,
        },
    });
}

#[async_trait]
impl BoltLoadAdapter for ReqwestAdapter {
    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, UnretryableError> {
        let context = self.get_context().await?;

        Ok(BoltLoadAdapterMeta {
            content_size: context.meta.content_size,
            filename: context.meta.filename.clone(),
        })
    }

    async fn is_range_stream_available(&self) -> bool {
        match self.get_context().await {
            Ok(context) => context.is_range_stream_available,
            Err(_) => false,
        }
    }

    async fn full_stream(&self) -> Result<AnyBytesStream, StreamError> {
        let response = self
            .apply_before_request(
                self.client
                    .request(self.target.0.clone(), self.target.1.clone()),
            )
            .send()
            .await
            .map_err(ReqwestError::from)?
            .error_for_status()
            .map_err(ReqwestError::from)?;
        let stream = ReqwestStream(Box::pin(response.bytes_stream()));
        Ok(Box::pin(stream))
    }

    async fn range_stream(&self, start: u64, end: u64) -> Result<AnyBytesStream, StreamError> {
        let response = self
            .apply_before_request(
                self.client
                    .request(self.target.0.clone(), self.target.1.clone())
                    .header(RANGE, format!("bytes={}-{}", start, end - 1)),
            )
            .send()
            .await
            .map_err(ReqwestError::from)?
            .error_for_status()
            .map_err(ReqwestError::from)?;
        let stream = ReqwestStream(Box::pin(response.bytes_stream()));
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod test {
    use bolt_load_tests::adapter::http_server;
    use futures::StreamExt;
    use pretty_assertions::assert_eq;

    use super::*;

    #[tokio::test]
    async fn test_get_content_size() {
        let content_size: u64 = 1024 * 1024;
        let (port, _) = http_server::create_http_server_with_file_size(content_size as usize)
            .await
            .unwrap();
        let url = Url::parse(&format!("http://localhost:{port}/no_range")).unwrap();
        let client = reqwest::Client::new();
        let adapter = client
            .clone()
            .into_reqwest_adapter((reqwest::Method::GET, url));
        assert_eq!(
            adapter.retrieve_meta().await.unwrap().content_size,
            content_size
        );

        let url = Url::parse(&format!("http://localhost:{port}/range")).unwrap();
        let adapter = client.into_reqwest_adapter((reqwest::Method::GET, url));
        assert_eq!(
            adapter.retrieve_meta().await.unwrap().content_size,
            content_size
        );
    }

    #[tokio::test]
    async fn test_suggest_filename() {
        let (port, _) = http_server::create_http_server().await.unwrap();
        let url = Url::parse(&format!("http://localhost:{port}/no_range")).unwrap();
        let client = reqwest::Client::new();
        let adapter = client.into_reqwest_adapter((reqwest::Method::GET, url));
        assert_eq!(
            adapter.retrieve_meta().await.unwrap().filename,
            Some("test.txt".to_owned())
        );
    }

    #[tokio::test]
    async fn test_is_range_stream_available() {
        let (port, _) = http_server::create_http_server().await.unwrap();
        let url = Url::parse(&format!("http://localhost:{port}/range")).unwrap();
        let client = reqwest::Client::new();
        let adapter = client.into_reqwest_adapter((reqwest::Method::GET, url));
        assert!(
            adapter.is_range_stream_available().await,
            "range stream should be available"
        );

        let url = Url::parse(&format!("http://localhost:{port}/no_range")).unwrap();
        let client = reqwest::Client::new();
        let adapter = client.into_reqwest_adapter((reqwest::Method::GET, url));
        assert!(
            !adapter.is_range_stream_available().await,
            "range stream should not be available"
        );
    }

    #[tokio::test]
    async fn test_full_stream() {
        let content_size: u64 = 1024 * 1024;
        let (port, _) = http_server::create_http_server_with_file_size(content_size as usize)
            .await
            .unwrap();
        let url = Url::parse(&format!("http://localhost:{port}/no_range")).unwrap();
        let client = reqwest::Client::new();
        let adapter = client.into_reqwest_adapter((reqwest::Method::GET, url));
        let mut stream = adapter.full_stream().await.unwrap();
        let mut bytes = bytes::BytesMut::new();
        while let Some(item) = stream.next().await {
            bytes.extend_from_slice(&item.unwrap());
        }
        assert_eq!(bytes.len(), content_size as usize);
    }

    #[tokio::test]
    async fn test_range_stream() {
        let (port, _) = http_server::create_http_server().await.unwrap();
        let url = Url::parse(&format!("http://localhost:{port}/range")).unwrap();
        let client = reqwest::Client::new();
        let adapter = client.into_reqwest_adapter((reqwest::Method::GET, url));
        let mut stream = adapter.range_stream(0, 100).await.unwrap();
        let mut bytes = bytes::BytesMut::new();
        while let Some(item) = stream.next().await {
            bytes.extend_from_slice(&item.unwrap());
        }
        assert_eq!(bytes.len(), 100);
    }
}
