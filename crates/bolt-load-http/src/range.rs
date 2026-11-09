//! Whether a range response carries the requested bytes.
//!
//! Like [`classify_status`], this is part of the adapter contract: the task
//! writes a range stream at the requested offset, so an HTTP adapter should
//! check every range response with [`validate_range_response`] before
//! handing out its body.

use std::{
    ops::Range,
    pin::Pin,
    task::{Context, Poll, ready},
    time::Duration,
};

use bolt_load_core::adapter::{
    AdapterError, AnyBytesStream, ContentRange, UnretryableError, Validator,
};
use futures::{Stream, StreamExt};

use crate::{ContentCoding, classify_status, conflicts_with, has_validator};

/// The response headers [`validate_range_response`] reads.
///
/// Pass each field as its combined value: repeated field lines joined with
/// `", "` in order (RFC 9110 §5.3). A repeated `Content-Range` then fails to
/// parse, and every `Content-Encoding` line is checked. Keep a value that is
/// not valid UTF-8 (lossily), so it cannot pass as absent.
///
/// `ETag` and `Last-Modified` are the exception: pass `None` for one that
/// is missing, repeated or not UTF-8, so the response has no such validator
/// (a repeated one combined does not parse either, which comes to the
/// same).
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct RangeHeaders<'a> {
    pub content_range: Option<&'a str>,
    pub content_type: Option<&'a str>,
    pub content_encoding: Option<&'a str>,
    /// The parsed `Retry-After` delay, passed on to [`classify_status`].
    pub retry_after: Option<Duration>,
    /// The validator the request sent in `If-Range`, if it sent one: the
    /// one the task expects, see
    /// [`BoltLoadAdapter::range_stream_if`](bolt_load_core::adapter::BoltLoadAdapter::range_stream_if).
    pub if_range: Option<&'a Validator>,
    /// The response's `ETag`, unless it is repeated.
    pub etag: Option<&'a str>,
    /// The response's `Last-Modified`, unless it is repeated.
    pub last_modified: Option<&'a str>,
}

impl<'a> RangeHeaders<'a> {
    pub fn with_content_range(mut self, value: Option<&'a str>) -> Self {
        self.content_range = value;
        self
    }

    pub fn with_content_type(mut self, value: Option<&'a str>) -> Self {
        self.content_type = value;
        self
    }

    pub fn with_content_encoding(mut self, value: Option<&'a str>) -> Self {
        self.content_encoding = value;
        self
    }

    pub fn with_retry_after(mut self, value: Option<Duration>) -> Self {
        self.retry_after = value;
        self
    }

    pub fn with_if_range(mut self, value: Option<&'a Validator>) -> Self {
        self.if_range = value;
        self
    }

    pub fn with_etag(mut self, value: Option<&'a str>) -> Self {
        self.etag = value;
        self
    }

    pub fn with_last_modified(mut self, value: Option<&'a str>) -> Self {
        self.last_modified = value;
        self
    }
}

/// Check a response to a single-part range request for `requested`
/// (half-open), and return the range it carries.
///
/// A 206 passes when its `Content-Range` starts at `requested.start`, ends
/// before `requested.end`, and names `expected_total` (if both are known),
/// and the body is neither multipart nor content-encoded. A 206 that ends
/// early is valid: the stream ends short and the rest is requested again.
///
/// | Response | Error |
/// | --- | --- |
/// | 206 to a request with `If-Range`, with a validator that conflicts with the one sent | [`UnretryableError::ResourceChanged`] |
/// | 206 that would pass but is content-coded | [`UnretryableError::RangeContentCoded`] |
/// | 206 that fails the checks otherwise | [`UnretryableError::RangeMismatch`] |
/// | 200 to a request with `If-Range`, with another validator than it sent, or none | [`UnretryableError::ResourceChanged`] |
/// | other 200 | [`UnretryableError::RangeIgnored`] |
/// | 416 | [`UnretryableError::RangeNotSatisfiable`], with the total from `bytes */total` |
/// | other 4xx, 5xx | [`classify_status`] |
/// | anything else | [`UnretryableError::Status`] |
///
/// A 206 without a validator, or with one that does not parse, passes: a
/// server need not repeat it. One of the other kind, such as an `ETag` when
/// the request sent a date, is not compared. See
/// [`conflicts_with`](crate::conflicts_with).
///
/// Headers cannot show a body the HTTP client has already decoded: the
/// client drops `Content-Encoding`. Wrap the body in [`limit_range_body`]
/// so such a body fails instead of being written.
pub fn validate_range_response(
    status: u16,
    headers: &RangeHeaders<'_>,
    requested: Range<u64>,
    expected_total: Option<u64>,
) -> Result<ContentRange, AdapterError> {
    match status {
        // A server that ignored `If-Range`, or a cache, may send a part of
        // another representation; its own validator shows it.
        206 if headers
            .if_range
            .is_some_and(|sent| conflicts_with(sent, headers.etag, headers.last_modified)) =>
        {
            Err(UnretryableError::ResourceChanged.into())
        }
        206 => check_partial(headers, requested, expected_total),
        // A server that ignores ranges sends the representation it holds
        // with its validator: the one sent, unless the resource changed.
        200 => Err(match headers.if_range {
            Some(sent) if !has_validator(sent, headers.etag, headers.last_modified) => {
                UnretryableError::ResourceChanged
            }
            _ => UnretryableError::RangeIgnored,
        }
        .into()),
        416 => Err(UnretryableError::RangeNotSatisfiable {
            total: headers
                .content_range
                .and_then(ContentRange::unsatisfied_complete),
        }
        .into()),
        400..=599 => Err(classify_status(status, headers.retry_after)),
        _ => Err(UnretryableError::Status { status }.into()),
    }
}

fn check_partial(
    headers: &RangeHeaders<'_>,
    requested: Range<u64>,
    expected_total: Option<u64>,
) -> Result<ContentRange, AdapterError> {
    let got = headers
        .content_range
        .and_then(|value| value.parse::<ContentRange>().ok());
    let multipart = headers.content_type.is_some_and(is_multipart);
    match got {
        Some(got)
            if !multipart
                && got.first == requested.start
                && got.last < requested.end
                && match (got.complete, expected_total) {
                    (Some(complete), Some(total)) => complete == total,
                    _ => true,
                } =>
        {
            // `Content-Range` of an encoded body counts encoded bytes.
            match headers
                .content_encoding
                .filter(|coding| !ContentCoding::parse_list(coding).is_empty())
            {
                None => Ok(got),
                Some(coding) => Err(UnretryableError::RangeContentCoded {
                    coding: coding.to_owned(),
                }
                .into()),
            }
        }
        _ => Err(UnretryableError::RangeMismatch { requested, got }.into()),
    }
}

fn is_multipart(content_type: &str) -> bool {
    content_type.split(',').any(|value| {
        let media_type = value.split(';').next().unwrap_or_default().trim();
        media_type.eq_ignore_ascii_case("multipart/byteranges")
    })
}

/// Hold `body` to exactly the bytes `got` covers.
///
/// - Once it carries more, fail with [`UnretryableError::RangeBodyTooLong`],
///   without passing on the chunk that crosses.
/// - If it ends cleanly with fewer, fail with
///   [`UnretryableError::RangeBodyTooShort`].
///
/// A client that decodes a content-coded range body and drops
/// `Content-Encoding` yields a body of another length: longer when the data
/// compresses, shorter when it does not. A `Content-Range` that ends before
/// the requested end is still fine; the task asks for the rest. Errors from
/// `body` pass unchanged, so a cut-off connection stays retryable.
pub fn limit_range_body(body: AnyBytesStream, got: ContentRange) -> AnyBytesStream {
    Box::pin(RangeBody {
        body: Some(body),
        got,
        remaining: got.last - got.first + 1,
    })
}

struct RangeBody {
    /// `None` once the body has failed the check.
    body: Option<AnyBytesStream>,
    got: ContentRange,
    remaining: u64,
}

impl Stream for RangeBody {
    type Item = Result<bytes::Bytes, AdapterError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Some(body) = self.body.as_mut() else {
            return Poll::Ready(None);
        };
        let item = ready!(body.poll_next_unpin(cx));
        let got = self.got;
        match &item {
            Some(Ok(chunk)) => match self.remaining.checked_sub(chunk.len() as u64) {
                Some(remaining) => self.remaining = remaining,
                None => {
                    self.body = None;
                    return Poll::Ready(Some(Err(
                        UnretryableError::RangeBodyTooLong { got }.into()
                    )));
                }
            },
            None if self.remaining > 0 => {
                self.body = None;
                let received = got.last - got.first + 1 - self.remaining;
                return Poll::Ready(Some(Err(UnretryableError::RangeBodyTooShort {
                    got,
                    received,
                }
                .into())));
            }
            _ => {}
        }
        Poll::Ready(item)
    }
}

#[cfg(test)]
mod tests {
    use bolt_load_core::adapter::RetryableError;

    use super::*;

    const TOTAL: u64 = 100;

    fn headers(content_range: &str) -> RangeHeaders<'_> {
        RangeHeaders::default().with_content_range(Some(content_range))
    }

    fn validate(status: u16, headers: RangeHeaders<'_>) -> Result<ContentRange, AdapterError> {
        validate_range_response(status, &headers, 10..20, Some(TOTAL))
    }

    fn mismatch(result: Result<ContentRange, AdapterError>) -> Option<Option<ContentRange>> {
        match result {
            Err(AdapterError::Unretryable {
                source: UnretryableError::RangeMismatch { requested, got },
            }) => {
                assert_eq!(requested, 10..20);
                Some(got)
            }
            _ => None,
        }
    }

    #[test]
    fn accepts_a_matching_partial_response() {
        assert_eq!(
            validate(206, headers("bytes 10-19/100")).ok(),
            Some(ContentRange {
                first: 10,
                last: 19,
                complete: Some(100),
            })
        );
        assert!(validate(206, headers("BYTES  10-19 / 100 ")).is_ok());
        assert!(validate(206, headers("bytes 10-19/*")).is_ok());
        // Ending early is a short read, not a mismatch.
        assert!(validate(206, headers("bytes 10-14/100")).is_ok());
        // Without a known total, any complete length passes.
        assert!(validate_range_response(206, &headers("bytes 10-19/500"), 10..20, None).is_ok());
        let identity = headers("bytes 10-19/100").with_content_encoding(Some(" Identity "));
        assert!(validate(206, identity).is_ok());
        let plain = headers("bytes 10-19/100").with_content_type(Some("application/octet-stream"));
        assert!(validate(206, plain).is_ok());
    }

    #[test]
    fn rejects_a_partial_response_with_other_bytes() {
        let got = |first, last, complete| {
            Some(Some(ContentRange {
                first,
                last,
                complete,
            }))
        };
        assert_eq!(
            mismatch(validate(206, headers("bytes 11-19/100"))),
            got(11, 19, Some(100))
        );
        assert_eq!(
            mismatch(validate(206, headers("bytes 10-20/100"))),
            got(10, 20, Some(100))
        );
        assert_eq!(
            mismatch(validate(206, headers("bytes 10-19/101"))),
            got(10, 19, Some(101))
        );
        assert_eq!(mismatch(validate(206, RangeHeaders::default())), Some(None));
        assert_eq!(mismatch(validate(206, headers("bytes 10-19"))), Some(None));
        assert_eq!(mismatch(validate(206, headers("bytes */100"))), Some(None));
    }

    fn content_coded(result: Result<ContentRange, AdapterError>) -> Option<String> {
        match result {
            Err(AdapterError::Unretryable {
                source: UnretryableError::RangeContentCoded { coding },
            }) => Some(coding),
            _ => None,
        }
    }

    #[test]
    fn rejects_a_multipart_or_encoded_partial_response() {
        let multipart = headers("bytes 10-19/100")
            .with_content_type(Some("Multipart/ByteRanges; boundary=THIS_STRING"));
        assert!(mismatch(validate(206, multipart)).is_some());
        for encoding in ["gzip", "GZIP", "identity, gzip"] {
            let encoded = headers("bytes 10-19/100").with_content_encoding(Some(encoding));
            assert_eq!(
                content_coded(validate(206, encoded)).as_deref(),
                Some(encoding)
            );
        }
        // Coded and with other bytes: the offsets are the problem to report.
        let shifted = headers("bytes 11-19/100").with_content_encoding(Some("gzip"));
        assert!(mismatch(validate(206, shifted)).is_some());
    }

    #[test]
    fn rejects_repeated_field_lines_that_hide_a_problem() {
        // Two `Content-Range` lines, combined.
        assert_eq!(
            mismatch(validate(206, headers("bytes 10-19/100, bytes 10-19/100"))),
            Some(None)
        );
        let multipart = headers("bytes 10-19/100")
            .with_content_type(Some("text/plain, multipart/byteranges; boundary=x"));
        assert!(mismatch(validate(206, multipart)).is_some());
        // A value that was not UTF-8, kept lossily.
        let garbled = headers("bytes 10-19/100").with_content_encoding(Some("\u{FFFD}"));
        assert!(content_coded(validate(206, garbled)).is_some());
    }

    fn body(chunks: &[&'static [u8]]) -> AnyBytesStream {
        let chunks: Vec<Result<bytes::Bytes, AdapterError>> = chunks
            .iter()
            .map(|chunk| Ok(bytes::Bytes::from_static(chunk)))
            .collect();
        Box::pin(futures::stream::iter(chunks))
    }

    fn drain(mut body: AnyBytesStream) -> (Vec<u8>, Option<AdapterError>) {
        futures::executor::block_on(async {
            let mut received = Vec::new();
            while let Some(item) = body.next().await {
                match item {
                    Ok(chunk) => received.extend_from_slice(&chunk),
                    Err(error) => {
                        assert!(body.next().await.is_none(), "the body ends after the error");
                        return (received, Some(error));
                    }
                }
            }
            (received, None)
        })
    }

    #[test]
    fn limits_a_body_to_its_content_range() {
        let got = ContentRange {
            first: 10,
            last: 19,
            complete: Some(100),
        };
        let (received, error) = drain(limit_range_body(body(&[b"01234", b"56789"]), got));
        assert_eq!((received.len(), error.is_none()), (10, true));
        // A body that ends before its own `Content-Range` is covered.
        let (received, error) = drain(limit_range_body(body(&[b"0123"]), got));
        assert_eq!(received, b"0123");
        assert!(matches!(
            error,
            Some(AdapterError::Unretryable {
                source: UnretryableError::RangeBodyTooShort { got: g, received: 4 }
            }) if g == got
        ));
        // The crossing chunk is not passed on.
        let (received, error) = drain(limit_range_body(body(&[b"01234", b"567890", b"x"]), got));
        assert_eq!(received, b"01234");
        assert!(matches!(
            error,
            Some(AdapterError::Unretryable {
                source: UnretryableError::RangeBodyTooLong { got: g }
            }) if g == got
        ));
    }

    /// A 200 to a request made conditional with `If-Range` shows a changed
    /// resource, unless it carries the validator sent: then the server
    /// ignores ranges, as without one.
    #[test]
    fn a_200_with_another_validator_is_a_changed_resource() {
        const SENT: &str = "\"v1\"";
        const DATE: &str = "Sun, 08 Nov 2026 12:00:09 GMT";
        let classify = |headers: RangeHeaders<'_>| match validate(200, headers) {
            Err(AdapterError::Unretryable { source }) => source,
            other => panic!("unexpected {other:?}"),
        };
        let strong = Validator::StrongEtag(SENT.into());
        let date = Validator::LastModified(DATE.into());
        let sent = RangeHeaders::default().with_if_range(Some(&strong));
        assert!(matches!(
            classify(sent.with_etag(Some(SENT))),
            UnretryableError::RangeIgnored
        ));
        for changed in [
            sent.with_etag(Some("\"v2\"")),
            sent.with_etag(Some("W/\"v1\"")),
            sent.with_last_modified(Some(DATE)),
            sent,
        ] {
            assert!(
                matches!(classify(changed), UnretryableError::ResourceChanged),
                "{changed:?}"
            );
        }
        let by_date = RangeHeaders::default().with_if_range(Some(&date));
        assert!(matches!(
            classify(by_date.with_last_modified(Some(DATE))),
            UnretryableError::RangeIgnored
        ));
        assert!(matches!(
            classify(by_date.with_etag(Some(SENT))),
            UnretryableError::ResourceChanged
        ));
        // Without `If-Range`, a 200 is an ignored range, whatever it carries.
        assert!(matches!(
            classify(RangeHeaders::default().with_etag(Some("\"v2\""))),
            UnretryableError::RangeIgnored
        ));
    }

    /// A 206 to a request made conditional with `If-Range` is part of
    /// another representation when it carries a validator that conflicts
    /// with the one sent; one it omits does not count.
    #[test]
    fn a_206_with_a_conflicting_validator_is_a_changed_resource() {
        const DATE: &str = "Sun, 08 Nov 2026 12:00:09 GMT";
        let changed = |result: Result<ContentRange, AdapterError>| {
            matches!(
                result,
                Err(AdapterError::Unretryable {
                    source: UnretryableError::ResourceChanged
                })
            )
        };
        let strong = Validator::StrongEtag("\"v1\"".into());
        let date = Validator::LastModified(DATE.into());
        let by_etag = headers("bytes 10-19/100").with_if_range(Some(&strong));
        let by_date = headers("bytes 10-19/100").with_if_range(Some(&date));

        // A conflicting entity tag.
        assert!(changed(validate(206, by_etag.with_etag(Some("\"v2\"")))));
        assert!(changed(validate(206, by_etag.with_etag(Some("W/\"v1\"")))));
        // Even when the part would not pass otherwise.
        assert!(changed(validate(
            206,
            by_etag
                .with_etag(Some("\"v2\""))
                .with_content_range(Some("bytes 10-19/200"))
        )));
        // A conflicting date.
        assert!(changed(validate(
            206,
            by_date.with_last_modified(Some("Sun, 08 Nov 2026 12:00:10 GMT"))
        )));
        // An absent validator, one that does not parse, and one of the other
        // kind pass.
        for passes in [
            by_etag,
            by_etag.with_etag(Some("\"v1\"")),
            by_etag.with_etag(Some("\"v1\", \"v2\"")),
            by_etag.with_last_modified(Some(DATE)),
            by_date,
            by_date.with_last_modified(Some(DATE)),
            by_date.with_last_modified(Some("Sun Nov  8 12:00:09 2026")),
            by_date.with_last_modified(Some("yesterday")),
            by_date.with_etag(Some("\"v2\"")),
        ] {
            assert_eq!(
                validate(206, passes).unwrap(),
                ContentRange {
                    first: 10,
                    last: 19,
                    complete: Some(TOTAL)
                },
                "{passes:?}"
            );
        }
        // Without `If-Range`, a validator is not compared.
        assert!(validate(206, headers("bytes 10-19/100").with_etag(Some("\"v2\""))).is_ok());
    }

    #[test]
    fn classifies_other_statuses() {
        assert!(matches!(
            validate(200, headers("bytes 10-19/100")),
            Err(AdapterError::Unretryable {
                source: UnretryableError::RangeIgnored
            })
        ));
        let not_satisfiable =
            |value| match validate(416, RangeHeaders::default().with_content_range(value)) {
                Err(AdapterError::Unretryable {
                    source: UnretryableError::RangeNotSatisfiable { total },
                }) => total,
                other => panic!("unexpected {other:?}"),
            };
        assert_eq!(not_satisfiable(Some("bytes */100")), Some(100));
        assert_eq!(not_satisfiable(Some("bytes */*")), None);
        assert_eq!(not_satisfiable(None), None);
        let retry_after = Some(Duration::from_secs(5));
        assert!(matches!(
            validate(503, RangeHeaders::default().with_retry_after(retry_after)),
            Err(AdapterError::Unretryable {
                source: UnretryableError::Congestion { status: 503, retry_after: r }
            }) if r == retry_after
        ));
        assert!(matches!(
            validate(500, RangeHeaders::default()),
            Err(AdapterError::Retryable {
                source: RetryableError::Status { status: 500 }
            })
        ));
        for status in [204, 304] {
            assert!(matches!(
                validate(status, RangeHeaders::default()),
                Err(AdapterError::Unretryable {
                    source: UnretryableError::Status { status: s }
                }) if s == status
            ));
        }
    }
}
