use std::fmt;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_as_the_header_value() {
        let known = ContentRange {
            first: 0,
            last: 9,
            complete: Some(100),
        };
        let unknown = ContentRange {
            complete: None,
            ..known
        };
        assert_eq!(known.to_string(), "bytes 0-9/100");
        assert_eq!(unknown.to_string(), "bytes 0-9/*");
    }
}
