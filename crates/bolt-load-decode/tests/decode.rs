use std::{
    io,
    task::{Context, Poll},
};

use bolt_load_decode::{ContentCoding, DecodeError, DecodeOptions, MAX_LAYERS, decode};
use bytes::Bytes;
use compression_codecs::{
    DeflateEncoder, EncodeV2, GzipEncoder, ZlibEncoder,
    core::{
        Level,
        util::{PartialBuffer, WriteBuffer},
    },
};
use futures::{Stream, StreamExt, executor::block_on, stream};
use proptest::prelude::*;

/// Encode `data` once with `encoder`.
fn encode(mut encoder: impl EncodeV2, data: &[u8]) -> Vec<u8> {
    let mut coded = Vec::new();
    let mut scratch = [0; 4096];
    let mut input = PartialBuffer::new(data);
    while !input.unwritten().is_empty() {
        let mut output = WriteBuffer::new_initialized(&mut scratch);
        encoder.encode(&mut input, &mut output).unwrap();
        coded.extend_from_slice(output.written());
    }
    loop {
        let mut output = WriteBuffer::new_initialized(&mut scratch);
        let done = encoder.finish(&mut output).unwrap();
        coded.extend_from_slice(output.written());
        if done {
            return coded;
        }
    }
}

fn gzip(data: &[u8]) -> Vec<u8> {
    encode(GzipEncoder::new(Level::Default.into()), data)
}

fn zlib(data: &[u8]) -> Vec<u8> {
    encode(ZlibEncoder::new(Level::Default.into()), data)
}

fn raw_deflate(data: &[u8]) -> Vec<u8> {
    encode(DeflateEncoder::new(Level::Default.into()), data)
}

#[cfg(feature = "br")]
fn brotli(data: &[u8]) -> Vec<u8> {
    use compression_codecs::{BrotliEncoder, brotli::params::EncoderParams};
    encode(BrotliEncoder::new(EncoderParams::default()), data)
}

#[cfg(feature = "zstd")]
fn zstd(data: &[u8]) -> Vec<u8> {
    encode(compression_codecs::ZstdEncoder::new(3), data)
}

/// Split `coded` into chunks of the given sizes, then one with the rest.
fn chunks(coded: &[u8], sizes: &[usize]) -> Vec<Result<Bytes, DecodeError>> {
    let mut rest = Bytes::copy_from_slice(coded);
    let mut chunks = Vec::new();
    for &size in sizes {
        chunks.push(Ok(rest.split_to(size.min(rest.len()))));
    }
    chunks.push(Ok(rest));
    chunks
}

/// Decode, and check that no chunk is longer than the buffer.
fn decode_with(
    body: impl Stream<Item = Result<Bytes, DecodeError>>,
    codings: &str,
    options: DecodeOptions,
) -> Result<Vec<u8>, DecodeError> {
    let buffer_size = options.buffer_size.max(1);
    let decoded = decode(body, &ContentCoding::parse_list(codings), options)?;
    let mut out = Vec::new();
    for chunk in block_on(decoded.collect::<Vec<_>>()) {
        let chunk = chunk?;
        assert!(!chunk.is_empty());
        assert!(
            chunk.len() <= buffer_size,
            "{} > {buffer_size}",
            chunk.len()
        );
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn decode_all(coded: &[u8], codings: &str) -> Result<Vec<u8>, DecodeError> {
    decode_with(
        stream::iter(chunks(coded, &[])),
        codings,
        DecodeOptions::default(),
    )
}

fn sample(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 % 251) as u8).collect()
}

type Encode = fn(&[u8]) -> Vec<u8>;

/// Each coding this build decodes, with an encoder for it.
fn encodings() -> Vec<(&'static str, Encode)> {
    #[cfg_attr(not(any(feature = "br", feature = "zstd")), allow(unused_mut))]
    let mut encodings: Vec<(&'static str, Encode)> =
        vec![("gzip", gzip), ("deflate", zlib), ("deflate", raw_deflate)];
    #[cfg(feature = "br")]
    encodings.push(("br", brotli));
    #[cfg(feature = "zstd")]
    encodings.push(("zstd", zstd));
    encodings
}

fn data() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        prop::collection::vec(any::<u8>(), 0..20_000),
        // Compresses well, so the output fills many buffers.
        (1..50_000usize, any::<u8>()).prop_map(|(len, byte)| vec![byte; len]),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn decodes_any_split(
        data in data(),
        sizes in prop::collection::vec(0..2_000usize, 0..40),
        buffer_size in 0..5_000usize,
    ) {
        for (coding, encode) in encodings() {
            let coded = encode(&data);
            let options = DecodeOptions::default().with_buffer_size(buffer_size);
            let decoded = decode_with(stream::iter(chunks(&coded, &sizes)), coding, options)
                .map_err(|err| TestCaseError::fail(format!("{coding}: {err}")))?;
            prop_assert!(decoded == data, "{coding}: decoded {} of {} bytes", decoded.len(), data.len());
        }
    }

    #[test]
    fn decodes_two_layers(data in data(), sizes in prop::collection::vec(0..500usize, 0..20)) {
        let coded = zlib(&gzip(&data));
        let decoded = decode_with(
            stream::iter(chunks(&coded, &sizes)),
            "gzip, deflate",
            DecodeOptions::default().with_buffer_size(1_000),
        )
        .map_err(|err| TestCaseError::fail(err.to_string()))?;
        prop_assert_eq!(decoded, data);
    }
}

#[test]
fn decodes_byte_by_byte() {
    let data = sample(3_000);
    for (coding, encode) in encodings() {
        let coded = encode(&data);
        let body = stream::iter(
            coded
                .iter()
                .map(|&byte| Ok(Bytes::copy_from_slice(&[byte]))),
        );
        let decoded = decode_with(body, coding, DecodeOptions::default()).unwrap();
        assert_eq!(decoded, data, "{coding}");
    }
}

#[test]
fn hands_out_decoded_bytes_while_the_body_waits() {
    let data = sample(10_000);
    let coded = Bytes::from(gzip(&data));
    let mut parts = vec![
        coded.slice(..coded.len() / 2),
        coded.slice(coded.len() / 2..),
    ]
    .into_iter();
    let mut pending = false;
    let body = stream::poll_fn(move |cx: &mut Context<'_>| {
        pending = !pending;
        if pending {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        Poll::Ready(parts.next().map(Ok::<_, DecodeError>))
    });
    let decoded = decode_with(body, "gzip", DecodeOptions::default()).unwrap();
    assert_eq!(decoded, data);
}

#[test]
fn bounds_each_chunk_for_a_body_that_compresses_well() {
    let data = vec![0; 8 << 20];
    let coded = gzip(&data);
    assert!(coded.len() < 64 << 10);
    let decoded = decode_with(
        stream::iter(chunks(&coded, &[])),
        "gzip",
        DecodeOptions::default(),
    )
    .unwrap();
    assert_eq!(decoded.len(), data.len());
}

#[test]
fn passes_a_body_without_coding_through() {
    let data = sample(1_000);
    assert_eq!(decode_all(&data, "").unwrap(), data);
    assert_eq!(decode_all(&data, "identity").unwrap(), data);
}

#[test]
fn decodes_an_empty_body_as_an_empty_file() {
    for coding in ["gzip", "deflate", "gzip, deflate"] {
        let body = stream::iter(Vec::<Result<Bytes, DecodeError>>::new());
        assert_eq!(
            decode_with(body, coding, DecodeOptions::default()).unwrap(),
            b"",
            "{coding}"
        );
    }
}

#[test]
fn decodes_several_gzip_members() {
    let coded = [gzip(b"hello, "), gzip(b""), gzip(b"world")].concat();
    assert_eq!(decode_all(&coded, "gzip").unwrap(), b"hello, world");
}

#[cfg(feature = "zstd")]
#[test]
fn decodes_several_zstd_frames() {
    // A skippable frame (RFC 8878 §3.1.2) between two frames.
    let skippable = [0x50, 0x2a, 0x4d, 0x18, 3, 0, 0, 0, 1, 2, 3];
    let coded = [zstd(b"hello, "), skippable.to_vec(), zstd(b"world")].concat();
    assert_eq!(decode_all(&coded, "zstd").unwrap(), b"hello, world");
}

#[test]
fn rejects_data_after_the_end() {
    let data = sample(1_000);
    for (coding, coded) in [
        ("gzip", [gzip(&data), b"\0junk".to_vec()].concat()),
        ("deflate", [zlib(&data), b"x".to_vec()].concat()),
        ("deflate", [raw_deflate(&data), b"x".to_vec()].concat()),
    ] {
        let err = decode_all(&coded, coding).unwrap_err();
        assert!(
            matches!(err, DecodeError::TrailingData),
            "{coding}: {err:?}"
        );
    }
}

#[test]
fn rejects_a_truncated_body() {
    let data = sample(5_000);
    for (coding, encode) in encodings() {
        let coded = encode(&data);
        for len in [1, coded.len() / 2, coded.len() - 1] {
            let err = decode_all(&coded[..len], coding).unwrap_err();
            assert!(
                matches!(&err, DecodeError::Invalid { source } if source.kind() == io::ErrorKind::UnexpectedEof),
                "{coding} cut at {len}: {err:?}"
            );
        }
    }
}

#[test]
fn rejects_a_corrupt_body() {
    let data = sample(5_000);
    for (coding, coded) in [("gzip", gzip(&data)), ("deflate", zlib(&data))] {
        // The last bytes are a checksum: CRC-32 for gzip, Adler-32 for zlib.
        let mut corrupt = coded.clone();
        let at = corrupt.len() - 6;
        corrupt[at] ^= 0xff;
        let err = decode_all(&corrupt, coding).unwrap_err();
        assert!(
            matches!(err, DecodeError::Invalid { .. }),
            "{coding}: {err:?}"
        );
    }
    let err = decode_all(b"not gzip at all", "gzip").unwrap_err();
    assert!(matches!(err, DecodeError::Invalid { .. }), "{err:?}");
}

#[test]
fn stops_at_max_output() {
    let data = sample(100_000);
    let coded = gzip(&data);
    let options = DecodeOptions::default()
        .with_buffer_size(1_000)
        .with_max_output(50_500);
    let decoded = decode(
        stream::iter(chunks(&coded, &[])),
        &ContentCoding::parse_list("gzip"),
        options.clone(),
    )
    .unwrap();
    let items = block_on(decoded.collect::<Vec<_>>());
    let (last, before) = items.split_last().unwrap();
    assert!(
        matches!(last, Err(DecodeError::TooLarge { limit: 50_500 })),
        "{last:?}"
    );
    let yielded: usize = before
        .iter()
        .map(|chunk| chunk.as_ref().unwrap().len())
        .sum();
    assert_eq!(yielded, 50_000);

    let exact = options.with_max_output(data.len() as u64);
    assert_eq!(
        decode_with(stream::iter(chunks(&coded, &[])), "gzip", exact).unwrap(),
        data
    );
}

#[test]
fn passes_body_errors_through() {
    let coded = Bytes::from(gzip(&sample(1_000)));
    let body = stream::iter([
        Ok(coded.slice(..10)),
        Err(DecodeError::TooLarge { limit: 7 }),
        Ok(coded.slice(10..)),
    ]);
    let err = decode_with(body, "gzip", DecodeOptions::default()).unwrap_err();
    assert!(matches!(err, DecodeError::TooLarge { limit: 7 }), "{err:?}");
}

#[test]
fn rejects_codings_it_cannot_decode() {
    let unsupported = |codings: &str| {
        let body = stream::iter(Vec::<Result<Bytes, DecodeError>>::new());
        match decode(
            body,
            &ContentCoding::parse_list(codings),
            DecodeOptions::default(),
        ) {
            Err(DecodeError::Unsupported { coding }) => coding,
            other => panic!("{codings}: {other:?}"),
        }
    };
    assert_eq!(unsupported("compress"), "compress");
    assert_eq!(unsupported("gzip, x-custom"), "x-custom");
    let layers = ["gzip"; MAX_LAYERS + 1].join(", ");
    assert_eq!(unsupported(&layers), layers);
    #[cfg(not(feature = "br"))]
    assert_eq!(unsupported("br"), "br");
    #[cfg(not(feature = "zstd"))]
    assert_eq!(unsupported("zstd"), "zstd");
}

#[test]
fn drains_held_output_after_the_last_chunk() {
    // A small coded body that decodes to far more than the buffer: the
    // decoder takes all input at once and holds most of the output.
    for data in [vec![0; 300_000], sample(300_000)] {
        for (coding, encode) in encodings() {
            let coded = encode(&data);
            for buffer_size in [7, 1_000, 70_000] {
                let options = DecodeOptions::default().with_buffer_size(buffer_size);
                let decoded = decode_with(stream::iter(chunks(&coded, &[])), coding, options)
                    .unwrap_or_else(|err| panic!("{coding} buffer {buffer_size}: {err}"));
                assert!(decoded == data, "{coding} buffer {buffer_size}");
            }
        }
    }
}

#[test]
fn hands_out_all_it_can_before_the_body_continues() {
    // The body sends everything but its end, then waits: all decoded bytes
    // must come out without more input.
    let data = sample(300_000);
    for (coding, encode) in encodings() {
        let mut coded = Some(Bytes::from(encode(&data)));
        let body = stream::poll_fn(move |_: &mut Context<'_>| match coded.take() {
            Some(coded) => Poll::Ready(Some(Ok::<_, DecodeError>(coded))),
            None => Poll::Pending,
        });
        let options = DecodeOptions::default().with_buffer_size(1_000);
        let mut decoded = decode(body, &ContentCoding::parse_list(coding), options).unwrap();
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        let mut out = Vec::new();
        while let Poll::Ready(Some(chunk)) = decoded.poll_next_unpin(&mut cx) {
            out.extend_from_slice(&chunk.unwrap());
        }
        assert!(
            out == data,
            "{coding}: {} of {} bytes",
            out.len(),
            data.len()
        );
    }
}
