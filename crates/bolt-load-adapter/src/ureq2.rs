use std::{
    convert::AsRef,
    ops::Range,
    sync::Arc,
    time::{Duration, SystemTime},
};

use blocking::unblock;
use bolt_load_core::adapter::{
    UnretryableError, error::unretryable::IoSnafu as UnretryableIoSnafu,
};
use bolt_load_http::{
    RangeHeaders, check_content_coding, classify_status,
    header::{ContentDisposition, parse_retry_after},
    limit_range_body, validate_range_response,
};
use bolt_load_utils::reader::CrossRuntimeStream;
use futures::Stream;
use snafu::IntoError;
use ureq2::{Agent, Request};

use super::{
    AdapterError, AnyBytesStream, AnyStream, BoltLoadAdapter, BoltLoadAdapterMeta, ContentRange,
    RetryableError,
};

type BeforeRequestFn = Box<dyn Fn(Request) -> Request + Send + Sync>;
type CallFn = Box<dyn Fn(Request) -> Result<ureq2::Response, ureq2::Error> + Send + Sync>;

#[non_exhaustive]
pub struct UreqAdapter {
    agent: Agent,
    target: (String, url::Url),
    head_response: Arc<async_lock::Mutex<Option<ureq2::Response>>>,
    before_request: Option<BeforeRequestFn>,
    call: Arc<Option<CallFn>>,
}

pub trait IntoUreqAdapter {
    fn into_ureq_adapter(self, target: (impl AsRef<str>, url::Url)) -> UreqAdapter;
}

impl IntoUreqAdapter for ureq2::Agent {
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

#[derive(Debug, snafu::Snafu)]
#[snafu(transparent)]
pub struct UreqError {
    source: ureq2::Error,
}

impl From<UreqError> for AdapterError {
    fn from(UreqError { source: e }: UreqError) -> Self {
        match e {
            ureq2::Error::Transport(_) => RetryableError::Io {
                source: Arc::new(std::io::Error::new(
                    std::io::ErrorKind::NetworkDown,
                    e.to_string(),
                )),
            }
            .into(),
            ureq2::Error::Status(status, response) => {
                classify_status(status, retry_after(&response))
            }
        }
    }
}

fn retry_after(response: &ureq2::Response) -> Option<Duration> {
    response
        .header(http::header::RETRY_AFTER.as_str())
        .and_then(|v| parse_retry_after(v, SystemTime::now()))
}

/// Every line of a field, combined as [`RangeHeaders`] expects.
fn combined(response: &ureq2::Response, name: &http::HeaderName) -> Option<String> {
    let values = response.all(name.as_str());
    (!values.is_empty()).then(|| values.join(", "))
}

/// Check that a range response carries `requested`, and return its range.
fn check_range(
    response: &ureq2::Response,
    requested: Range<u64>,
    expected_total: Option<u64>,
) -> Result<ContentRange, AdapterError> {
    let content_range = combined(response, &http::header::CONTENT_RANGE);
    let content_type = combined(response, &http::header::CONTENT_TYPE);
    let content_encoding = combined(response, &http::header::CONTENT_ENCODING);
    let headers = RangeHeaders::default()
        .with_content_range(content_range.as_deref())
        .with_content_type(content_type.as_deref())
        .with_content_encoding(content_encoding.as_deref())
        .with_retry_after(retry_after(response));
    validate_range_response(response.status(), &headers, requested, expected_total)
}

/// The response, also for an error status, so its headers can be checked.
#[expect(
    clippy::result_large_err,
    reason = "the error type is returned by ureq"
)]
fn any_status(
    result: Result<ureq2::Response, ureq2::Error>,
) -> Result<ureq2::Response, ureq2::Error> {
    match result {
        Err(ureq2::Error::Status(_, response)) => Ok(response),
        result => result,
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
        f: impl Fn(Request) -> Result<ureq2::Response, ureq2::Error> + Send + Sync + 'static,
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

    #[expect(
        clippy::result_large_err,
        reason = "the error type is returned by ureq"
    )]
    fn apply_call(
        call: Arc<Option<CallFn>>,
        builder: Request,
    ) -> Result<ureq2::Response, ureq2::Error> {
        if let Some(f) = call.as_ref() {
            f(builder)
        } else {
            builder.call()
        }
    }

    /// Run `f` on the HEAD response, sent once and shared by the meta, the
    /// range check and range detection.
    async fn with_head<T>(&self, f: impl FnOnce(&ureq2::Response) -> T) -> Result<T, AdapterError> {
        let mut head = self.head_response.lock().await;
        let response = match head.take() {
            Some(response) => response,
            None => self.perform_head().await?,
        };
        let result = f(&response);
        *head = Some(response);
        Ok(result)
    }

    async fn perform_head(&self) -> Result<ureq2::Response, AdapterError> {
        let request = self.apply_before_request(
            self.agent
                .head(self.target.1.as_str())
                .set(http::header::ACCEPT_ENCODING.as_str(), "identity"),
        );
        let call = self.call.clone();
        unblock(move || Self::apply_call(call, request).map_err(UreqError::from))
            .await
            .map_err(AdapterError::from)
    }

    fn get_content_size(&self, response: &ureq2::Response) -> u64 {
        let content_length = response.header("content-length");
        content_length
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    }

    fn suggest_filename(&self, response: &ureq2::Response) -> Option<String> {
        response
            .header("content-disposition")
            .and_then(|v| http::HeaderValue::from_str(v).ok())
            .and_then(|v| ContentDisposition::from_raw(&v).ok())
            .and_then(|d| d.get_filename().map(String::from))
    }
}

const BUFFER_SIZE: usize = 1024 * 1024;

struct UreqStream(AnyStream<'static, Result<bytes::Bytes, std::io::Error>>);

/// Classify an error reading the body. A connection lost mid-body is
/// retryable, as it is on reqwest; a body that cannot be decoded is not.
fn body_error(e: std::io::Error) -> AdapterError {
    use std::io::ErrorKind;

    let source = Arc::new(e);
    match source.kind() {
        ErrorKind::UnexpectedEof
        | ErrorKind::ConnectionReset
        | ErrorKind::ConnectionAborted
        | ErrorKind::BrokenPipe
        | ErrorKind::TimedOut
        | ErrorKind::Interrupted
        | ErrorKind::NetworkDown => RetryableError::Io { source }.into(),
        ErrorKind::InvalidData => UnretryableError::BodyUndecodable { source }.into(),
        _ => UnretryableIoSnafu {}.into_error(source).into(),
    }
}

impl Stream for UreqStream {
    type Item = Result<bytes::Bytes, AdapterError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.get_mut().0)
            .poll_next(cx)
            .map_err(body_error)
    }
}

impl BoltLoadAdapter for UreqAdapter {
    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError> {
        self.with_head(|response| {
            BoltLoadAdapterMeta::new(self.get_content_size(response))
                .with_filename(self.suggest_filename(response))
                .with_content_encoding(combined(response, &http::header::CONTENT_ENCODING))
        })
        .await
    }

    async fn full_stream(&self) -> Result<AnyBytesStream, AdapterError> {
        let request = self.apply_before_request(
            self.agent
                .request_url(&self.target.0, &self.target.1)
                .set(http::header::ACCEPT_ENCODING.as_str(), "identity"),
        );
        let call = self.call.clone();
        let res = unblock(move || Self::apply_call(call, request).map_err(UreqError::from))
            .await
            .map_err(AdapterError::from)?;
        check_content_coding(
            combined(&res, &http::header::CONTENT_ENCODING).as_deref(),
            combined(&res, &http::header::CONTENT_TYPE).as_deref(),
        )?;
        let reader = res.into_reader();
        let stream = CrossRuntimeStream::new(reader, BUFFER_SIZE);
        let ureq_stream = UreqStream(Box::pin(stream));
        Ok(Box::pin(ureq_stream))
    }

    async fn range_stream(&self, start: u64, end: u64) -> Result<AnyBytesStream, AdapterError> {
        let request = self.apply_before_request(
            self.agent
                .request_url(&self.target.0, &self.target.1)
                .set(
                    http::header::RANGE.as_str(),
                    &format!("bytes={}-{}", start, end - 1),
                )
                .set(http::header::ACCEPT_ENCODING.as_str(), "identity"),
        );
        let call = self.call.clone();
        let res =
            unblock(move || any_status(Self::apply_call(call, request)).map_err(UreqError::from))
                .await
                .map_err(AdapterError::from)?;
        let expected_total = self
            .head_response
            .lock()
            .await
            .as_ref()
            .map(|head| self.get_content_size(head))
            .filter(|&size| size > 0);
        let got = check_range(&res, start..end, expected_total)?;
        let reader = res.into_reader();
        let stream = CrossRuntimeStream::new(reader, BUFFER_SIZE);
        let ureq_stream = UreqStream(Box::pin(stream));
        Ok(limit_range_body(Box::pin(ureq_stream), got))
    }

    async fn is_range_stream_available(&self) -> bool {
        // A weak signal: every range response is still checked.
        let accepts_ranges = self
            .with_head(|response| {
                response
                    .header(http::header::ACCEPT_RANGES.as_str())
                    .is_some_and(|v| v.trim().eq_ignore_ascii_case("bytes"))
            })
            .await;
        let Ok(mut is_range_supported) = accepts_ranges else {
            return false;
        };
        // Otherwise only a 206 that proves its offsets shows range support.
        if !is_range_supported {
            let request = self.apply_before_request(
                self.agent
                    .request_url(&self.target.0, &self.target.1)
                    .set(http::header::RANGE.as_str(), "bytes=0-0")
                    .set(http::header::ACCEPT_ENCODING.as_str(), "identity"),
            );
            let call = self.call.clone();
            is_range_supported = unblock(move || Self::apply_call(call, request))
                .await
                .is_ok_and(|res| check_range(&res, 0..1, None).is_ok());
        }
        is_range_supported
    }
}

#[cfg(test)]
mod test {
    #[test]
    fn a_lost_connection_is_retryable_and_a_bad_body_is_not() {
        use std::io::{Error, ErrorKind};

        for kind in [
            ErrorKind::UnexpectedEof,
            ErrorKind::ConnectionReset,
            ErrorKind::TimedOut,
        ] {
            assert!(
                matches!(
                    body_error(Error::from(kind)),
                    AdapterError::Retryable { .. }
                ),
                "{kind:?}"
            );
        }
        assert!(matches!(
            body_error(Error::from(ErrorKind::InvalidData)),
            AdapterError::Unretryable {
                source: UnretryableError::BodyUndecodable { .. }
            }
        ));
        assert!(matches!(
            body_error(Error::from(ErrorKind::InvalidInput)),
            AdapterError::Unretryable {
                source: UnretryableError::Io { .. }
            }
        ));
    }

    use bolt_load_tests::adapter::http_server::{self, DEFAULT_FILE_SIZE};
    use futures::StreamExt;
    use pretty_assertions::assert_eq;
    use url::Url;

    use super::*;

    #[tokio::test]
    async fn test_get_content_size() {
        let (port, _) = http_server::create_http_server().await.unwrap();
        let url = Url::parse(&format!("http://localhost:{}/no_range", port)).unwrap();
        let agent = ureq2::Agent::new();
        let adapter = agent.clone().into_ureq_adapter(("get", url));
        assert_eq!(
            adapter.retrieve_meta().await.unwrap().content_size,
            DEFAULT_FILE_SIZE as u64
        );

        let url = Url::parse(&format!("http://localhost:{}/range", port)).unwrap();
        let adapter = agent.clone().into_ureq_adapter(("get", url));
        assert_eq!(
            adapter.retrieve_meta().await.unwrap().content_size,
            DEFAULT_FILE_SIZE as u64
        );
    }

    #[tokio::test]
    async fn test_suggest_filename() {
        let (port, _) = http_server::create_http_server().await.unwrap();
        let url = Url::parse(&format!("http://localhost:{}/no_range", port)).unwrap();
        let agent = ureq2::Agent::new();
        let adapter = agent.clone().into_ureq_adapter(("get", url));
        assert_eq!(
            adapter.retrieve_meta().await.unwrap().filename,
            Some("test.txt".to_owned())
        );
    }

    #[tokio::test]
    async fn test_is_range_stream_available() {
        let (port, _) = http_server::create_http_server().await.unwrap();
        let url = Url::parse(&format!("http://localhost:{}/range", port)).unwrap();
        let agent = ureq2::Agent::new();
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

    #[tokio::test]
    async fn test_full_stream() {
        let (port, _) = http_server::create_http_server().await.unwrap();
        let url = Url::parse(&format!("http://localhost:{}/no_range", port)).unwrap();
        let agent = ureq2::Agent::new();
        let adapter = agent.clone().into_ureq_adapter(("get", url));
        let mut stream = adapter.full_stream().await.unwrap();
        let mut bytes = bytes::BytesMut::new();
        while let Some(item) = stream.next().await {
            bytes.extend_from_slice(&item.unwrap());
        }
        assert_eq!(bytes.len(), DEFAULT_FILE_SIZE);
    }

    #[tokio::test]
    async fn test_range_stream() {
        let (port, _) = http_server::create_http_server().await.unwrap();
        let url = Url::parse(&format!("http://localhost:{}/range", port)).unwrap();
        let agent = ureq2::Agent::new();
        let adapter = agent.clone().into_ureq_adapter(("get", url));
        let mut stream = adapter.range_stream(0, 100).await.unwrap();
        let mut bytes = bytes::BytesMut::new();
        while let Some(item) = stream.next().await {
            bytes.extend_from_slice(&item.unwrap());
        }
        assert_eq!(bytes.len(), 100);
    }
}
