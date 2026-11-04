//! Content codings of responses.
//!
//! The task writes the body of a full response as the file, decoded by
//! [`codings_to_decode`]. An HTTP adapter hands the task the headers this
//! reads, in `FullResponse`.

use std::fmt;

/// A content coding (RFC 9110 §8.4.1) other than `identity`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ContentCoding {
    /// `gzip`, or its alias `x-gzip`.
    Gzip,
    /// `deflate`, the zlib format (RFC 9110 §8.4.1.2).
    Deflate,
    /// `br`.
    Brotli,
    /// `zstd`.
    Zstd,
    /// A coding this crate does not know, lowercased.
    Other(String),
}

impl ContentCoding {
    /// Parse a combined `Content-Encoding` value, in the order applied.
    ///
    /// Names match case-insensitively and `x-gzip` is [`Gzip`](Self::Gzip).
    /// `identity` and empty items are skipped, so a value without a coding
    /// gives an empty list.
    pub fn parse_list(value: &str) -> Vec<ContentCoding> {
        value
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty() && !name.eq_ignore_ascii_case("identity"))
            .map(Self::from_name)
            .collect()
    }

    fn from_name(name: &str) -> Self {
        let name = name.to_ascii_lowercase();
        match name.as_str() {
            "gzip" | "x-gzip" => Self::Gzip,
            "deflate" => Self::Deflate,
            "br" => Self::Brotli,
            "zstd" => Self::Zstd,
            _ => Self::Other(name),
        }
    }

    /// The registered name, such as `gzip` or `br`.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Gzip => "gzip",
            Self::Deflate => "deflate",
            Self::Brotli => "br",
            Self::Zstd => "zstd",
            Self::Other(name) => name,
        }
    }

    /// Whether `content_type` names the file format this coding produces
    /// (RFC 6713 for gzip and zlib, RFC 8878 for zstd).
    fn is_format_of(&self, content_type: &str) -> bool {
        let formats: &[&str] = match self {
            Self::Gzip => &["application/gzip", "application/x-gzip"],
            Self::Deflate => &["application/zlib"],
            Self::Zstd => &["application/zstd"],
            Self::Brotli | Self::Other(_) => &[],
        };
        let media_type = content_type.split(';').next().unwrap_or_default().trim();
        formats
            .iter()
            .any(|format| media_type.eq_ignore_ascii_case(format))
    }
}

impl fmt::Display for ContentCoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The codings to remove from the body of a full response, in the order
/// applied, to get the file.
///
/// Pass combined values, as for [`RangeHeaders`](crate::RangeHeaders). The
/// list is empty for a response without a coding, and for one whose
/// `Content-Type` is the format of its only coding, such as
/// `application/gzip` with `Content-Encoding: gzip`: a server that marks a
/// `.tar.gz` file this way means the archive itself.
pub fn codings_to_decode(
    content_encoding: Option<&str>,
    content_type: Option<&str>,
) -> Vec<ContentCoding> {
    let Some(content_encoding) = content_encoding else {
        return Vec::new();
    };
    let codings = ContentCoding::parse_list(content_encoding);
    match (codings.as_slice(), content_type) {
        ([only], Some(content_type)) if only.is_format_of(content_type) => Vec::new(),
        _ => codings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_no_coding() {
        for value in ["", "identity", " Identity ", "identity, identity", ","] {
            assert_eq!(ContentCoding::parse_list(value), [], "{value:?}");
        }
    }

    #[test]
    fn parses_names_case_insensitively() {
        use ContentCoding::*;
        for (value, coding) in [
            ("gzip", Gzip),
            (" GZip ", Gzip),
            ("X-GZIP", Gzip),
            ("Deflate", Deflate),
            ("BR", Brotli),
            ("zstd", Zstd),
        ] {
            assert_eq!(ContentCoding::parse_list(value), [coding], "{value:?}");
        }
    }

    #[test]
    fn keeps_the_order_applied_and_skips_identity() {
        use ContentCoding::*;
        assert_eq!(
            ContentCoding::parse_list("gzip, identity,, br"),
            [Gzip, Brotli]
        );
    }

    #[test]
    fn keeps_an_unknown_coding_lowercased() {
        assert_eq!(
            ContentCoding::parse_list("Compress, \u{FFFD}"),
            [
                ContentCoding::Other("compress".into()),
                ContentCoding::Other("\u{FFFD}".into()),
            ]
        );
    }

    #[test]
    fn displays_the_registered_name() {
        for value in ["gzip", "deflate", "br", "zstd", "compress"] {
            let codings = ContentCoding::parse_list(value);
            assert_eq!(codings.len(), 1, "{value:?}");
            assert_eq!(codings[0].to_string(), value);
        }
        assert_eq!(ContentCoding::parse_list("x-gzip")[0].as_str(), "gzip");
    }

    #[test]
    fn decodes_nothing_without_a_coding() {
        assert_eq!(codings_to_decode(None, None), []);
        assert_eq!(codings_to_decode(None, Some("application/gzip")), []);
        assert_eq!(codings_to_decode(Some("identity"), None), []);
    }

    #[test]
    fn decodes_every_coding_of_a_coded_response() {
        use ContentCoding::*;
        for (encoding, content_type, codings) in [
            ("gzip", None, vec![Gzip]),
            ("br", Some("application/octet-stream"), vec![Brotli]),
            ("gzip", Some("text/plain"), vec![Gzip]),
            // Decoding once would still leave gzip, so it is not the archive.
            ("gzip, gzip", Some("application/gzip"), vec![Gzip, Gzip]),
            ("br", Some("application/gzip"), vec![Brotli]),
            ("compress", None, vec![Other("compress".into())]),
        ] {
            assert_eq!(
                codings_to_decode(Some(encoding), content_type),
                codings,
                "{encoding:?} {content_type:?}"
            );
        }
    }

    #[test]
    fn keeps_a_file_whose_type_is_its_coding() {
        for (encoding, content_type) in [
            ("gzip", "application/gzip"),
            ("GZIP", "Application/X-Gzip; charset=binary"),
            ("x-gzip", "application/gzip"),
            ("identity, gzip", "application/gzip"),
            ("deflate", "application/zlib"),
            ("zstd", "application/zstd"),
        ] {
            assert_eq!(
                codings_to_decode(Some(encoding), Some(content_type)),
                [],
                "{encoding:?} {content_type:?}"
            );
        }
    }
}
