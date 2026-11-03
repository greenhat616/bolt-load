use std::{
    error::Error as _,
    io::ErrorKind,
    ops::Range,
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime},
};

use async_once_cell::OnceCell;
use bolt_load_utils::http::{ContentDisposition, parse_retry_after};
use futures::Stream;
use reqwest::header::{
    ACCEPT_ENCODING, ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_ENCODING, CONTENT_LENGTH,
    CONTENT_RANGE, CONTENT_TYPE, HeaderName, RANGE, RETRY_AFTER,
};
use snafu::IntoError;
use url::Url;

use super::{
    AnyBytesStream, AnyStream, BoltLoadAdapter, BoltLoadAdapterMeta, ContentRange, RangeHeaders,
    check_content_coding, classify_status,
    error::{
        AdapterError, UnretryableError, retryable::IoSnafu as RetryableIoSnafu,
        unretryable::IoSnafu as UnretryableIoSnafu,
    },
    is_identity_coding, limit_range_body, validate_range_response,
};

type BeforeRequestFn =
    Arc<dyn Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder + Send + Sync>;

struct Context {
    pub is_range_stream_available: bool,
    pub meta: BoltLoadAdapterMeta,
}

#[derive(Clone)]
struct Target(reqwest::Method, Url);

#[derive(Debug, Clone, Copy)]
enum ContentSize {
    ContentRange(u64),
    ContentLength(u64),
    Unknown,
}

impl ContentSize {
    fn to_u64(self) -> Option<u64> {
        match self {
            ContentSize::ContentRange(size) => Some(size),
            ContentSize::ContentLength(size) => Some(size),
            ContentSize::Unknown => None,
        }
    }
}

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

#[derive(Debug, snafu::Snafu)]
#[snafu(transparent)]
pub struct ReqwestError {
    source: reqwest::Error,
}

impl From<ReqwestError> for AdapterError {
    fn from(ReqwestError { source: e }: ReqwestError) -> Self {
        // Responses go through `check_status`; this only catches a status
        // error whose headers are already gone.
        if let Some(status) = e.status() {
            return classify_status(status.as_u16(), None);
        }
        if is_undecodable(&e) {
            return UnretryableError::BodyUndecodable {
                source: Arc::new(std::io::Error::new(ErrorKind::InvalidData, e.to_string())),
            }
            .into();
        }
        if e.is_builder() || e.is_body() {
            return UnretryableIoSnafu {}
                .into_error(Arc::new(std::io::Error::other(e.to_string())))
                .into();
        }

        // fallback to other errors
        RetryableIoSnafu {}
            .into_error(Arc::new(std::io::Error::other(e.to_string())))
            .into()
    }
}

fn header<'a>(response: &'a reqwest::Response, name: &HeaderName) -> Option<&'a str> {
    response.headers().get(name).and_then(|v| v.to_str().ok())
}

/// Every line of a field, combined as [`RangeHeaders`] expects.
fn combined(response: &reqwest::Response, name: &HeaderName) -> Option<String> {
    let values: Vec<_> = response
        .headers()
        .get_all(name)
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()))
        .collect();
    (!values.is_empty()).then(|| values.join(", "))
}

/// The combined `Content-Encoding`, unless it is `identity`.
fn content_coding(response: &reqwest::Response) -> Option<String> {
    combined(response, &CONTENT_ENCODING).filter(|value| !is_identity_coding(value))
}

fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    header(response, &RETRY_AFTER).and_then(|v| parse_retry_after(v, SystemTime::now()))
}

/// Classify an error status. Unlike `error_for_status`, this still sees the
/// headers, so `Retry-After` reaches [`classify_status`].
fn check_status(response: reqwest::Response) -> Result<reqwest::Response, AdapterError> {
    let status = response.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return Ok(response);
    }
    Err(classify_status(status.as_u16(), retry_after(&response)))
}

/// Check that a range response carries `requested`, and return its range.
fn check_range(
    response: &reqwest::Response,
    requested: Range<u64>,
    expected_total: Option<u64>,
) -> Result<ContentRange, AdapterError> {
    let content_range = combined(response, &CONTENT_RANGE);
    let content_type = combined(response, &CONTENT_TYPE);
    let content_encoding = combined(response, &CONTENT_ENCODING);
    let headers = RangeHeaders::default()
        .with_content_range(content_range.as_deref())
        .with_content_type(content_type.as_deref())
        .with_content_encoding(content_encoding.as_deref())
        .with_retry_after(retry_after(response));
    validate_range_response(
        response.status().as_u16(),
        &headers,
        requested,
        expected_total,
    )
}

impl ReqwestAdapter {
    async fn perform_head(&self) -> Result<reqwest::Response, AdapterError> {
        let response = self
            .apply_before_request(
                self.client
                    .head(self.target.1.clone())
                    .header(ACCEPT_ENCODING, "identity"),
            )
            .send()
            .await
            .map_err(ReqwestError::from)?;
        check_status(response)
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

    fn get_content_size(response: &reqwest::Response) -> ContentSize {
        // First, try to parse the `Content-Range` header
        let content_size: Option<u64> = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split_once('/'))
            .and_then(|(_, end)| end.parse().ok());
        // Then, try to parse the `Content-Length` header
        match content_size {
            Some(size) if size > 0 => ContentSize::ContentRange(size),
            _ => {
                let content_length: Option<u64> = response
                    .content_length()
                    .filter(|&len| len > 0)
                    // fallback to just parse the CONTENT_LENGTH header
                    .or_else(|| {
                        response
                            .headers()
                            .get(CONTENT_LENGTH)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| s.parse().ok())
                    });
                match content_length {
                    Some(size) => ContentSize::ContentLength(size),
                    None => ContentSize::Unknown,
                }
            }
        }
    }

    async fn suggest_filename(response: &reqwest::Response) -> Option<String> {
        response
            .headers()
            .get(CONTENT_DISPOSITION)
            .and_then(|v| ContentDisposition::from_raw(v).ok())
            .and_then(|d| d.get_filename().map(String::from))
    }

    async fn get_context(&self) -> Result<&Context, AdapterError> {
        self.context
            .get_or_try_init(fetch_remote_meta(self))
            .await
            .inspect_err(|e| {
                bolt_load_utils::telemetry::error!("failed to get context: {}", e);
            })
    }
}

/// Whether the body bytes themselves could not be decoded, as opposed to a
/// connection that broke while they were decoded.
///
/// A client built with a decoder reports every body error as a decode error,
/// plain bodies included. The source chain tells the two apart: a broken
/// connection carries a `hyper::Error`, a decoder failure only the decoder's
/// `io::Error` (`InvalidData` for a bad header, `UnexpectedEof` for a stream
/// that stops early). Retrying cannot fix the latter.
fn is_undecodable(e: &reqwest::Error) -> bool {
    if !e.is_decode() || e.is_timeout() {
        return false;
    }
    let mut undecodable = false;
    let mut source = e.source();
    while let Some(error) = source {
        if error.is::<hyper::Error>() {
            return false;
        }
        if let Some(io) = error.downcast_ref::<std::io::Error>() {
            undecodable |= matches!(io.kind(), ErrorKind::InvalidData | ErrorKind::UnexpectedEof);
        }
        source = error.source();
    }
    undecodable
}

struct ReqwestStream<'a>(AnyStream<'a, Result<bytes::Bytes, reqwest::Error>>);

impl<'a> Stream for ReqwestStream<'a> {
    type Item = Result<bytes::Bytes, AdapterError>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        Pin::new(&mut self.get_mut().0)
            .poll_next(cx)
            .map(|opt| opt.map(|res| res.map_err(ReqwestError::from).map_err(AdapterError::from)))
    }
}

async fn fetch_remote_meta(adapter: &ReqwestAdapter) -> Result<Context, AdapterError> {
    // Firstly, try to perform a `RANGE` GET request to test if the remote source supports range requests
    let Target(method, url) = adapter.target.clone();
    let response = adapter
        .apply_before_request(
            adapter
                .client
                .request(method.clone(), url.clone())
                .header(RANGE, "bytes=0-0")
                .header(ACCEPT_ENCODING, "identity"),
        )
        .send()
        .await
        .map_err(ReqwestError::from)?;

    // Only a 206 that proves its offsets shows range support; a coded one
    // does not.
    let probed = check_range(&response, 0..1, None).ok();
    let probe_coding = content_coding(&response);
    let mut is_range_stream_available = probed.is_some();
    if let Some(size) = probed.and_then(|range| range.complete) {
        return Ok(Context {
            is_range_stream_available,
            meta: BoltLoadAdapterMeta::new(size)
                .with_filename(ReqwestAdapter::suggest_filename(&response).await),
        });
    }

    // Try to perform a `HEAD` request to get the meta
    let response = adapter.perform_head().await?;
    // A weak signal: every range response is still checked.
    if !is_range_stream_available
        && header(&response, &ACCEPT_RANGES).is_some_and(|v| v.trim().eq_ignore_ascii_case("bytes"))
    {
        is_range_stream_available = true;
    }

    return Ok(Context {
        is_range_stream_available,
        meta: BoltLoadAdapterMeta::new(
            ReqwestAdapter::get_content_size(&response)
                .to_u64()
                .unwrap_or_default(),
        )
        .with_filename(ReqwestAdapter::suggest_filename(&response).await)
        .with_content_encoding(content_coding(&response).or(probe_coding)),
    });
}

impl BoltLoadAdapter for ReqwestAdapter {
    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError> {
        let context = self.get_context().await?;

        Ok(context.meta.clone())
    }

    async fn is_range_stream_available(&self) -> bool {
        match self.get_context().await {
            Ok(context) => context.is_range_stream_available,
            Err(_) => false,
        }
    }

    async fn full_stream(&self) -> Result<AnyBytesStream, AdapterError> {
        let response = self
            .apply_before_request(
                self.client
                    .request(self.target.0.clone(), self.target.1.clone())
                    .header(ACCEPT_ENCODING, "identity"),
            )
            .send()
            .await
            .map_err(ReqwestError::from)?;
        let response = check_status(response)?;
        // A decoding client has dropped the header of what it decodes.
        check_content_coding(
            combined(&response, &CONTENT_ENCODING).as_deref(),
            combined(&response, &CONTENT_TYPE).as_deref(),
        )?;
        let stream = ReqwestStream(Box::pin(response.bytes_stream()));
        Ok(Box::pin(stream))
    }

    async fn range_stream(&self, start: u64, end: u64) -> Result<AnyBytesStream, AdapterError> {
        let response = self
            .apply_before_request(
                self.client
                    .request(self.target.0.clone(), self.target.1.clone())
                    .header(RANGE, format!("bytes={}-{}", start, end - 1))
                    .header(ACCEPT_ENCODING, "identity"),
            )
            .send()
            .await
            .map_err(ReqwestError::from)?;
        let expected_total = self
            .context
            .get()
            .map(|context| context.meta.content_size)
            .filter(|&size| size > 0);
        let got = check_range(&response, start..end, expected_total)?;
        let stream = ReqwestStream(Box::pin(response.bytes_stream()));
        Ok(limit_range_body(Box::pin(stream), got))
    }
}

#[cfg(test)]
mod test {
    use bolt_load_tests::adapter::http_server;
    use futures::StreamExt;
    use pretty_assertions::assert_eq;

    use super::*;

    fn local_client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    #[tokio::test]
    async fn test_get_content_size() {
        let content_size: u64 = 1024 * 1024;
        let (port, _) = http_server::create_http_server_with_file_size(content_size as usize)
            .await
            .unwrap();
        let url = Url::parse(&format!("http://localhost:{port}/no_range")).unwrap();
        let client = local_client();
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
        let client = local_client();
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
        let client = local_client();
        let adapter = client.into_reqwest_adapter((reqwest::Method::GET, url));
        assert!(
            adapter.is_range_stream_available().await,
            "range stream should be available"
        );

        let url = Url::parse(&format!("http://localhost:{port}/no_range")).unwrap();
        let client = local_client();
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
        let client = local_client();
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
        let client = local_client();
        let adapter = client.into_reqwest_adapter((reqwest::Method::GET, url));
        let mut stream = adapter.range_stream(0, 100).await.unwrap();
        let mut bytes = bytes::BytesMut::new();
        while let Some(item) = stream.next().await {
            bytes.extend_from_slice(&item.unwrap());
        }
        assert_eq!(bytes.len(), 100);
    }
}
