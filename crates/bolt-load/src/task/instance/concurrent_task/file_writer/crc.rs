//! The CRC32C of a written frame, split at block boundaries (design
//! §11.5.6).

use std::{num::NonZeroU64, ops::Range};

/// The CRC32C of `len` bytes written at `offset`, all within one block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrcSegment {
    pub offset: u64,
    pub len: u32,
    pub crc: u32,
}

impl CrcSegment {
    /// The byte range the segment covers.
    #[inline]
    pub fn range(&self) -> Range<u64> {
        self.offset..self.offset + u64::from(self.len)
    }
}

/// The segments of `data`, written at `range`: one per block of
/// `block_size` bytes the range touches, each with the CRC32C of its bytes.
///
/// `block_size` must fit in a `u32`, so that a segment's length does.
pub(crate) fn frame_segments(
    range: Range<u64>,
    data: &[u8],
    block_size: NonZeroU64,
) -> Vec<CrcSegment> {
    debug_assert_eq!(range.end - range.start, data.len() as u64);
    debug_assert!(block_size.get() <= u64::from(u32::MAX));
    let block_size = block_size.get();
    let mut segments = Vec::with_capacity(1);
    let mut offset = range.start;
    let mut rest = data;
    while !rest.is_empty() {
        let block_end = (offset / block_size + 1) * block_size;
        let len = (block_end - offset).min(rest.len() as u64) as usize;
        let (head, tail) = rest.split_at(len);
        segments.push(CrcSegment {
            offset,
            len: len as u32,
            crc: crc32c::crc32c(head),
        });
        offset += len as u64;
        rest = tail;
    }
    segments
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    const BLOCK: NonZeroU64 = NonZeroU64::new(8).unwrap();

    /// A frame within a block is one segment; one across a boundary is
    /// split there, each part with its own CRC; an empty one has none.
    #[test]
    fn test_a_frame_splits_at_block_boundaries() {
        let data: Vec<u8> = (0..20).collect();
        assert_eq!(
            frame_segments(2..7, &data[..5], BLOCK),
            [CrcSegment {
                offset: 2,
                len: 5,
                crc: crc32c::crc32c(&data[..5]),
            }]
        );
        let segments = frame_segments(6..26, &data, BLOCK);
        assert_eq!(
            segments.iter().map(CrcSegment::range).collect::<Vec<_>>(),
            [6..8, 8..16, 16..24, 24..26]
        );
        assert_eq!(segments[0].crc, crc32c::crc32c(&data[..2]));
        assert_eq!(segments[1].crc, crc32c::crc32c(&data[2..10]));
        assert_eq!(segments[3].crc, crc32c::crc32c(&data[18..]));
        assert_eq!(frame_segments(8..8, &[], BLOCK), []);
    }
}
