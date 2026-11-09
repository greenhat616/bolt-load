//! Validators that make a range request conditional (RFC 9110 §8.8,
//! §13.1.5).
//!
//! An adapter reads the validators of a resource when it retrieves its
//! metadata, and [`if_range_validator`] picks the one for
//! [`BoltLoadAdapterMeta::validator`](bolt_load_core::adapter::BoltLoadAdapterMeta::validator).
//! The task passes it back with every range request; the adapter sends it
//! in `If-Range`, and [`validate_range_response`](crate::validate_range_response)
//! checks the response's own validators against it. A server that holds
//! another representation then answers with the whole of it, or with
//! another validator, and the task learns that the resource changed.
//!
//! `ETag` and `Last-Modified` occur once in a response. Pass `None` for a
//! field that is missing or repeated; a repeated field combined into one
//! value fails to parse, so it counts as missing too.

use std::time::{Duration, SystemTime};

use bolt_load_core::adapter::Validator;

/// Optional whitespace around a field value (RFC 9110 §5.6.3).
const OWS: [char; 2] = [' ', '\t'];

/// Whether `value` is an entity tag (RFC 9110 §8.8.3), and whether it is
/// weak: `None` when it is not one.
///
/// ```text
/// entity-tag = [ weak ] opaque-tag
/// weak       = %s"W/"
/// opaque-tag = DQUOTE *etagc DQUOTE
/// etagc      = %x21 / %x23-7E / obs-text
/// ```
///
/// An opaque tag has no escaping, so a comma or a backslash inside it is
/// literal, and a quote ends it: a list such as `"a", "b"` is no entity tag.
fn entity_tag(value: &str) -> Option<bool> {
    let value = value.trim_matches(OWS);
    let (weak, opaque) = match value.strip_prefix("W/") {
        Some(opaque) => (true, opaque),
        None => (false, value),
    };
    let inner = opaque.strip_prefix('"')?.strip_suffix('"')?;
    inner
        .bytes()
        .all(|byte| byte == 0x21 || (0x23..=0x7e).contains(&byte) || byte >= 0x80)
        .then_some(weak)
}

/// Whether `etag` is a strong entity tag: an opaque tag, without the
/// weakness prefix `W/`. Only a strong one may go in `If-Range`.
pub fn is_strong_etag(etag: &str) -> bool {
    entity_tag(etag) == Some(false)
}

/// `etag` as a strong validator, if it is a strong entity tag.
pub fn strong_etag(etag: &str) -> Option<Validator> {
    is_strong_etag(etag).then(|| Validator::StrongEtag(etag.trim_matches(OWS).to_owned()))
}

fn http_date(value: &str) -> Option<SystemTime> {
    httpdate::parse_http_date(value.trim_matches(OWS)).ok()
}

/// The validator to send in `If-Range` for a resource whose response
/// carried `etag`, `last_modified` and `date` (RFC 9110 §13.1.5):
///
/// - its strong entity tag;
/// - nothing, if it has a weak one: a date may stand in only when there is
///   no entity tag at all;
/// - else its `Last-Modified`, when that is at least one second older than
///   the response's `Date`. A date closer to `Date`, later than it, or
///   without one, may name two versions modified within the same second,
///   so it is no strong validator (§8.8.2.2).
///
/// A value that does not parse counts as missing. The validator holds the
/// value as the server sent it, trimmed.
pub fn if_range_validator(
    etag: Option<&str>,
    last_modified: Option<&str>,
    date: Option<&str>,
) -> Option<Validator> {
    match etag.and_then(entity_tag) {
        Some(false) => return strong_etag(etag?),
        Some(true) => return None,
        None => {}
    }
    let last_modified = last_modified?;
    let modified = http_date(last_modified)?;
    let date = http_date(date?)?;
    date.duration_since(modified)
        .is_ok_and(|age| age >= Duration::from_secs(1))
        .then(|| Validator::LastModified(last_modified.trim_matches(OWS).to_owned()))
}

/// Whether a response that carried `etag` and `last_modified` has the
/// validator `sent`: the same strong entity tag, or the same
/// `Last-Modified` date, compared as dates, so its format may differ.
pub fn has_validator(sent: &Validator, etag: Option<&str>, last_modified: Option<&str>) -> bool {
    match sent {
        Validator::StrongEtag(sent) => etag.is_some_and(|etag| {
            is_strong_etag(etag) && etag.trim_matches(OWS) == sent.trim_matches(OWS)
        }),
        Validator::LastModified(sent) => last_modified.is_some_and(|last_modified| {
            match (http_date(sent), http_date(last_modified)) {
                (Some(sent), Some(last_modified)) => sent == last_modified,
                _ => sent.trim_matches(OWS) == last_modified.trim_matches(OWS),
            }
        }),
    }
}

/// Whether a response that carried `etag` and `last_modified` names
/// another representation than `expected` does: a validator of the same
/// kind that is there, parses, and differs. An entity tag differs from a
/// strong one unless it is the same strong tag. A missing validator, or
/// one that does not parse, does not conflict.
pub fn conflicts_with(
    expected: &Validator,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> bool {
    match expected {
        Validator::StrongEtag(_) => etag
            .filter(|etag| entity_tag(etag).is_some())
            .is_some_and(|etag| !has_validator(expected, Some(etag), None)),
        Validator::LastModified(_) => last_modified
            .filter(|last_modified| http_date(last_modified).is_some())
            .is_some_and(|last_modified| !has_validator(expected, None, Some(last_modified))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATE: &str = "Sun, 08 Nov 2026 12:00:10 GMT";
    const OLD: &str = "Sun, 08 Nov 2026 12:00:09 GMT";

    fn etag(value: &str) -> Validator {
        Validator::StrongEtag(value.to_owned())
    }

    fn date(value: &str) -> Validator {
        Validator::LastModified(value.to_owned())
    }

    #[test]
    fn test_the_entity_tag_grammar() {
        for strong in [
            "\"v1\"",
            " \"\" ",
            "\"a,b\"",
            "\"a\\b\"",
            "\"!#~\"",
            "\"caf\u{e9}\"",
        ] {
            assert!(is_strong_etag(strong), "{strong:?}");
        }
        assert_eq!(entity_tag("W/\"v1\""), Some(true));
        for invalid in [
            "v1",
            "\"",
            "\"v1",
            "\"a\", \"b\"",
            "\"a\",\"b\"",
            "\"a\"b\"",
            "\"a b\"",
            "\"a\tb\"",
            "w/\"v1\"",
            "W/ \"v1\"",
            "W/v1",
            "",
        ] {
            assert_eq!(entity_tag(invalid), None, "{invalid:?}");
        }
        assert_eq!(strong_etag(" \"v1\"\t"), Some(etag("\"v1\"")));
    }

    /// A strong entity tag first; a weak one means no validator at all; a
    /// `Last-Modified` at least a second before `Date` otherwise.
    #[test]
    fn test_the_validator_for_if_range() {
        assert_eq!(
            if_range_validator(Some(" \"v1\" "), Some(OLD), Some(DATE)),
            Some(etag("\"v1\""))
        );
        assert_eq!(if_range_validator(Some("W/\"v1\""), Some(OLD), Some(DATE)), None);
        assert_eq!(if_range_validator(Some("W/\"v1\""), None, Some(DATE)), None);
        assert_eq!(
            if_range_validator(None, Some(OLD), Some(DATE)),
            Some(date(OLD))
        );
        // A malformed or combined entity tag is no entity tag.
        for malformed in ["\"a\", \"b\"", "v1", "\"a\"b\""] {
            assert_eq!(
                if_range_validator(Some(malformed), Some(OLD), Some(DATE)),
                Some(date(OLD)),
                "{malformed:?}"
            );
        }
        assert_eq!(if_range_validator(None, None, Some(DATE)), None);
        assert_eq!(
            if_range_validator(None, Some("yesterday"), Some(DATE)),
            None
        );
        let combined = format!("{OLD}, {OLD}");
        assert_eq!(
            if_range_validator(None, Some(&combined), Some(DATE)),
            None
        );
    }

    /// Equal, exactly one second, later than `Date`, no `Date`, and the
    /// three HTTP-date formats.
    #[test]
    fn test_a_date_is_a_validator_one_second_before_date() {
        assert_eq!(if_range_validator(None, Some(DATE), Some(DATE)), None);
        assert_eq!(
            if_range_validator(None, Some(OLD), Some(DATE)),
            Some(date(OLD))
        );
        assert_eq!(
            if_range_validator(None, Some("Sun, 08 Nov 2026 12:00:09 GMT"), Some(OLD)),
            None
        );
        assert_eq!(
            if_range_validator(None, Some("Sun, 08 Nov 2026 12:00:11 GMT"), Some(DATE)),
            None
        );
        assert_eq!(if_range_validator(None, Some(OLD), None), None);
        assert_eq!(if_range_validator(None, Some(OLD), Some("soon")), None);
        for old in ["Sunday, 08-Nov-26 12:00:09 GMT", "Sun Nov  8 12:00:09 2026"] {
            assert_eq!(
                if_range_validator(None, Some(old), Some(DATE)),
                Some(date(old)),
                "{old:?}"
            );
        }
    }

    #[test]
    fn test_a_response_has_the_validator_sent() {
        assert!(has_validator(&etag("\"v1\""), Some("\"v1\""), None));
        assert!(!has_validator(&etag("\"v1\""), Some("\"v2\""), Some(OLD)));
        assert!(!has_validator(&etag("\"v1\""), Some("W/\"v1\""), None));
        assert!(!has_validator(&etag("\"v1\""), Some("\"v1\", \"v1\""), None));
        assert!(!has_validator(&etag("\"v1\""), None, None));
        assert!(has_validator(&date(OLD), Some("\"v2\""), Some(OLD)));
        assert!(has_validator(
            &date(OLD),
            None,
            Some("Sunday, 08-Nov-26 12:00:09 GMT")
        ));
        assert!(!has_validator(&date(OLD), None, Some(DATE)));
        assert!(!has_validator(&date(OLD), None, None));
    }

    /// Only a validator of the same kind that is there, parses and differs
    /// conflicts.
    #[test]
    fn test_a_conflicting_validator() {
        let v1 = etag("\"v1\"");
        assert!(!conflicts_with(&v1, Some("\"v1\""), None));
        assert!(conflicts_with(&v1, Some("\"v2\""), None));
        assert!(conflicts_with(&v1, Some("W/\"v1\""), None));
        assert!(!conflicts_with(&v1, None, Some(DATE)));
        assert!(!conflicts_with(&v1, Some("\"v1\", \"v2\""), None));
        assert!(!conflicts_with(&v1, Some("v2"), None));

        let old = date(OLD);
        assert!(!conflicts_with(&old, None, Some(OLD)));
        assert!(!conflicts_with(&old, None, Some("Sun Nov  8 12:00:09 2026")));
        assert!(conflicts_with(&old, None, Some(DATE)));
        assert!(!conflicts_with(&old, Some("\"v2\""), None));
        assert!(!conflicts_with(&old, None, Some("yesterday")));
        assert!(!conflicts_with(&old, None, Some(&format!("{OLD}, {DATE}"))));
    }
}
