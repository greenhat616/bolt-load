//! Streaming content decoding for bolt-load.
//!
//! [`decode`] wraps a body stream and decodes its `Content-Encoding` as the
//! chunks arrive. It is a thin layer over the decoders of
//! `compression-codecs`, which stays out of the API.
//!
//! - Input chunks are read in place, never copied.
//! - Output goes to one buffer of [`DecodeOptions::buffer_size`] per coding,
//!   handed out without a copy. Once the last chunk is dropped, the buffer's
//!   allocation is reused.
//! - Each poll yields at most one buffer, so a body that compresses well
//!   cannot take more memory or time per poll, and backpressure reaches the
//!   network read.
//!
//! The codings follow their features: `gzip` and `deflate` are on by
//! default, `br` and `zstd` are off. `zstd` builds the libzstd C library.

mod codec;
mod error;
mod stream;

pub use bolt_load_http::ContentCoding;
use futures::Stream;

use crate::codec::Codec;
pub use crate::{error::DecodeError, stream::Decoded};

/// The most codings one body may stack, such as `gzip, br`.
pub const MAX_LAYERS: usize = 2;

/// The default [`DecodeOptions::buffer_size`]: 64 KiB.
pub const DEFAULT_BUFFER_SIZE: usize = 64 * 1024;

/// Options for [`decode`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DecodeOptions {
    /// The output buffer of each coding, in bytes; 0 counts as 1. Each item
    /// of the decoded stream is at most this long.
    pub buffer_size: usize,
    /// Fail with [`DecodeError::TooLarge`] once the decoded body would be
    /// longer, against decompression bombs.
    pub max_output: Option<u64>,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        Self {
            buffer_size: DEFAULT_BUFFER_SIZE,
            max_output: None,
        }
    }
}

impl DecodeOptions {
    /// Set [`buffer_size`](Self::buffer_size).
    pub fn with_buffer_size(mut self, buffer_size: usize) -> Self {
        self.buffer_size = buffer_size;
        self
    }

    /// Set [`max_output`](Self::max_output).
    pub fn with_max_output(mut self, max_output: u64) -> Self {
        self.max_output = Some(max_output);
        self
    }
}

/// Decode `body`, whose codings are `codings` in the order applied, as
/// [`ContentCoding::parse_list`] returns them.
///
/// Fails with [`DecodeError::Unsupported`] for [`ContentCoding::Other`], a
/// coding whose feature is off, or more than [`MAX_LAYERS`] codings. With no
/// coding, the body passes through unchanged, still held to
/// [`max_output`](DecodeOptions::max_output).
///
/// The stream fails with [`DecodeError`], converted into `E`:
///
/// - [`Invalid`](DecodeError::Invalid) for a corrupt body, or one that ends
///   inside its coding. An empty body is an empty file.
/// - [`TrailingData`](DecodeError::TrailingData) for data after the end of
///   the coding. gzip members and zstd frames may follow each other; trailing
///   data that cannot start one is still an error.
/// - [`TooLarge`](DecodeError::TooLarge) past `max_output`. No byte past the
///   limit is yielded.
///
/// ```
/// use bolt_load_decode::{ContentCoding, DecodeError, DecodeOptions, decode};
/// use bytes::Bytes;
/// use futures::{StreamExt, executor::block_on, stream};
///
/// // `hello`, gzip-coded.
/// let coded = Bytes::from_static(&[
///     0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xcb, 0x48, 0xcd, 0xc9, 0xc9,
///     0x07, 0x00, 0x86, 0xa6, 0x10, 0x36, 0x05, 0x00, 0x00, 0x00,
/// ]);
/// let body = stream::iter([Ok::<_, DecodeError>(coded)]);
/// let codings = ContentCoding::parse_list("gzip");
/// let decoded = decode(body, &codings, DecodeOptions::default())?;
/// let chunks: Vec<_> = block_on(decoded.collect());
/// assert_eq!(chunks.into_iter().collect::<Result<Vec<_>, _>>()?, [Bytes::from("hello")]);
/// # Ok::<(), DecodeError>(())
/// ```
pub fn decode<S, E>(
    body: S,
    codings: &[ContentCoding],
    options: DecodeOptions,
) -> Result<Decoded<S>, DecodeError>
where
    S: Stream<Item = Result<bytes::Bytes, E>>,
    E: From<DecodeError>,
{
    if codings.len() > MAX_LAYERS {
        let codings: Vec<_> = codings.iter().map(ContentCoding::as_str).collect();
        return Err(DecodeError::Unsupported {
            coding: codings.join(", "),
        });
    }
    let codecs = codings
        .iter()
        .rev()
        .map(Codec::new)
        .collect::<Result<_, _>>()?;
    Ok(Decoded::new(body, codecs, &options))
}
