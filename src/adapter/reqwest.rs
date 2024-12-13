use std::{pin::Pin, sync::Arc};

use super::{
    AnyBytesStream, AnyStream, BoltLoadAdapter, BoltLoadAdapterMeta, RetryableError, StreamError,
    UnretryableError,
};
use async_trait::async_trait;
use futures::Stream;
use reqwest::header::{ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use url::Url;

type BeforeRequestFn =
    Box<dyn Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder + Send + Sync>;

#[derive(Clone)]
#[non_exhaustive]
pub struct ReqwestAdapter {
    client: reqwest::Client,
    target: (reqwest::Method, Url),
    head_response: Arc<async_lock::Mutex<Option<reqwest::Response>>>,
    before_request: Arc<Option<BeforeRequestFn>>,
}

pub trait IntoReqwestAdapter {
    fn into_reqwest_adapter(self, target: (reqwest::Method, Url)) -> ReqwestAdapter;
}

impl IntoReqwestAdapter for reqwest::Client {
    fn into_reqwest_adapter(self, target: (reqwest::Method, Url)) -> ReqwestAdapter {
        ReqwestAdapter {
            client: self,
            target,
            head_response: Arc::new(async_lock::Mutex::new(None)),
            before_request: Arc::new(None),
        }
    }
}

impl From<reqwest::Error> for StreamError {
    fn from(e: reqwest::Error) -> Self {
        if let Some(status_code) = e.status() {
            if status_code.is_client_error() {
                match status_code {
                    reqwest::StatusCode::NOT_FOUND => {
                        return UnretryableError::Other(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            e.to_string(),
                        ))
                        .into()
                    }
                    reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::UNAUTHORIZED => {
                        return UnretryableError::Other(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            e.to_string(),
                        ))
                        .into()
                    }
                    _ => {
                        return RetryableError::Other(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            e.to_string(),
                        ))
                        .into()
                    }
                }
            }
        }
        if e.is_builder() || e.is_body() {
            return UnretryableError::Other(std::io::Error::new(
                std::io::ErrorKind::Other,
                e.to_string(),
            ))
            .into();
        }

        // fallback to other errors
        RetryableError::Other(std::io::Error::new(
            std::io::ErrorKind::Other,
            e.to_string(),
        ))
        .into()
    }
}

impl ReqwestAdapter {
    async fn perform_head(&self) -> Result<reqwest::Response, StreamError> {
        let response = self
            .apply_before_request(self.client.head(self.target.1.clone()))
            .send()
            .await?;
        Ok(response.error_for_status()?)
    }

    pub fn before_request(
        &mut self,
        f: impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder + Send + Sync + 'static,
    ) {
        self.before_request = Arc::new(Some(Box::new(f)));
    }

    #[inline]
    fn apply_before_request(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(f) = self.before_request.as_ref() {
            f(builder)
        } else {
            builder
        }
    }

    fn get_content_size(&self, response: &reqwest::Response) -> std::io::Result<u64> {
        Ok(response
            .content_length()
            .and_then(|len| if len > 0 { Some(len) } else { None })
            // fallback to just parse the CONTENT_LENGTH header
            .or_else(|| {
                response
                    .headers()
                    .get(CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse().ok())
            })
            .unwrap_or_default())
    }

    async fn suggest_filename(&self, response: &reqwest::Response) -> Option<String> {
        response
            .headers()
            .get(CONTENT_DISPOSITION)
            .and_then(|v| crate::utils::http::ContentDisposition::from_raw(v).ok())
            .and_then(|d| d.get_filename().map(String::from))
    }
}

struct ReqwestStream(AnyStream<Result<bytes::Bytes, reqwest::Error>>);

impl Stream for ReqwestStream {
    type Item = Result<bytes::Bytes, super::StreamError>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        Pin::new(&mut self.get_mut().0)
            .poll_next(cx)
            .map(|opt| opt.map(|res| res.map_err(StreamError::from)))
    }
}

#[async_trait]
impl BoltLoadAdapter for ReqwestAdapter {
    type Item = Result<bytes::Bytes, StreamError>;
    type Stream = AnyBytesStream;

    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, UnretryableError> {
        let mut response = self.head_response.lock().await;
        if response.is_none() {
            match self.perform_head().await {
                Ok(res) => *response = Some(res),
                Err(e) => match e {
                    StreamError::Retryable(e) => {
                        return Err(UnretryableError::Other(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            e.to_string(),
                        )));
                    }
                    StreamError::Unretryable(e) => {
                        return Err(e);
                    }
                },
            }
        }

        Ok(BoltLoadAdapterMeta {
            content_size: self.get_content_size(response.as_ref().unwrap())?,
            filename: self.suggest_filename(response.as_ref().unwrap()).await,
        })
    }

    async fn is_range_stream_available(&self) -> bool {
        let mut response = self.head_response.lock().await;
        if response.is_none() {
            match self.perform_head().await {
                Ok(res) => *response = Some(res),
                Err(_) => return false,
            }
        }
        // check Accept-Ranges header
        let mut is_range_supported = response
            .as_ref()
            .unwrap()
            .headers()
            .get(ACCEPT_RANGES)
            .is_some_and(|v| v == "bytes");
        // try to send a real range request to test
        if !is_range_supported {
            is_range_supported = self
                .apply_before_request(
                    self.client
                        .request(self.target.0.clone(), self.target.1.clone())
                        .header(RANGE, "bytes=0-8"),
                )
                .send()
                .await
                .and_then(|res| res.error_for_status())
                .map(|res| {
                    res.headers().get(CONTENT_RANGE).is_some()
                        && res.content_length().unwrap_or(0) > 1
                })
                .unwrap_or_default();
        }
        is_range_supported
    }

    async fn full_stream(&self) -> Result<Self::Stream, StreamError> {
        let response = self
            .apply_before_request(
                self.client
                    .request(self.target.0.clone(), self.target.1.clone()),
            )
            .send()
            .await?
            .error_for_status()?;
        let stream = ReqwestStream(Box::pin(response.bytes_stream()));
        Ok(Box::pin(stream))
    }

    async fn range_stream(&self, start: u64, end: u64) -> Result<Self::Stream, StreamError> {
        let response = self
            .apply_before_request(
                self.client
                    .request(self.target.0.clone(), self.target.1.clone())
                    .header(RANGE, format!("bytes={}-{}", start, end - 1)),
            )
            .send()
            .await?
            .error_for_status()?;
        let stream = ReqwestStream(Box::pin(response.bytes_stream()));
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use futures::StreamExt;
    use pretty_assertions::assert_eq;
    use test_log::test;

    #[test(tokio::test)]
    async fn test_get_content_size() {
        let (port, _) = super::super::tests::create_http_server().await.unwrap();
        let url = Url::parse(&format!("http://localhost:{}/no_range", port)).unwrap();
        let client = reqwest::Client::new();
        let adapter = client
            .clone()
            .into_reqwest_adapter((reqwest::Method::GET, url));
        assert_eq!(adapter.retrieve_meta().await.unwrap().content_size, 1040384);

        let url = Url::parse(&format!("http://localhost:{}/range", port)).unwrap();
        let adapter = client.into_reqwest_adapter((reqwest::Method::GET, url));
        assert_eq!(adapter.retrieve_meta().await.unwrap().content_size, 1040384);
    }

    #[test(tokio::test)]
    async fn test_suggest_filename() {
        let (port, _) = super::super::tests::create_http_server().await.unwrap();
        let url = Url::parse(&format!("http://localhost:{}/no_range", port)).unwrap();
        let client = reqwest::Client::new();
        let adapter = client.into_reqwest_adapter((reqwest::Method::GET, url));
        assert_eq!(
            adapter.retrieve_meta().await.unwrap().filename,
            Some("test.txt".to_owned())
        );
    }

    #[test(tokio::test)]
    async fn test_is_range_stream_available() {
        let (port, _) = super::super::tests::create_http_server().await.unwrap();
        let url = Url::parse(&format!("http://localhost:{}/range", port)).unwrap();
        let client = reqwest::Client::new();
        let adapter = client.into_reqwest_adapter((reqwest::Method::GET, url));
        assert!(
            adapter.is_range_stream_available().await,
            "range stream should be available"
        );

        let url = Url::parse(&format!("http://localhost:{}/no_range", port)).unwrap();
        let client = reqwest::Client::new();
        let adapter = client.into_reqwest_adapter((reqwest::Method::GET, url));
        assert!(
            !adapter.is_range_stream_available().await,
            "range stream should not be available"
        );
    }

    #[test(tokio::test)]
    async fn test_full_stream() {
        let (port, _) = super::super::tests::create_http_server().await.unwrap();
        let url = Url::parse(&format!("http://localhost:{}/no_range", port)).unwrap();
        let client = reqwest::Client::new();
        let adapter = client.into_reqwest_adapter((reqwest::Method::GET, url));
        let mut stream = adapter.full_stream().await.unwrap();
        let mut bytes = bytes::BytesMut::new();
        while let Some(item) = stream.next().await {
            bytes.extend_from_slice(&item.unwrap());
        }
        assert_eq!(bytes.len(), 1040384);
    }

    #[test(tokio::test)]
    async fn test_range_stream() {
        let (port, _) = super::super::tests::create_http_server().await.unwrap();
        let url = Url::parse(&format!("http://localhost:{}/range", port)).unwrap();
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
