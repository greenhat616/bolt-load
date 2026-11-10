//! Both adapters make their range requests conditional on the validator
//! they retrieved with the metadata (design §11.2, §14.23): the strong
//! `ETag` of a GET, else a `Last-Modified` at least a second before
//! `Date`, never with a weak `ETag`. A 200 with another validator, or a 206
//! with a conflicting one, is a changed resource; a 200 with the same one,
//! an ignored range.

#![cfg(all(feature = "reqwest", feature = "ureq2"))]

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use bolt_load_adapter::{reqwest::IntoReqwestAdapter, ureq2::IntoUreqAdapter};
use bolt_load_core::adapter::{
    AdapterError, BoltLoadAdapter, DynBoltLoadAdapter, UnretryableError, Validator,
};
use bolt_load_tests::adapter::{
    fault::RangeFault,
    http_server::{RequestLog, create_range_fault_server_with_log},
};
use url::Url;

const SIZE: usize = 64 * 1024;

/// Each adapter, on a server of its own serving `fault`, with its log.
async fn adapters(
    fault: RangeFault,
) -> Vec<(
    &'static str,
    Box<DynBoltLoadAdapter<'static>>,
    RequestLog,
    tokio::task::JoinHandle<()>,
)> {
    let content: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();
    let mut adapters = Vec::new();
    for name in ["reqwest", "ureq2"] {
        let (port, server, log) = create_range_fault_server_with_log(content.clone().into(), None)
            .await
            .unwrap();
        let url = Url::parse(&format!("http://127.0.0.1:{port}{}", fault.path())).unwrap();
        let adapter: Box<DynBoltLoadAdapter<'static>> = match name {
            "reqwest" => DynBoltLoadAdapter::new_box(
                reqwest::Client::builder()
                    .no_proxy()
                    .build()
                    .unwrap()
                    .into_reqwest_adapter((reqwest::Method::GET, url)),
            ),
            _ => DynBoltLoadAdapter::new_box(ureq2::Agent::new().into_ureq_adapter(("get", url))),
        };
        adapters.push((name, adapter, log, server));
    }
    adapters
}

/// The `If-Range` of the last range request other than a probe.
fn last_if_range(log: &RequestLog) -> Option<String> {
    log.requests()
        .into_iter()
        .rev()
        .find(|request| request.range.is_some_and(|range| range != (0, 0)))
        .expect("a range request")
        .if_range
}

fn unretryable<T>(result: Result<T, AdapterError>) -> UnretryableError {
    match result {
        Err(AdapterError::Unretryable { source }) => source,
        Err(other) => panic!("unexpected {other:?}"),
        Ok(_) => panic!("unexpected success"),
    }
}

fn changed<T>(result: Result<T, AdapterError>) -> bool {
    matches!(unretryable(result), UnretryableError::ResourceChanged)
}

/// A strong `ETag` goes in `If-Range`. Once the resource changed, the 200
/// it gets is a changed resource.
#[tokio::test]
async fn a_strong_etag_makes_ranges_conditional() {
    for (name, adapter, log, server) in adapters(RangeFault::ChangedMidDownload).await {
        let meta = adapter.retrieve_meta().await.unwrap();
        assert_eq!(meta.etag.as_deref(), Some("\"v1\""), "{name}");
        assert!(meta.last_modified.is_some(), "{name}");
        // The first range gets half of itself, and the resource changes.
        assert!(adapter.range_stream(10, 20).await.is_ok(), "{name}");
        assert_eq!(last_if_range(&log).as_deref(), Some("\"v1\""), "{name}");
        assert!(changed(adapter.range_stream(20, 30).await), "{name}");
        assert_eq!(last_if_range(&log).as_deref(), Some("\"v1\""), "{name}");
        server.abort();
    }
}

/// A weak `ETag` is not kept, and no `If-Range` is sent: a date may stand
/// in only without an entity tag (RFC 9110 §13.1.5).
#[tokio::test]
async fn a_weak_etag_sends_no_validator() {
    for (name, adapter, log, server) in adapters(RangeFault::WeakEtag).await {
        let meta = adapter.retrieve_meta().await.unwrap();
        assert_eq!(meta.etag, None, "{name}");
        assert!(meta.last_modified.is_some(), "{name}");
        assert!(adapter.range_stream(10, 20).await.is_ok(), "{name}");
        assert_eq!(last_if_range(&log), None, "{name}");
        server.abort();
    }
}

/// A `Last-Modified` is a validator only when it is at least a second
/// before `Date`: not when equal, nor when later. Dates in the obsolete
/// formats count, are sent back as they came, and match a 206 that names
/// the same date in another format.
#[tokio::test]
async fn a_last_modified_a_second_before_date_is_sent() {
    for (fault, sent) in [
        (RangeFault::RecentLastModified, false),
        (RangeFault::FutureLastModified, false),
        (RangeFault::LastModifiedOneSecondOld, true),
        (RangeFault::ObsoleteDateFormats, true),
    ] {
        for (name, adapter, log, server) in adapters(fault).await {
            let meta = adapter.retrieve_meta().await.unwrap();
            let last_modified = meta.last_modified.clone().expect("a Last-Modified");
            assert_eq!(meta.etag, None, "{name} {fault:?}");
            if fault == RangeFault::ObsoleteDateFormats {
                assert!(last_modified.contains('-'), "{name}: {last_modified}");
            }
            assert!(
                adapter.range_stream(10, 20).await.is_ok(),
                "{name} {fault:?}"
            );
            assert_eq!(
                last_if_range(&log),
                sent.then_some(last_modified),
                "{name} {fault:?}"
            );
            server.abort();
        }
    }
}

/// A repeated `ETag` field, or one holding a list, is no entity tag: the
/// `Last-Modified` is the validator.
#[tokio::test]
async fn a_repeated_or_listed_etag_is_no_validator() {
    for fault in [RangeFault::RepeatedEtag, RangeFault::ListEtag] {
        for (name, adapter, log, server) in adapters(fault).await {
            let meta = adapter.retrieve_meta().await.unwrap();
            assert_eq!(meta.etag, None, "{name} {fault:?}");
            let last_modified = meta.last_modified.clone().expect("a Last-Modified");
            assert!(
                adapter.range_stream(10, 20).await.is_ok(),
                "{name} {fault:?}"
            );
            assert_eq!(last_if_range(&log), Some(last_modified), "{name} {fault:?}");
            server.abort();
        }
    }
}

/// A server that ignores ranges sends the whole resource with the
/// validator sent: an ignored range, not a changed resource. The
/// validator is that of a GET, even when a HEAD names another.
#[tokio::test]
async fn a_200_with_the_validator_sent_ignores_the_range() {
    for fault in [
        RangeFault::IgnoreRangeWithValidators,
        RangeFault::IgnoreRangeWithHeadValidators,
    ] {
        for (name, adapter, log, server) in adapters(fault).await {
            let meta = adapter.retrieve_meta().await.unwrap();
            assert_eq!(meta.etag.as_deref(), Some("\"v1\""), "{name} {fault:?}");
            assert_eq!(
                meta.validator,
                Some(Validator::StrongEtag("\"v1\"".into())),
                "{name} {fault:?}"
            );
            assert!(
                matches!(
                    unretryable(adapter.range_stream(10, 20).await),
                    UnretryableError::RangeIgnored
                ),
                "{name} {fault:?}"
            );
            assert_eq!(
                last_if_range(&log).as_deref(),
                Some("\"v1\""),
                "{name} {fault:?}"
            );
            server.abort();
        }
    }
}

/// The validator comes from a GET, even when the HEAD advertises ranges
/// and names another: ranges of a stable resource pass.
#[tokio::test]
async fn the_validator_of_a_get_is_sent_not_that_of_a_head() {
    for (name, adapter, log, server) in adapters(RangeFault::HeadValidatorsDiffer).await {
        let meta = adapter.retrieve_meta().await.unwrap();
        assert_eq!(meta.etag.as_deref(), Some("\"v1\""), "{name}");
        assert!(adapter.is_range_stream_available().await, "{name}");
        assert!(adapter.range_stream(10, 20).await.is_ok(), "{name}");
        assert_eq!(last_if_range(&log).as_deref(), Some("\"v1\""), "{name}");
        server.abort();
    }
}

/// A 206 with a validator that conflicts with the one sent is a part of
/// another version, by `ETag` or by date. One without validators passes.
#[tokio::test]
async fn a_206_with_a_conflicting_validator_is_a_changed_resource() {
    for (fault, sent_etag) in [
        (RangeFault::ChangedIgnoringIfRange, true),
        (RangeFault::ChangedByDateIgnoringIfRange, false),
    ] {
        for (name, adapter, log, server) in adapters(fault).await {
            let meta = adapter.retrieve_meta().await.unwrap();
            assert_eq!(meta.etag.is_some(), sent_etag, "{name} {fault:?}");
            // The first range gets half of itself, and the resource changes.
            assert!(
                adapter.range_stream(10, 20).await.is_ok(),
                "{name} {fault:?}"
            );
            assert!(last_if_range(&log).is_some(), "{name} {fault:?}");
            assert!(
                changed(adapter.range_stream(20, 30).await),
                "{name} {fault:?}"
            );
            server.abort();
        }
    }
    for (name, adapter, log, server) in adapters(RangeFault::PartsWithoutValidators).await {
        adapter.retrieve_meta().await.unwrap();
        assert!(adapter.range_stream(10, 20).await.is_ok(), "{name}");
        assert_eq!(last_if_range(&log).as_deref(), Some("\"v1\""), "{name}");
        server.abort();
    }
}

/// The size comes from the validated 206 of the probe, not from a HEAD
/// that names another, and ranges are checked against it.
#[tokio::test]
async fn the_total_comes_from_the_validated_206() {
    for (name, adapter, log, server) in adapters(RangeFault::HeadSizeDiffers).await {
        let meta = adapter.retrieve_meta().await.unwrap();
        assert_eq!(meta.content_size, SIZE as u64, "{name}");
        let end = SIZE as u64;
        assert!(adapter.range_stream(end - 10, end).await.is_ok(), "{name}");
        assert_eq!(last_if_range(&log).as_deref(), Some("\"v1\""), "{name}");
        server.abort();
    }
}

/// A rejected probe leaves the adapter without validators: its ranges are
/// not conditional, for the adapter's whole life.
#[tokio::test]
async fn a_rejected_probe_leaves_no_validator() {
    for (name, adapter, log, server) in adapters(RangeFault::RejectedProbe).await {
        let meta = adapter.retrieve_meta().await.unwrap();
        assert_eq!(meta.etag, None, "{name}");
        assert_eq!(meta.validator, None, "{name}");
        assert_eq!(meta.content_size, SIZE as u64, "{name}");
        assert!(adapter.is_range_stream_available().await, "{name}");
        assert!(adapter.range_stream(10, 20).await.is_ok(), "{name}");
        assert_eq!(last_if_range(&log), None, "{name}");
        server.abort();
    }
}

/// The meta names the validator that the range requests send in
/// `If-Range`, kind and value, and `None` when they send none (design
/// §11.5.7): a strong `ETag` of the GET, even when the HEAD names another;
/// a `Last-Modified` a second before `Date`, in the format it came in; none
/// with a weak `ETag`, a recent or future `Last-Modified`, or a rejected
/// probe.
#[tokio::test]
async fn the_meta_names_the_validator_sent() {
    let etag = |value: &str| Some(Validator::StrongEtag(value.to_owned()));
    let date = |value: &str| Some(Validator::LastModified(value.to_owned()));
    for (fault, expected) in [
        (RangeFault::HeadValidatorsDiffer, etag("\"v1\"")),
        (
            RangeFault::LastModifiedOneSecondOld,
            date("Fri, 15 Jan 2027 07:59:59 GMT"),
        ),
        (
            RangeFault::ObsoleteDateFormats,
            date("Tuesday, 14-Nov-23 22:13:21 GMT"),
        ),
        (RangeFault::WeakEtag, None),
        (RangeFault::RecentLastModified, None),
        (RangeFault::FutureLastModified, None),
        (RangeFault::RejectedProbe, None),
    ] {
        for (name, adapter, log, server) in adapters(fault).await {
            let meta = adapter.retrieve_meta().await.unwrap();
            assert_eq!(meta.validator, expected, "{name} {fault:?}");
            for (start, end) in [(10, 20), (20, 30)] {
                assert!(
                    adapter.range_stream(start, end).await.is_ok(),
                    "{name} {fault:?}"
                );
                assert_eq!(
                    last_if_range(&log).as_deref(),
                    meta.validator.as_ref().map(Validator::as_str),
                    "{name} {fault:?}"
                );
            }
            server.abort();
        }
    }
}

/// The server of `fault`, with its log, and the resource it serves.
async fn server(fault: RangeFault) -> (Url, RequestLog, tokio::task::JoinHandle<()>) {
    let content: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();
    let (port, server, log) = create_range_fault_server_with_log(content.into(), None)
        .await
        .unwrap();
    let url = Url::parse(&format!("http://127.0.0.1:{port}{}", fault.path())).unwrap();
    (url, log, server)
}

/// How many `bytes=0-0` probes the server answered.
fn probes(log: &RequestLog) -> usize {
    log.requests()
        .into_iter()
        .filter(|request| request.range == Some((0, 0)))
        .count()
}

/// A port that accepts connections and closes them at once, without an
/// answer: every request sent there fails at the transport level.
async fn closing_port() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });
    (port, task)
}

/// A reqwest adapter cloned before the probe shares the probe: the clone
/// reports the same `meta.validator` and sends the same `If-Range`, and the
/// resource is probed once. `UreqAdapter` is not `Clone`; it is shared
/// through an `Arc`, which is one instance.
#[tokio::test]
async fn a_cloned_adapter_shares_the_validator() {
    let (url, log, server) = server(RangeFault::HeadValidatorsDiffer).await;
    let original = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .into_reqwest_adapter((reqwest::Method::GET, url));
    let clone = original.clone();
    let meta = original.retrieve_meta().await.unwrap();
    assert_eq!(meta.validator, Some(Validator::StrongEtag("\"v1\"".into())));
    assert!(clone.range_stream(10, 20).await.is_ok());
    assert_eq!(last_if_range(&log).as_deref(), Some("\"v1\""));
    assert_eq!(
        clone.retrieve_meta().await.unwrap().validator,
        meta.validator
    );
    assert!(original.range_stream(20, 30).await.is_ok());
    assert_eq!(last_if_range(&log).as_deref(), Some("\"v1\""));
    assert_eq!(probes(&log), 1);
    server.abort();
}

/// What a test hook saw of the range requests other than the probe, and
/// whether it sends the next one to a port that closes the connection.
#[derive(Clone, Default)]
struct Hook {
    fail: Arc<AtomicBool>,
    sent: Arc<Mutex<Vec<Option<String>>>>,
}

impl Hook {
    /// Record the `If-Range` of a range request with `range`, if it is not
    /// the probe; and whether it is to fail.
    fn record(&self, range: Option<&str>, if_range: Option<&str>) -> bool {
        if range.is_none_or(|range| range == "bytes=0-0") {
            return false;
        }
        self.sent.lock().unwrap().push(if_range.map(str::to_owned));
        self.fail.load(Ordering::SeqCst)
    }

    fn sent(&self) -> Vec<Option<String>> {
        self.sent.lock().unwrap().clone()
    }
}

/// A range request that fails at the transport level leaves the meta as it
/// was: it sent the validator of the meta, and the request that retries it
/// sends the same one, for both adapters.
#[tokio::test]
#[expect(
    clippy::result_large_err,
    reason = "the ureq call hook returns the error type of ureq"
)]
async fn a_failed_range_request_keeps_the_validator() {
    let (dead, closer) = closing_port().await;
    for name in ["reqwest", "ureq2"] {
        let (url, log, server) = server(RangeFault::HeadValidatorsDiffer).await;
        let hook = Hook::default();
        let adapter: Box<DynBoltLoadAdapter<'static>> = match name {
            "reqwest" => {
                let mut adapter = reqwest::Client::builder()
                    .no_proxy()
                    .build()
                    .unwrap()
                    .into_reqwest_adapter((reqwest::Method::GET, url));
                let hook = hook.clone();
                adapter.before_request(move |builder| {
                    let (client, request) = builder.build_split();
                    let mut request = request.unwrap();
                    let header = |name| {
                        request
                            .headers()
                            .get(name)
                            .map(|value: &reqwest::header::HeaderValue| {
                                value.to_str().unwrap().to_owned()
                            })
                    };
                    let range = header(reqwest::header::RANGE);
                    let if_range = header(reqwest::header::IF_RANGE);
                    if hook.record(range.as_deref(), if_range.as_deref()) {
                        request.url_mut().set_port(Some(dead)).unwrap();
                    }
                    reqwest::RequestBuilder::from_parts(client, request)
                });
                DynBoltLoadAdapter::new_box(adapter)
            }
            _ => {
                let mut adapter = ureq2::Agent::new().into_ureq_adapter(("get", url));
                let hook = hook.clone();
                adapter.call(move |request| {
                    if hook.record(request.header("range"), request.header("if-range")) {
                        ureq2::get(&format!("http://127.0.0.1:{dead}/")).call()
                    } else {
                        request.call()
                    }
                });
                DynBoltLoadAdapter::new_box(adapter)
            }
        };
        let meta = adapter.retrieve_meta().await.unwrap();
        let v1 = Some(Validator::StrongEtag("\"v1\"".into()));
        assert_eq!(meta.validator, v1, "{name}");

        hook.fail.store(true, Ordering::SeqCst);
        assert!(
            matches!(
                adapter.range_stream(10, 20).await,
                Err(AdapterError::Retryable { .. })
            ),
            "{name}"
        );
        assert_eq!(
            adapter.retrieve_meta().await.unwrap().validator,
            v1,
            "{name}"
        );

        hook.fail.store(false, Ordering::SeqCst);
        assert!(adapter.range_stream(10, 20).await.is_ok(), "{name}");
        assert_eq!(
            adapter.retrieve_meta().await.unwrap().validator,
            v1,
            "{name}"
        );
        let sent = Some("\"v1\"".to_owned());
        assert_eq!(hook.sent(), [sent.clone(), sent.clone()], "{name}");
        assert_eq!(last_if_range(&log), sent, "{name}");
        assert_eq!(probes(&log), 1, "{name}");
        server.abort();
    }
    closer.abort();
}
