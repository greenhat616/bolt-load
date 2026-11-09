use std::{
    convert::AsRef,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

use blocking::unblock;
use bolt_load_core::adapter::{
    UnretryableError, error::unretryable::IoSnafu as UnretryableIoSnafu,
};
use bolt_load_http::{
    RangeHeaders, classify_status,
    header::{ContentDisposition, parse_retry_after},
    if_range_validator, is_strong_etag, limit_range_body, validate_range_response,
};
use bolt_load_utils::reader::CrossRuntimeStream;
use futures::Stream;
use snafu::IntoError;
use ureq2::{Agent, Request};

use super::{
    AdapterError, AnyBytesStream, AnyStream, BoltLoadAdapter, BoltLoadAdapterMeta, ContentRange,
    FullResponse, RangeExpectation, RetryableError, Validator,
};

type BeforeRequestFn = Box<dyn Fn(Request) -> Request + Send + Sync>;
type CallFn = Box<dyn Fn(Request) -> Result<ureq2::Response, ureq2::Error> + Send + Sync>;

#[non_exhaustive]
pub struct UreqAdapter {
    agent: Agent,
    target: (String, url::Url),
    probed: Arc<async_lock::Mutex<Option<Arc<Probed>>>>,
    /// What was probed is out of date: [`BoltLoadAdapter::refresh`] was
    /// called since.
    stale: Arc<AtomicBool>,
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
            probed: Arc::new(async_lock::Mutex::new(None)),
            stale: Arc::default(),
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

/// The value of a field that occurs once, such as `ETag`: `None` when it is
/// missing or repeated.
fn single<'a>(response: &'a ureq2::Response, name: &http::HeaderName) -> Option<&'a str> {
    match response.all(name.as_str())[..] {
        [value] => Some(value),
        _ => None,
    }
}

/// `meta` with the validators of `response`, a response to a GET: its
/// strong `ETag`, its `Last-Modified`, and the one for `If-Range`. A
/// repeated field counts as missing.
fn with_validators(meta: BoltLoadAdapterMeta, response: &ureq2::Response) -> BoltLoadAdapterMeta {
    let etag = single(response, &http::header::ETAG);
    let last_modified = single(response, &http::header::LAST_MODIFIED);
    let date = single(response, &http::header::DATE);
    let validator = if_range_validator(etag, last_modified, date);
    meta.with_etag(
        etag.filter(|etag| is_strong_etag(etag))
            .map(|etag| etag.trim().to_owned()),
    )
    .with_last_modified(last_modified.map(|value| value.trim().to_owned()))
    .with_validator(validator)
}

/// What the adapter learned about the resource, from a HEAD and an
/// unconditional `bytes=0-0` GET.
///
/// The validators and the total come from the GET, as the range requests
/// that are checked against them: some servers send other validators with
/// a HEAD. The HEAD still gives the filename, the coding, and the size
/// when the GET does not show the total.
struct Probed {
    meta: BoltLoadAdapterMeta,
    /// The HEAD says `Accept-Ranges: bytes`, or the GET was a 206 that
    /// proves its offsets.
    is_range_stream_available: bool,
}

/// Check that a range response carries `requested`, and return its range;
/// `if_range` is what the request sent in `If-Range`.
fn check_range(
    response: &ureq2::Response,
    requested: Range<u64>,
    expected_total: Option<u64>,
    if_range: Option<&Validator>,
) -> Result<ContentRange, AdapterError> {
    let content_range = combined(response, &http::header::CONTENT_RANGE);
    let content_type = combined(response, &http::header::CONTENT_TYPE);
    let content_encoding = combined(response, &http::header::CONTENT_ENCODING);
    let headers = RangeHeaders::default()
        .with_content_range(content_range.as_deref())
        .with_content_type(content_type.as_deref())
        .with_content_encoding(content_encoding.as_deref())
        .with_retry_after(retry_after(response))
        .with_if_range(if_range)
        .with_etag(single(response, &http::header::ETAG))
        .with_last_modified(single(response, &http::header::LAST_MODIFIED));
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

    /// What was probed since the last refresh, probed once and shared by
    /// the meta and range detection.
    async fn probed(&self) -> Result<Arc<Probed>, AdapterError> {
        let mut probed = self.probed.lock().await;
        if self.stale.swap(false, Ordering::SeqCst) {
            probed.take();
        }
        if let Some(probed) = probed.as_ref() {
            return Ok(probed.clone());
        }
        let fresh = Arc::new(self.probe().await?);
        *probed = Some(fresh.clone());
        Ok(fresh)
    }

    /// Send the HEAD, which must succeed, and the `bytes=0-0` GET, which
    /// may fail.
    async fn probe(&self) -> Result<Probed, AdapterError> {
        let head = self.perform_head().await?;
        let request = self.apply_before_request(
            self.agent
                .request_url(&self.target.0, &self.target.1)
                .set(http::header::RANGE.as_str(), "bytes=0-0")
                .set(http::header::ACCEPT_ENCODING.as_str(), "identity"),
        );
        let call = self.call.clone();
        let get = unblock(move || Self::apply_call(call, request))
            .await
            .ok()
            .filter(|get| (200..300).contains(&get.status()));
        // Only a 206 that proves its offsets shows range support.
        let probed = get
            .as_ref()
            .and_then(|get| check_range(get, 0..1, None, None).ok());
        let accepts_ranges = head
            .header(http::header::ACCEPT_RANGES.as_str())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("bytes"));
        let size = probed
            .and_then(|range| range.complete)
            .unwrap_or_else(|| self.get_content_size(&head));
        let meta = BoltLoadAdapterMeta::new(size)
            .with_filename(self.suggest_filename(&head))
            .with_content_encoding(combined(&head, &http::header::CONTENT_ENCODING));
        Ok(Probed {
            meta: match &get {
                Some(get) => with_validators(meta, get),
                None => meta,
            },
            // A weak signal: every range response is still checked.
            is_range_stream_available: accepts_ranges || probed.is_some(),
        })
    }

    /// Request `start..end`, sending `if_range` in `If-Range`, and check the
    /// response against it and against `expected_total`.
    async fn request_range(
        &self,
        start: u64,
        end: u64,
        expected_total: Option<u64>,
        if_range: Option<&Validator>,
    ) -> Result<AnyBytesStream, AdapterError> {
        let mut request = self
            .agent
            .request_url(&self.target.0, &self.target.1)
            .set(
                http::header::RANGE.as_str(),
                &format!("bytes={}-{}", start, end - 1),
            )
            .set(http::header::ACCEPT_ENCODING.as_str(), "identity");
        if let Some(validator) = if_range {
            request = request.set(http::header::IF_RANGE.as_str(), validator.as_str());
        }
        let request = self.apply_before_request(request);
        let call = self.call.clone();
        let res =
            unblock(move || any_status(Self::apply_call(call, request)).map_err(UreqError::from))
                .await
                .map_err(AdapterError::from)?;
        let got = check_range(&res, start..end, expected_total, if_range)?;
        let reader = res.into_reader();
        let stream = CrossRuntimeStream::new(reader, BUFFER_SIZE);
        let ureq_stream = UreqStream(Box::pin(stream));
        Ok(limit_range_body(Box::pin(ureq_stream), got))
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
        Ok(self.probed().await?.meta.clone())
    }

    async fn full_stream(&self) -> Result<FullResponse, AdapterError> {
        let request = self.apply_before_request(
            self.agent
                .request_url(&self.target.0, &self.target.1)
                .set(http::header::ACCEPT_ENCODING.as_str(), "identity"),
        );
        let call = self.call.clone();
        let res = unblock(move || Self::apply_call(call, request).map_err(UreqError::from))
            .await
            .map_err(AdapterError::from)?;
        let content_encoding = combined(&res, &http::header::CONTENT_ENCODING);
        let content_type = combined(&res, &http::header::CONTENT_TYPE);
        let reader = res.into_reader();
        let stream = CrossRuntimeStream::new(reader, BUFFER_SIZE);
        let ureq_stream = UreqStream(Box::pin(stream));
        Ok(FullResponse::new(Box::pin(ureq_stream))
            .with_content_encoding(content_encoding)
            .with_content_type(content_type))
    }

    /// A range without `If-Range`, checked against the size probed since
    /// the last refresh, if any.
    async fn range_stream(&self, start: u64, end: u64) -> Result<AnyBytesStream, AdapterError> {
        let expected_total = {
            let probed = self.probed.lock().await;
            match probed.as_ref() {
                Some(probed) if !self.stale.load(Ordering::SeqCst) => {
                    Some(probed.meta.content_size).filter(|&size| size > 0)
                }
                _ => None,
            }
        };
        self.request_range(start, end, expected_total, None).await
    }

    /// A range of the representation `expected` describes, whatever the
    /// adapter probed since.
    async fn range_stream_if(
        &self,
        start: u64,
        end: u64,
        expected: &RangeExpectation,
    ) -> Result<AnyBytesStream, AdapterError> {
        let expected_total = Some(expected.total).filter(|&size| size > 0);
        self.request_range(start, end, expected_total, expected.validator.as_ref())
            .await
    }

    async fn is_range_stream_available(&self) -> bool {
        self.probed()
            .await
            .is_ok_and(|probed| probed.is_range_stream_available)
    }

    fn refresh(&self) {
        self.stale.store(true, Ordering::SeqCst);
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

    /// The metadata of a response with `headers`.
    fn validators_of(headers: &[(&str, &str)]) -> BoltLoadAdapterMeta {
        let mut response = String::from("HTTP/1.1 200 OK\r\n");
        for (name, value) in headers {
            response.push_str(&format!("{name}: {value}\r\n"));
        }
        response.push_str("\r\n");
        with_validators(BoltLoadAdapterMeta::new(1), &response.parse().unwrap())
    }

    const OLD: &str = "Sun, 08 Nov 2026 12:00:09 GMT";
    const DATE: &str = "Sun, 08 Nov 2026 12:00:10 GMT";

    /// The validators of a response, `None` for a field that is missing or
    /// repeated: a `Last-Modified` without `Date`, or with a repeated one,
    /// is no validator.
    #[test]
    fn validators_come_from_single_fields() {
        let date = |value: &str| Some(Validator::LastModified(value.into()));
        let meta = validators_of(&[("last-modified", OLD)]);
        assert_eq!(meta.last_modified.as_deref(), Some(OLD));
        assert_eq!(meta.validator, None);
        assert_eq!(
            validators_of(&[("last-modified", OLD), ("date", DATE)]).validator,
            date(OLD)
        );
        assert_eq!(
            validators_of(&[("last-modified", OLD), ("date", DATE), ("date", DATE)]).validator,
            None
        );
        let meta = validators_of(&[("last-modified", OLD), ("last-modified", OLD), ("date", DATE)]);
        assert_eq!((meta.last_modified, meta.validator), (None, None));
        let meta = validators_of(&[
            ("etag", "\"a\""),
            ("etag", "\"a\""),
            ("last-modified", OLD),
            ("date", DATE),
        ]);
        assert_eq!((meta.etag, meta.validator), (None, date(OLD)));
        let meta = validators_of(&[("etag", "\"a\", \"b\""), ("last-modified", OLD), ("date", DATE)]);
        assert_eq!((meta.etag, meta.validator), (None, date(OLD)));
        let meta = validators_of(&[("etag", "W/\"a\""), ("last-modified", OLD), ("date", DATE)]);
        assert_eq!((meta.etag, meta.validator), (None, None));
        let meta = validators_of(&[("etag", "\"a\""), ("last-modified", OLD)]);
        assert_eq!(meta.etag.as_deref(), Some("\"a\""));
        assert_eq!(meta.validator, Some(Validator::StrongEtag("\"a\"".into())));
    }

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
        let mut stream = adapter.full_stream().await.unwrap().body;
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
