//! Both adapters make their range requests conditional on the validator
//! the task expects (design §11.2, D14): the strong `ETag` of a GET, else a
//! `Last-Modified` at least a second before `Date`, never with a weak
//! `ETag`. A 200 with another validator, or a 206 with a conflicting one,
//! is a changed resource; a 200 with the same one, an ignored range.

#![cfg(all(feature = "reqwest", feature = "ureq2"))]

use bolt_load_adapter::{reqwest::IntoReqwestAdapter, ureq2::IntoUreqAdapter};
use bolt_load_core::adapter::{
    AdapterError, BoltLoadAdapter, BoltLoadAdapterMeta, DynBoltLoadAdapter, UnretryableError,
    Validator,
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

/// A range of `meta`'s representation.
async fn range(
    adapter: &DynBoltLoadAdapter<'static>,
    meta: &BoltLoadAdapterMeta,
    start: u64,
    end: u64,
) -> Result<bolt_load_core::adapter::AnyBytesStream, AdapterError> {
    adapter
        .range_stream_if(start, end, &meta.range_expectation())
        .await
}

/// A strong `ETag` goes in `If-Range`. Once the resource changed, the 200
/// it gets is a changed resource; after a refresh, the adapter retrieves
/// the new version and its ranges pass again.
#[tokio::test]
async fn a_strong_etag_makes_ranges_conditional() {
    let replacement = RangeFault::replacement(&vec![0; SIZE]).len() as u64;
    for (name, adapter, log, server) in adapters(RangeFault::ChangedMidDownload).await {
        let meta = adapter.retrieve_meta().await.unwrap();
        assert_eq!(meta.etag.as_deref(), Some("\"v1\""), "{name}");
        assert_eq!(
            meta.validator,
            Some(Validator::StrongEtag("\"v1\"".into())),
            "{name}"
        );
        assert!(meta.last_modified.is_some(), "{name}");
        // The first range gets half of itself, and the resource changes.
        assert!(range(&adapter, &meta, 10, 20).await.is_ok(), "{name}");
        assert_eq!(last_if_range(&log).as_deref(), Some("\"v1\""), "{name}");
        assert!(changed(range(&adapter, &meta, 20, 30).await), "{name}");

        adapter.refresh();
        let new = adapter.retrieve_meta().await.unwrap();
        assert_eq!(new.etag.as_deref(), Some("\"v2\""), "{name}");
        assert_eq!(new.content_size, replacement, "{name}");
        assert!(range(&adapter, &new, 1, 3).await.is_ok(), "{name}");
        assert_eq!(last_if_range(&log).as_deref(), Some("\"v2\""), "{name}");

        // The expectation passed, not what the adapter retrieved since,
        // makes the range conditional.
        assert!(changed(range(&adapter, &meta, 1, 3).await), "{name}");
        assert_eq!(last_if_range(&log).as_deref(), Some("\"v1\""), "{name}");
        // A plain range sends no validator.
        assert!(adapter.range_stream(1, 3).await.is_ok(), "{name}");
        assert_eq!(last_if_range(&log), None, "{name}");
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
        assert_eq!(meta.validator, None, "{name}");
        assert!(range(&adapter, &meta, 10, 20).await.is_ok(), "{name}");
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
            let expected = sent.then(|| Validator::LastModified(last_modified.clone()));
            assert_eq!(meta.validator, expected, "{name} {fault:?}");
            if fault == RangeFault::ObsoleteDateFormats {
                assert!(last_modified.contains('-'), "{name}: {last_modified}");
            }
            assert!(
                range(&adapter, &meta, 10, 20).await.is_ok(),
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
            assert_eq!(
                meta.validator,
                Some(Validator::LastModified(last_modified.clone())),
                "{name} {fault:?}"
            );
            assert!(
                range(&adapter, &meta, 10, 20).await.is_ok(),
                "{name} {fault:?}"
            );
            assert_eq!(
                last_if_range(&log),
                Some(last_modified),
                "{name} {fault:?}"
            );
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
            assert!(
                matches!(
                    unretryable(range(&adapter, &meta, 10, 20).await),
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
        assert_eq!(
            meta.validator,
            Some(Validator::StrongEtag("\"v1\"".into())),
            "{name}"
        );
        assert!(adapter.is_range_stream_available().await, "{name}");
        assert!(range(&adapter, &meta, 10, 20).await.is_ok(), "{name}");
        assert_eq!(last_if_range(&log).as_deref(), Some("\"v1\""), "{name}");
        server.abort();
    }
}

/// A 206 with a validator that conflicts with the one sent is a part of
/// another version, by `ETag` or by date. One without validators passes.
#[tokio::test]
async fn a_206_with_a_conflicting_validator_is_a_changed_resource() {
    for fault in [
        RangeFault::ChangedIgnoringIfRange,
        RangeFault::ChangedByDateIgnoringIfRange,
    ] {
        for (name, adapter, _log, server) in adapters(fault).await {
            let meta = adapter.retrieve_meta().await.unwrap();
            assert!(meta.validator.is_some(), "{name} {fault:?}");
            // The first range gets half of itself, and the resource changes.
            assert!(
                range(&adapter, &meta, 10, 20).await.is_ok(),
                "{name} {fault:?}"
            );
            assert!(
                changed(range(&adapter, &meta, 20, 30).await),
                "{name} {fault:?}"
            );
            server.abort();
        }
    }
    for (name, adapter, log, server) in adapters(RangeFault::PartsWithoutValidators).await {
        let meta = adapter.retrieve_meta().await.unwrap();
        assert!(meta.validator.is_some(), "{name}");
        assert!(range(&adapter, &meta, 10, 20).await.is_ok(), "{name}");
        assert_eq!(last_if_range(&log).as_deref(), Some("\"v1\""), "{name}");
        server.abort();
    }
}
