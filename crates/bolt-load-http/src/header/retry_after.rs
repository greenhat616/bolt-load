//! `Retry-After` header parsing (RFC 9110 §10.2.3).

use std::time::{Duration, SystemTime};

/// Parse a `Retry-After` value: delay-seconds or an HTTP-date.
///
/// A date is measured from `now`; one in the past is no delay. A number
/// too large for `u64` saturates. Anything else is `None`. Callers apply
/// their own upper bound.
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        // Only an overflowing number fails to parse here.
        return Some(Duration::from_secs(value.parse().unwrap_or(u64::MAX)));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn parses_seconds_and_dates() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let date = |at: SystemTime| httpdate::fmt_http_date(at);
        let secs = |s| Some(Duration::from_secs(s));
        let cases = [
            ("5".to_owned(), secs(5)),
            (" 5 ".to_owned(), secs(5)),
            ("0".to_owned(), secs(0)),
            ("999".to_owned(), secs(999)),
            ("99999999999999999999999".to_owned(), secs(u64::MAX)),
            ("-1".to_owned(), None),
            ("+5".to_owned(), None),
            (date(now - Duration::from_secs(60)), secs(0)),
            (date(now + Duration::from_secs(30)), secs(30)),
            ("soon".to_owned(), None),
            (String::new(), None),
        ];
        for (value, expected) in cases {
            assert_eq!(parse_retry_after(&value, now), expected, "{value:?}");
        }
    }
}
