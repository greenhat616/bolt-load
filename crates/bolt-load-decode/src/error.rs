use std::{io, sync::Arc};

use snafu::prelude::*;

/// Why a content-coded body cannot be decoded. Every case is final:
/// fetching the same body again gives the same result.
#[derive(Debug, Snafu, Clone)]
#[snafu(visibility(pub(crate)))]
#[non_exhaustive]
pub enum DecodeError {
    /// A coding this build does not decode: [`ContentCoding::Other`], a
    /// coding whose feature is off, or more than [`MAX_LAYERS`] layers.
    /// `coding` is the coding, or the combined list for too many layers.
    ///
    /// [`ContentCoding::Other`]: crate::ContentCoding::Other
    /// [`MAX_LAYERS`]: crate::MAX_LAYERS
    #[snafu(display("unsupported content coding ({coding})"))]
    Unsupported { coding: String },

    /// The body is not valid in its coding, or ends before the coding does.
    #[snafu(display("invalid content-coded body: {source}"))]
    Invalid { source: Arc<io::Error> },

    /// Data follows the end of the coded body.
    #[snafu(display("data follows the end of the content-coded body"))]
    TrailingData,

    /// The decoded body is longer than [`DecodeOptions::max_output`].
    ///
    /// [`DecodeOptions::max_output`]: crate::DecodeOptions::max_output
    #[snafu(display("decoded body is longer than {limit} bytes"))]
    TooLarge { limit: u64 },
}

impl From<io::Error> for DecodeError {
    fn from(source: io::Error) -> Self {
        Self::Invalid {
            source: Arc::new(source),
        }
    }
}
