use std::{error::Error, fmt, str::FromStr};

/// A parsed single-part `Content-Range: bytes first-last/complete` header.
///
/// `last` is inclusive, as on the wire. `complete` is `None` for `*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentRange {
    pub first: u64,
    pub last: u64,
    pub complete: Option<u64>,
}

impl fmt::Display for ContentRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bytes {}-{}/", self.first, self.last)?;
        match self.complete {
            Some(complete) => write!(f, "{complete}"),
            None => f.write_str("*"),
        }
    }
}

/// A `Content-Range` value that is not a valid `bytes first-last/complete`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidContentRange;

impl fmt::Display for InvalidContentRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid Content-Range")
    }
}

impl Error for InvalidContentRange {}

impl FromStr for ContentRange {
    type Err = InvalidContentRange;

    /// Parse a satisfied byte range (RFC 9110 §14.4). The unit is matched
    /// case-insensitively and whitespace around the numbers is ignored.
    /// `first > last`, or `last` beyond a known `complete`, is invalid.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let range = byte_range(value).ok_or(InvalidContentRange)?;
        let (span, complete) = range.split_once('/').ok_or(InvalidContentRange)?;
        let (first, last) = span.split_once('-').ok_or(InvalidContentRange)?;
        let first = number(first).ok_or(InvalidContentRange)?;
        let last = number(last).ok_or(InvalidContentRange)?;
        let complete = match complete.trim() {
            "*" => None,
            complete => Some(number(complete).ok_or(InvalidContentRange)?),
        };
        if first > last || complete.is_some_and(|complete| last >= complete) {
            return Err(InvalidContentRange);
        }
        Ok(Self {
            first,
            last,
            complete,
        })
    }
}

impl ContentRange {
    /// The complete length of an unsatisfied range, `bytes */complete`, as a
    /// 416 response sends it.
    pub fn unsatisfied_complete(value: &str) -> Option<u64> {
        let (star, complete) = byte_range(value)?.split_once('/')?;
        (star.trim() == "*").then(|| number(complete)).flatten()
    }
}

/// The part after the `bytes` unit.
fn byte_range(value: &str) -> Option<&str> {
    let (unit, range) = value.trim().split_once(|c: char| c.is_ascii_whitespace())?;
    unit.eq_ignore_ascii_case("bytes").then_some(range)
}

fn number(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(first: u64, last: u64, complete: Option<u64>) -> ContentRange {
        ContentRange {
            first,
            last,
            complete,
        }
    }

    #[test]
    fn displays_as_the_header_value() {
        assert_eq!(range(0, 9, Some(100)).to_string(), "bytes 0-9/100");
        assert_eq!(range(0, 9, None).to_string(), "bytes 0-9/*");
    }

    #[test]
    fn parses_case_and_whitespace_variants() {
        for value in [
            "bytes 0-9/100",
            "Bytes 0-9/100",
            "BYTES 0-9/100",
            "  bytes 0-9/100  ",
            "bytes\t0-9/100",
            "bytes  0 - 9 / 100",
        ] {
            assert_eq!(value.parse(), Ok(range(0, 9, Some(100))), "{value:?}");
        }
        assert_eq!("bytes 5-5/*".parse(), Ok(range(5, 5, None)));
    }

    #[test]
    fn rejects_malformed_values() {
        for value in [
            "",
            "bytes",
            "bytes 0-9",
            "bytes */100",
            "items 0-9/100",
            "bytes=0-9/100",
            "bytes 9-0/100",
            "bytes 0-100/100",
            "bytes -1-9/100",
            "bytes +0-9/100",
            "bytes 0-9/abc",
            "bytes 0-9,20-29/100",
        ] {
            assert_eq!(
                value.parse::<ContentRange>(),
                Err(InvalidContentRange),
                "{value:?}"
            );
        }
    }

    #[test]
    fn reads_the_complete_length_of_an_unsatisfied_range() {
        assert_eq!(ContentRange::unsatisfied_complete("bytes */100"), Some(100));
        assert_eq!(
            ContentRange::unsatisfied_complete(" Bytes * / 100 "),
            Some(100)
        );
        assert_eq!(ContentRange::unsatisfied_complete("bytes 0-9/100"), None);
        assert_eq!(ContentRange::unsatisfied_complete("bytes */*"), None);
        assert_eq!(ContentRange::unsatisfied_complete("*/100"), None);
    }
}
