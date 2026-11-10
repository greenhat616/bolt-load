use std::time::SystemTime;

/// A validator that can make a range request conditional, sent in
/// `If-Range` (RFC 9110 §13.1.5).
///
/// It holds the value as the server sent it, and is sent back unchanged.
/// `If-Range` takes no other kind, so the enum is exhaustive.
///
/// An adapter reports the one it sends in
/// [`BoltLoadAdapterMeta::validator`](super::BoltLoadAdapterMeta::validator).
/// `bolt-load-http` picks it from the headers of a response and checks
/// responses against it.
///
/// `==` compares the values as strings; [`Validator::matches`] compares
/// what they name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Validator {
    /// A strong entity tag, quotes included, such as `"v1"`.
    StrongEtag(String),
    /// A `Last-Modified` date at least one second older than the `Date` of
    /// the response that carried it.
    LastModified(String),
}

impl Validator {
    /// The value to send in `If-Range`.
    pub fn as_str(&self) -> &str {
        match self {
            Validator::StrongEtag(value) | Validator::LastModified(value) => value,
        }
    }

    /// Whether `other` names the same representation: a validator of the
    /// same kind, with the same strong entity tag, byte for byte, or the
    /// same `Last-Modified` date. Dates compare as dates, so their formats
    /// may differ (RFC 9110 §5.6.7); a value that is no HTTP-date matches
    /// only the same string.
    pub fn matches(&self, other: &Validator) -> bool {
        match (self, other) {
            (Validator::StrongEtag(this), Validator::StrongEtag(other)) => this == other,
            (Validator::LastModified(this), Validator::LastModified(other)) => {
                match (http_date(this), http_date(other)) {
                    (Some(this), Some(other)) => this == other,
                    _ => this == other,
                }
            }
            _ => false,
        }
    }
}

/// `value` as an HTTP-date, in any of its three formats.
fn http_date(value: &str) -> Option<SystemTime> {
    httpdate::parse_http_date(value.trim_matches([' ', '\t'])).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "Sun, 08 Nov 2026 12:00:09 GMT";
    const DATE: &str = "Sun, 08 Nov 2026 12:00:10 GMT";

    fn etag(value: &str) -> Validator {
        Validator::StrongEtag(value.to_owned())
    }

    fn date(value: &str) -> Validator {
        Validator::LastModified(value.to_owned())
    }

    #[test]
    fn test_as_str_is_the_value_sent() {
        assert_eq!(etag("\"v1\"").as_str(), "\"v1\"");
        assert_eq!(date(OLD).as_str(), OLD);
    }

    /// The same strong entity tag, byte for byte.
    #[test]
    fn test_entity_tags_match_byte_for_byte() {
        assert!(etag("\"v1\"").matches(&etag("\"v1\"")));
        assert!(!etag("\"v1\"").matches(&etag("\"v2\"")));
        assert!(!etag("\"v1\"").matches(&etag("\"V1\"")));
        assert!(!etag("\"v1\"").matches(&etag("\"v1\" ")));
    }

    /// The same date, in any format; another date does not match.
    #[test]
    fn test_dates_match_as_dates() {
        assert!(date(OLD).matches(&date(OLD)));
        for same in ["Sunday, 08-Nov-26 12:00:09 GMT", "Sun Nov  8 12:00:09 2026"] {
            assert!(date(OLD).matches(&date(same)), "{same:?}");
            assert!(date(same).matches(&date(OLD)), "{same:?}");
        }
        assert!(!date(OLD).matches(&date(DATE)));
        assert!(!date(OLD).matches(&date("yesterday")));
        assert!(!date("yesterday").matches(&date(OLD)));
        assert!(date("yesterday").matches(&date("yesterday")));
        assert!(!date("yesterday").matches(&date("today")));
    }

    /// An entity tag never matches a date, even with the same value.
    #[test]
    fn test_another_kind_does_not_match() {
        assert!(!etag(OLD).matches(&date(OLD)));
        assert!(!date(OLD).matches(&etag(OLD)));
        assert!(!etag("\"v1\"").matches(&date("\"v1\"")));
    }
}
