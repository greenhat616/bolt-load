//! Content codings of responses.
//!
//! Like [`validate_range_response`](super::validate_range_response), this is
//! part of the adapter contract: the task writes a body as the file, so an
//! HTTP adapter should check a full response with [`check_content_coding`]
//! before handing out its body.

use super::{AdapterError, UnretryableError};

/// Whether a combined `Content-Encoding` value lists only `identity`.
pub fn is_identity_coding(content_encoding: &str) -> bool {
    codings(content_encoding).next().is_none()
}

/// Check the `Content-Encoding` of a full response whose body the HTTP client
/// did not decode, so that only the file's bytes are written.
///
/// Pass combined values, as for [`RangeHeaders`](super::RangeHeaders). A
/// response without a coding passes. So does one whose `Content-Type` is the
/// format of its only coding, such as `application/gzip` with
/// `Content-Encoding: gzip`: a server that marks a `.tar.gz` file this way
/// means the archive itself. Any other coding fails with
/// [`UnretryableError::ContentCoded`].
pub fn check_content_coding(
    content_encoding: Option<&str>,
    content_type: Option<&str>,
) -> Result<(), AdapterError> {
    let Some(content_encoding) = content_encoding.filter(|value| !is_identity_coding(value)) else {
        return Ok(());
    };
    let mut listed = codings(content_encoding);
    let only = listed.next().filter(|_| listed.next().is_none());
    let is_format = only
        .zip(content_type)
        .is_some_and(|(coding, content_type)| is_format_of(coding, content_type));
    if is_format {
        return Ok(());
    }
    Err(UnretryableError::ContentCoded {
        coding: content_encoding.to_owned(),
    }
    .into())
}

/// The codings in `content_encoding` other than `identity`, in order.
fn codings(content_encoding: &str) -> impl Iterator<Item = &str> {
    content_encoding
        .split(',')
        .map(str::trim)
        .filter(|coding| !coding.is_empty() && !coding.eq_ignore_ascii_case("identity"))
}

/// Whether `content_type` names the file format that `coding` produces
/// (RFC 6713 for gzip and zlib, RFC 8878 for zstd).
fn is_format_of(coding: &str, content_type: &str) -> bool {
    let formats: &[&str] =
        if coding.eq_ignore_ascii_case("gzip") || coding.eq_ignore_ascii_case("x-gzip") {
            &["application/gzip", "application/x-gzip"]
        } else if coding.eq_ignore_ascii_case("deflate") {
            &["application/zlib"]
        } else if coding.eq_ignore_ascii_case("zstd") {
            &["application/zstd"]
        } else {
            &[]
        };
    let media_type = content_type.split(';').next().unwrap_or_default().trim();
    formats
        .iter()
        .any(|format| media_type.eq_ignore_ascii_case(format))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coded(result: Result<(), AdapterError>) -> Option<String> {
        match result {
            Ok(()) => None,
            Err(AdapterError::Unretryable {
                source: UnretryableError::ContentCoded { coding },
            }) => Some(coding),
            Err(other) => panic!("unexpected error {other:?}"),
        }
    }

    #[test]
    fn identity_is_no_coding() {
        for value in ["", "identity", " Identity ", "identity, identity", ","] {
            assert!(is_identity_coding(value), "{value:?}");
        }
        for value in ["gzip", "identity, gzip", "\u{FFFD}"] {
            assert!(!is_identity_coding(value), "{value:?}");
        }
    }

    #[test]
    fn passes_a_response_without_a_coding() {
        assert_eq!(coded(check_content_coding(None, None)), None);
        assert_eq!(coded(check_content_coding(Some("identity"), None)), None);
    }

    #[test]
    fn rejects_a_coded_response() {
        for (encoding, content_type) in [
            ("gzip", None),
            ("br", Some("application/octet-stream")),
            ("gzip", Some("text/plain")),
            // Decoding once would still leave gzip, so it is not the archive.
            ("gzip, gzip", Some("application/gzip")),
            ("br", Some("application/gzip")),
        ] {
            assert_eq!(
                coded(check_content_coding(Some(encoding), content_type)),
                Some(encoding.to_owned()),
                "{encoding:?} {content_type:?}"
            );
        }
    }

    #[test]
    fn passes_a_file_whose_type_is_its_coding() {
        for (encoding, content_type) in [
            ("gzip", "application/gzip"),
            ("GZIP", "Application/X-Gzip; charset=binary"),
            ("x-gzip", "application/gzip"),
            ("identity, gzip", "application/gzip"),
            ("deflate", "application/zlib"),
            ("zstd", "application/zstd"),
        ] {
            assert_eq!(
                coded(check_content_coding(Some(encoding), Some(content_type))),
                None,
                "{encoding:?} {content_type:?}"
            );
        }
    }
}
