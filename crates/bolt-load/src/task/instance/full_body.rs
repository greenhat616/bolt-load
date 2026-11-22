//! The file a full response carries: its body, decoded as its
//! `Content-Encoding` says (download strategy hardening, design §4.4).

use std::{io, sync::Arc};

use bolt_load_decode::{DecodeError, DecodeOptions, decode};
use bolt_load_http::codings_to_decode;
use bolt_load_utils::telemetry::*;
use futures::TryStreamExt;

use crate::adapter::{AdapterError, AnyBytesStream, FullResponse, UnretryableError};

/// The bytes of the file, from a full response.
pub(super) struct FileBody {
    pub(super) stream: AnyBytesStream,
    /// Whether `stream` is decoded, so that its length is not the
    /// response's.
    pub(super) decoded: bool,
}

/// Decode the body of `response`, unless it is the file as it is.
///
/// A coding the task cannot decode fails with `ContentCoded`; a body that
/// does not decode fails with `BodyUndecodable` as it is read. Both are
/// unretryable. An error of the body itself passes through unchanged.
pub(super) fn file_body(response: FullResponse) -> Result<FileBody, AdapterError> {
    let FullResponse {
        body,
        content_encoding,
        content_type,
        ..
    } = response;
    let codings = codings_to_decode(content_encoding.as_deref(), content_type.as_deref());
    if codings.is_empty() {
        return Ok(FileBody {
            stream: body,
            decoded: false,
        });
    }
    // No `max_output`: the file goes to disk as it is decoded, and the
    // decoder holds one buffer per coding however large it gets.
    let decoded =
        decode(body.map_err(BodyError), &codings, DecodeOptions::default()).map_err(|error| {
            warn!("cannot decode the full response: {error}");
            UnretryableError::ContentCoded {
                coding: content_encoding.unwrap_or_default(),
            }
        })?;
    Ok(FileBody {
        stream: Box::pin(decoded.map_err(|BodyError(error)| error)),
        decoded: true,
    })
}

/// An error of the body, or of decoding it as the adapter error it becomes.
struct BodyError(AdapterError);

impl From<DecodeError> for BodyError {
    fn from(error: DecodeError) -> Self {
        let source = match error {
            DecodeError::Invalid { source } => source,
            error => Arc::new(io::Error::new(io::ErrorKind::InvalidData, error)),
        };
        Self(UnretryableError::BodyUndecodable { source }.into())
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures::{StreamExt, stream};
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::adapter::RetryableError;

    const CONTENT: &[u8] = b"bolt-load decodes the full response";

    fn gzip(content: &[u8]) -> Vec<u8> {
        bolt_load_tests::adapter::http_server::gzip(content)
            .unwrap()
            .to_vec()
    }

    /// A response whose body yields `items`.
    fn response(
        items: Vec<Result<Bytes, AdapterError>>,
        content_encoding: Option<&str>,
        content_type: Option<&str>,
    ) -> FullResponse {
        FullResponse::new(Box::pin(stream::iter(items)))
            .with_content_encoding(content_encoding.map(str::to_owned))
            .with_content_type(content_type.map(str::to_owned))
    }

    /// Whether the body was decoded, and every item it yields.
    async fn read(response: FullResponse) -> (bool, Vec<Result<Bytes, AdapterError>>) {
        let body = file_body(response).unwrap();
        (body.decoded, body.stream.collect().await)
    }

    fn concat(items: Vec<Result<Bytes, AdapterError>>) -> Vec<u8> {
        items
            .into_iter()
            .flat_map(|item| item.unwrap().to_vec())
            .collect()
    }

    fn unretryable(item: &Result<Bytes, AdapterError>) -> &UnretryableError {
        match item {
            Err(AdapterError::Unretryable { source }) => source,
            other => panic!("expected an unretryable error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn hands_out_a_body_without_a_coding() {
        for encoding in [None, Some("identity")] {
            let (decoded, items) = read(response(
                vec![Ok(Bytes::from_static(CONTENT))],
                encoding,
                None,
            ))
            .await;
            assert!(!decoded, "{encoding:?}");
            assert_eq!(concat(items), CONTENT);
        }
    }

    #[tokio::test]
    async fn decodes_a_coded_body() {
        let encoded = gzip(CONTENT);
        let (head, tail) = encoded.split_at(encoded.len() / 2);
        let items = vec![
            Ok(Bytes::copy_from_slice(head)),
            Ok(Bytes::copy_from_slice(tail)),
        ];
        let (decoded, items) = read(response(items, Some("GZIP"), Some("text/plain"))).await;
        assert!(decoded);
        assert_eq!(concat(items), CONTENT);
    }

    #[tokio::test]
    async fn keeps_a_file_whose_type_is_its_coding() {
        let archive = gzip(CONTENT);
        let items = vec![Ok(Bytes::from(archive.clone()))];
        let (decoded, items) = read(response(items, Some("gzip"), Some("application/gzip"))).await;
        assert!(!decoded);
        assert_eq!(concat(items), archive);
    }

    #[test]
    fn rejects_a_coding_it_cannot_decode() {
        for encoding in ["compress", "gzip, compress", "gzip, gzip, gzip"] {
            let error = file_body(response(Vec::new(), Some(encoding), None))
                .err()
                .unwrap();
            assert!(
                matches!(
                    &error,
                    AdapterError::Unretryable {
                        source: UnretryableError::ContentCoded { coding }
                    } if coding == encoding
                ),
                "{encoding:?}: {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn fails_a_body_that_does_not_decode() {
        let mut trailing = gzip(CONTENT);
        trailing.push(0);
        let encoded = gzip(CONTENT);
        for (case, body) in [
            ("corrupt", b"not gzip".to_vec()),
            ("truncated", encoded[..encoded.len() - 1].to_vec()),
            ("trailing data", trailing),
        ] {
            let (_, items) = read(response(vec![Ok(body.into())], Some("gzip"), None)).await;
            let last = items.last().unwrap();
            assert!(
                matches!(unretryable(last), UnretryableError::BodyUndecodable { .. }),
                "{case}: {last:?}"
            );
        }
    }

    #[tokio::test]
    async fn passes_a_body_error_through() {
        let encoded = gzip(CONTENT);
        let items = vec![
            Ok(Bytes::copy_from_slice(&encoded[..4])),
            Err(RetryableError::Status { status: 503 }.into()),
        ];
        let (_, items) = read(response(items, Some("gzip"), None)).await;
        assert!(
            matches!(
                items.last().unwrap(),
                Err(AdapterError::Retryable {
                    source: RetryableError::Status { status: 503 }
                })
            ),
            "{items:?}"
        );
    }
}
