//! One decoder per content coding, on top of `compression-codecs`.

use std::io;

use bolt_load_http::ContentCoding;
#[cfg(feature = "br")]
use compression_codecs::BrotliDecoder;
#[cfg(feature = "gzip")]
use compression_codecs::GzipDecoder;
#[cfg(feature = "zstd")]
use compression_codecs::ZstdDecoder;
use compression_codecs::{
    DecodeV2,
    core::util::{PartialBuffer, WriteBuffer},
};
#[cfg(feature = "deflate")]
use compression_codecs::{DeflateDecoder, ZlibDecoder};

use crate::DecodeError;

/// The largest zstd window to accept, as a power of two: 8 MiB, the limit
/// RFC 9659 §3 sets for the HTTP `zstd` coding.
#[cfg(feature = "zstd")]
const ZSTD_WINDOW_LOG_MAX: u32 = 23;

#[derive(Debug)]
pub(crate) enum Codec {
    #[cfg(feature = "gzip")]
    Gzip(GzipDecoder),
    #[cfg(feature = "deflate")]
    Deflate(Deflate),
    #[cfg(feature = "br")]
    Brotli(BrotliDecoder),
    #[cfg(feature = "zstd")]
    Zstd(ZstdDecoder),
}

impl Codec {
    pub(crate) fn new(coding: &ContentCoding) -> Result<Self, DecodeError> {
        match coding {
            #[cfg(feature = "gzip")]
            ContentCoding::Gzip => Ok(Self::Gzip(GzipDecoder::new())),
            #[cfg(feature = "deflate")]
            ContentCoding::Deflate => Ok(Self::Deflate(Deflate::default())),
            #[cfg(feature = "br")]
            ContentCoding::Brotli => Ok(Self::Brotli(BrotliDecoder::new())),
            #[cfg(feature = "zstd")]
            ContentCoding::Zstd => Ok(Self::Zstd(ZstdDecoder::new_with_params(&[
                compression_codecs::zstd::params::DParameter::window_log_max(ZSTD_WINDOW_LOG_MAX),
            ]))),
            coding => Err(DecodeError::Unsupported {
                coding: coding.to_string(),
            }),
        }
    }

    fn get(&mut self) -> &mut dyn DecodeV2 {
        match *self {
            #[cfg(feature = "gzip")]
            Self::Gzip(ref mut decoder) => decoder,
            #[cfg(feature = "deflate")]
            Self::Deflate(ref mut decoder) => decoder,
            #[cfg(feature = "br")]
            Self::Brotli(ref mut decoder) => decoder,
            #[cfg(feature = "zstd")]
            Self::Zstd(ref mut decoder) => decoder,
        }
    }

    /// Decode from `input`; `true` once the coded stream ends.
    pub(crate) fn decode(
        &mut self,
        input: &mut PartialBuffer<&[u8]>,
        output: &mut WriteBuffer<'_>,
    ) -> io::Result<bool> {
        self.get().decode(input, output)
    }

    /// Write out what the decoder holds without more input.
    pub(crate) fn flush(&mut self, output: &mut WriteBuffer<'_>) -> io::Result<()> {
        self.get().flush(output).map(drop)
    }

    /// End the stream after the last input; `true` once all output is out.
    /// Fails when the input stopped before the coded stream ended.
    pub(crate) fn finish(&mut self, output: &mut WriteBuffer<'_>) -> io::Result<bool> {
        self.get().finish(output)
    }

    /// Whether `byte` can start another member after the end of one: gzip
    /// allows several members (RFC 1952 §2.2) and zstd several frames
    /// (RFC 8878 §3.1), including skippable ones. Other codings end once.
    #[cfg_attr(
        not(any(feature = "gzip", feature = "zstd")),
        expect(unused_variables, reason = "only gzip and zstd have members")
    )]
    pub(crate) fn starts_member(&self, byte: u8) -> bool {
        match self {
            #[cfg(feature = "gzip")]
            Self::Gzip(_) => byte == 0x1f,
            #[cfg(feature = "zstd")]
            Self::Zstd(_) => byte == 0x28 || (0x50..=0x5f).contains(&byte),
            #[allow(unreachable_patterns)]
            _ => false,
        }
    }

    /// Get ready for the next member.
    pub(crate) fn reinit(&mut self) -> io::Result<()> {
        self.get().reinit()
    }
}

/// `deflate` is the zlib format (RFC 9110 §8.4.1.2), but some servers send
/// raw deflate. The first two bytes decide: a valid zlib header picks zlib,
/// anything else raw deflate, as browsers do.
#[cfg(feature = "deflate")]
#[derive(Debug, Default)]
pub(crate) struct Deflate {
    head: PartialBuffer<[u8; 2]>,
    /// How much of `head` the decoder has taken.
    fed: usize,
    decoder: Option<DeflateDecoderKind>,
}

#[cfg(feature = "deflate")]
#[derive(Debug)]
enum DeflateDecoderKind {
    Zlib(ZlibDecoder),
    Raw(DeflateDecoder),
}

#[cfg(feature = "deflate")]
impl DeflateDecoderKind {
    fn get(&mut self) -> &mut dyn DecodeV2 {
        match self {
            Self::Zlib(decoder) => decoder,
            Self::Raw(decoder) => decoder,
        }
    }
}

/// Whether `cmf` and `flg` form a zlib header (RFC 1950 §2.2): deflate
/// with a window of at most 32 KiB, and a valid check value.
#[cfg(feature = "deflate")]
fn is_zlib_header(cmf: u8, flg: u8) -> bool {
    cmf & 0x0f == 8 && cmf >> 4 <= 7 && (u16::from(cmf) << 8 | u16::from(flg)) % 31 == 0
}

#[cfg(feature = "deflate")]
impl Deflate {
    /// Pick the decoder once the first two bytes are in.
    fn select<'a>(
        decoder: &'a mut Option<DeflateDecoderKind>,
        head: &mut PartialBuffer<[u8; 2]>,
        input: &mut PartialBuffer<&[u8]>,
    ) -> Option<&'a mut DeflateDecoderKind> {
        if decoder.is_none() {
            head.copy_unwritten_from(input);
            let &mut [cmf, flg] = head.get_mut();
            if !head.unwritten().is_empty() {
                return None;
            }
            *decoder = Some(if is_zlib_header(cmf, flg) {
                DeflateDecoderKind::Zlib(ZlibDecoder::new())
            } else {
                DeflateDecoderKind::Raw(DeflateDecoder::new())
            });
        }
        decoder.as_mut()
    }
}

#[cfg(feature = "deflate")]
impl DecodeV2 for Deflate {
    fn reinit(&mut self) -> io::Result<()> {
        *self = Self::default();
        Ok(())
    }

    fn decode(
        &mut self,
        input: &mut PartialBuffer<&[u8]>,
        output: &mut WriteBuffer<'_>,
    ) -> io::Result<bool> {
        let Some(decoder) = Self::select(&mut self.decoder, &mut self.head, input) else {
            return Ok(false);
        };
        let decoder = decoder.get();
        if self.fed < self.head.written_len() {
            let mut head = PartialBuffer::new(&self.head.written()[self.fed..]);
            let ended = decoder.decode(&mut head, output)?;
            self.fed += head.written_len();
            if ended || self.fed < self.head.written_len() {
                return Ok(ended);
            }
        }
        if input.unwritten().is_empty() {
            return Ok(false);
        }
        decoder.decode(input, output)
    }

    fn flush(&mut self, output: &mut WriteBuffer<'_>) -> io::Result<bool> {
        match &mut self.decoder {
            Some(decoder) => decoder.get().flush(output),
            None => Ok(true),
        }
    }

    fn finish(&mut self, output: &mut WriteBuffer<'_>) -> io::Result<bool> {
        if self.decode(&mut PartialBuffer::new(&[][..]), output)? {
            return Ok(true);
        }
        match &mut self.decoder {
            Some(decoder) if self.fed == self.head.written_len() => decoder.get().finish(output),
            Some(_) => Ok(false),
            None => Err(io::ErrorKind::UnexpectedEof.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "deflate")]
    #[test]
    fn recognizes_zlib_headers() {
        // Default, fastest and best compression, from `ZlibEncoder`.
        for [cmf, flg] in [[0x78, 0x9c], [0x78, 0x01], [0x78, 0xda]] {
            assert!(is_zlib_header(cmf, flg), "{cmf:#x} {flg:#x}");
        }
        // Bad check value; window over 32 KiB; not deflate.
        for [cmf, flg] in [[0x78, 0x9d], [0x88, 0x98], [0x79, 0x9c]] {
            assert!(!is_zlib_header(cmf, flg), "{cmf:#x} {flg:#x}");
        }
    }

    #[test]
    fn rejects_unknown_codings() {
        let err = Codec::new(&ContentCoding::parse_list("compress")[0]).unwrap_err();
        assert!(
            matches!(&err, DecodeError::Unsupported { coding } if coding == "compress"),
            "{err:?}"
        );
    }
}
