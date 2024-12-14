use crate::utils::{http::ContentDisposition, reader::CrossRuntimeStream};
use blocking::unblock;
use futures::Stream;
use std::convert::AsRef;
use std::sync::Arc;
use ureq::{Agent, Request};

use super::{
    AnyBytesStream, AnyStream, BoltLoadAdapter, BoltLoadAdapterMeta, RetryableError, StreamError,
    UnretryableError,
};

type BeforeRequestFn = Box<dyn Fn(Request) -> Request + Send + Sync>;
type CallFn = Box<dyn Fn(Request) -> Result<ureq::Response, ureq::Error> + Send + Sync>;

#[non_exhaustive]
pub struct UreqAdapter {
    agent: Agent,
    target: (String, url::Url),
    head_response: Arc<async_lock::Mutex<Option<ureq::Response>>>,
    before_request: Option<BeforeRequestFn>,
    call: Arc<Option<CallFn>>,
}

pub trait IntoUreqAdapter {
    fn into_ureq_adapter(self, target: (impl AsRef<str>, url::Url)) -> UreqAdapter;
}

impl IntoUreqAdapter for ureq::Agent {
    fn into_ureq_adapter(self, target: (impl AsRef<str>, url::Url)) -> UreqAdapter {
        let method = target.0.as_ref().to_ascii_uppercase();
        UreqAdapter {
            agent: self,
            target: (method, target.1),
            head_response: Arc::new(async_lock::Mutex::new(None)),
            before_request: None,
            call: Arc::new(None),
        }
    }
}

impl From<ureq::Error> for StreamError {
    fn from(e: ureq::Error) -> Self {
        match e {
            ureq::Error::Transport(_) => StreamError::Retryable(RetryableError::Other(
                std::io::Error::new(std::io::ErrorKind::NetworkDown, e.to_string()),
            )),
            ureq::Error::Status(404, _) => StreamError::Unretryable(UnretryableError::NotFound),
            ureq::Error::Status(401, _) | ureq::Error::Status(403, _) => StreamError::Unretryable(
                UnretryableError::Unauthorized(format!("http status code: {}", 401)),
            ),
            ureq::Error::Status(status, _) if status < 500 => {
                StreamError::Unretryable(UnretryableError::Other(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("http client error, status code: {}", status),
                )))
            }
            ureq::Error::Status(status, _) => {
                StreamError::Retryable(RetryableError::Other(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("http status code: {}", status),
                )))
            }
        }
    }
}

impl UreqAdapter {
    pub fn before_request(&mut self, f: impl Fn(Request) -> Request + Send + Sync + 'static) {
        self.before_request = Some(Box::new(f));
    }

    /// Set the ureq send request related function
    ///
    /// For example, you can pass a closure like `|req| req.send_bytes(b"hello )` to send request
    pub fn call(
        &mut self,
        f: impl Fn(Request) -> Result<ureq::Response, ureq::Error> + Send + Sync + 'static,
    ) {
        self.call = Arc::new(Some(Box::new(f)));
    }

    fn apply_before_request(&self, builder: Request) -> Request {
        if let Some(f) = self.before_request.as_ref() {
            f(builder)
        } else {
            builder
        }
    }

    #[allow(clippy::result_large_err)] // The error type if returned by ureq, we can't do anything about it
    fn apply_call(
        call: Arc<Option<CallFn>>,
        builder: Request,
    ) -> Result<ureq::Response, ureq::Error> {
        if let Some(f) = call.as_ref() {
            f(builder)
        } else {
            builder.call()
        }
    }

    async fn perform_head(&self) -> Result<ureq::Response, StreamError> {
        let request = self.apply_before_request(self.agent.head(self.target.1.as_str()));
        let call = self.call.clone();
        unblock(move || Self::apply_call(call, request))
            .await
            .map_err(StreamError::from)
    }

    fn get_content_size(&self, response: &ureq::Response) -> u64 {
        let content_length = response.header("content-length");
        content_length
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    }

    fn suggest_filename(&self, response: &ureq::Response) -> Option<String> {
        response
            .header("content-disposition")
            .and_then(|v| http::HeaderValue::from_str(v).ok())
            .and_then(|v| ContentDisposition::from_raw(&v).ok())
            .and_then(|d| d.get_filename().map(String::from))
    }
}

const BUFFER_SIZE: usize = 1024 * 1024;

struct UreqStream(AnyStream<Result<bytes::Bytes, std::io::Error>>);

impl Stream for UreqStream {
    type Item = Result<bytes::Bytes, StreamError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.get_mut().0)
            .poll_next(cx)
            .map_err(|e| UnretryableError::Other(e).into())
    }
}

#[async_trait::async_trait]
impl BoltLoadAdapter for UreqAdapter {
    type Item = Result<bytes::Bytes, StreamError>;
    type Stream = AnyBytesStream;

    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, UnretryableError> {
        let response = self.perform_head().await?;
        let content_size = self.get_content_size(&response);
        let filename = self.suggest_filename(&response);
        Ok(BoltLoadAdapterMeta {
            content_size,
            filename,
        })
    }

    async fn full_stream(&self) -> Result<Self::Stream, StreamError> {
        let request =
            self.apply_before_request(self.agent.request_url(&self.target.0, &self.target.1));
        let call = self.call.clone();
        let res = unblock(move || Self::apply_call(call, request))
            .await
            .map_err(StreamError::from)?;
        let reader = res.into_reader();
        let stream = CrossRuntimeStream::new(reader, BUFFER_SIZE);
        let ureq_stream = UreqStream(Box::pin(stream));
        Ok(Box::pin(ureq_stream))
    }

    async fn range_stream(&self, start: u64, end: u64) -> Result<Self::Stream, StreamError> {
        let request =
            self.apply_before_request(self.agent.request_url(&self.target.0, &self.target.1).set(
                http::header::RANGE.as_str(),
                &format!("bytes={}-{}", start, end - 1),
            ));
        let call = self.call.clone();
        let res = unblock(move || Self::apply_call(call, request))
            .await
            .map_err(StreamError::from)?;
        let reader = res.into_reader();
        let stream = CrossRuntimeStream::new(reader, BUFFER_SIZE);
        let ureq_stream = UreqStream(Box::pin(stream));
        Ok(Box::pin(ureq_stream))
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
            .header(http::header::ACCEPT_RANGES.as_str())
            .is_some_and(|v| v == "bytes");
        // try to send a real range request to test
        if !is_range_supported {
            let request = self.apply_before_request(
                self.agent
                    .request_url(&self.target.0, &self.target.1)
                    .set(http::header::RANGE.as_str(), "bytes=0-8"),
            );
            let call = self.call.clone();
            is_range_supported = unblock(move || Self::apply_call(call, request))
                .await
                .map(|res| {
                    res.header(http::header::CONTENT_RANGE.as_str()).is_some()
                        && res
                            .header(http::header::CONTENT_LENGTH.as_str())
                            .is_some_and(|v| v.parse::<u64>().unwrap_or(0) > 1)
                })
                .unwrap_or_default();
        }
        is_range_supported
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use futures::StreamExt;
    use pretty_assertions::assert_eq;
    use test_log::test;
    use url::Url;

    #[test(tokio::test)]
    async fn test_get_content_size() {
        let (port, _) = super::super::tests::create_http_server().await.unwrap();
        let url = Url::parse(&format!("http://localhost:{}/no_range", port)).unwrap();
        let agent = ureq::Agent::new();
        let adapter = agent.clone().into_ureq_adapter(("get", url));
        assert_eq!(adapter.retrieve_meta().await.unwrap().content_size, 1040384);

        let url = Url::parse(&format!("http://localhost:{}/range", port)).unwrap();
        let adapter = agent.clone().into_ureq_adapter(("get", url));
        assert_eq!(adapter.retrieve_meta().await.unwrap().content_size, 1040384);
    }

    #[test(tokio::test)]
    async fn test_suggest_filename() {
        let (port, _) = super::super::tests::create_http_server().await.unwrap();
        let url = Url::parse(&format!("http://localhost:{}/no_range", port)).unwrap();
        let agent = ureq::Agent::new();
        let adapter = agent.clone().into_ureq_adapter(("get", url));
        assert_eq!(
            adapter.retrieve_meta().await.unwrap().filename,
            Some("test.txt".to_owned())
        );
    }

    #[test(tokio::test)]
    async fn test_is_range_stream_available() {
        let (port, _) = super::super::tests::create_http_server().await.unwrap();
        let url = Url::parse(&format!("http://localhost:{}/range", port)).unwrap();
        let agent = ureq::Agent::new();
        let adapter = agent.clone().into_ureq_adapter(("get", url));
        assert!(
            adapter.is_range_stream_available().await,
            "range stream should be available"
        );

        let url = Url::parse(&format!("http://localhost:{}/no_range", port)).unwrap();
        let adapter = agent.clone().into_ureq_adapter(("get", url));
        assert!(
            !adapter.is_range_stream_available().await,
            "range stream should not be available"
        );
    }

    #[test(tokio::test)]
    async fn test_full_stream() {
        let (port, _) = super::super::tests::create_http_server().await.unwrap();
        let url = Url::parse(&format!("http://localhost:{}/no_range", port)).unwrap();
        let agent = ureq::Agent::new();
        let adapter = agent.clone().into_ureq_adapter(("get", url));
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
        let agent = ureq::Agent::new();
        let adapter = agent.clone().into_ureq_adapter(("get", url));
        let mut stream = adapter.range_stream(0, 100).await.unwrap();
        let mut bytes = bytes::BytesMut::new();
        while let Some(item) = stream.next().await {
            bytes.extend_from_slice(&item.unwrap());
        }
        assert_eq!(bytes.len(), 100);
    }
}
