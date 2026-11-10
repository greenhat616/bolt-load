//! The CRCs of the blocks not written in full yet (design §11.5.6).

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU64,
};

use super::super::file_writer::CrcSegment;

/// The most segments one block keeps before its CRC is read back.
pub(crate) const MAX_SEGMENTS_PER_BLOCK: usize = 64;
/// The most segments all blocks keep together; past it, the block with the
/// most segments is read back.
pub(crate) const MAX_SEGMENTS: usize = 16384;

/// How the CRC of a complete block is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockCrc {
    /// From the CRCs of the writes, combined.
    Known(u32),
    /// Read back from the file, once its data is synced.
    ReadBack,
}

/// The CRC segments of every block that is not complete yet, and the blocks
/// that fell back to reading their CRC back.
///
/// Each block keeps its segments sorted by offset, neither overlapping nor
/// adjacent: a write that completes is merged at once with the segments it
/// touches, with `crc32c_combine`. A block written in order has one
/// segment, and a complete block has one that covers it, its CRC.
#[derive(Debug)]
pub(crate) struct BlockCrcs {
    block_size: u64,
    blocks: BTreeMap<u64, Vec<CrcSegment>>,
    read_back: BTreeSet<u64>,
    /// The segments of all blocks together.
    segments: usize,
}

impl BlockCrcs {
    pub(crate) fn new(block_size: NonZeroU64) -> Self {
        Self {
            block_size: block_size.get(),
            blocks: BTreeMap::new(),
            read_back: BTreeSet::new(),
            segments: 0,
        }
    }

    /// Add the CRC of a successful write's bytes in one block.
    ///
    /// Writes never overlap (design §14.21); a segment that would is a bug.
    pub(crate) fn add(&mut self, segment: CrcSegment) {
        if segment.len == 0 {
            return;
        }
        let index = segment.offset / self.block_size;
        debug_assert_eq!(
            (segment.range().end - 1) / self.block_size,
            index,
            "a segment within one block"
        );
        if self.read_back.contains(&index) {
            return;
        }
        let segments = self.blocks.entry(index).or_default();
        let at = segments.partition_point(|s| s.offset < segment.offset);
        debug_assert!(
            at == 0 || segments[at - 1].range().end <= segment.offset,
            "{segment:?} overlaps {:?}",
            segments[at - 1]
        );
        debug_assert!(
            at == segments.len() || segment.range().end <= segments[at].offset,
            "{segment:?} overlaps {:?}",
            segments[at]
        );
        let mut merged = segment;
        let mut first = at;
        if at > 0 && segments[at - 1].range().end == merged.offset {
            first = at - 1;
            merged = combine(segments[first], merged);
        }
        let mut last = at;
        if at < segments.len() && merged.range().end == segments[at].offset {
            merged = combine(merged, segments[at]);
            last = at + 1;
        }
        segments.splice(first..last, [merged]);
        let count = segments.len();
        self.segments = self.segments + 1 - (last - first);
        if count > MAX_SEGMENTS_PER_BLOCK {
            self.read_back(index);
        }
        if self.segments > MAX_SEGMENTS {
            let fullest = self
                .blocks
                .iter()
                .max_by_key(|(_, segments)| segments.len())
                .map(|(&index, _)| index)
                .expect("segments are kept");
            self.read_back(fullest);
        }
    }

    /// Drop the segments of block `index`: its CRC is read back.
    fn read_back(&mut self, index: u64) {
        if let Some(segments) = self.blocks.remove(&index) {
            self.segments -= segments.len();
        }
        self.read_back.insert(index);
    }

    /// The CRC of the complete block `index`, `len` bytes long, which the
    /// blocks forget.
    pub(crate) fn take_complete(&mut self, index: u64, len: u64) -> BlockCrc {
        if self.read_back.remove(&index) {
            return BlockCrc::ReadBack;
        }
        let segments = self.blocks.remove(&index).unwrap_or_default();
        self.segments -= segments.len();
        match segments.as_slice() {
            [whole] if whole.offset == index * self.block_size && u64::from(whole.len) == len => {
                BlockCrc::Known(whole.crc)
            }
            other => {
                // Only a bug leaves a complete block without the one
                // segment that covers it; reading it back stays correct.
                debug_assert!(false, "complete block {index} has segments {other:?}");
                BlockCrc::ReadBack
            }
        }
    }

    /// The segments all blocks keep together.
    #[cfg(test)]
    pub(crate) fn segment_count(&self) -> usize {
        self.segments
    }

    /// The segments of block `index`.
    #[cfg(test)]
    pub(crate) fn segments_of(&self, index: u64) -> &[CrcSegment] {
        self.blocks.get(&index).map_or(&[], Vec::as_slice)
    }

    /// Whether block `index` reads its CRC back.
    #[cfg(test)]
    pub(crate) fn reads_back(&self, index: u64) -> bool {
        self.read_back.contains(&index)
    }
}

/// `left` followed by `right`, which starts where it ends.
fn combine(left: CrcSegment, right: CrcSegment) -> CrcSegment {
    debug_assert_eq!(left.range().end, right.offset);
    CrcSegment {
        offset: left.offset,
        len: left.len + right.len,
        crc: crc32c::crc32c_combine(left.crc, right.crc, right.len as usize),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const BLOCK: u64 = 64;

    fn crcs() -> BlockCrcs {
        BlockCrcs::new(NonZeroU64::new(BLOCK).unwrap())
    }

    /// The bytes of a file whose every byte differs from its neighbours.
    fn content(len: u64) -> Vec<u8> {
        (0..len)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 7) as u8)
            .collect()
    }

    fn segment(data: &[u8], offset: u64, len: u64) -> CrcSegment {
        CrcSegment {
            offset,
            len: len as u32,
            crc: crc32c::crc32c(&data[offset as usize..(offset + len) as usize]),
        }
    }

    /// A block written in order keeps one segment, which ends up as its CRC.
    #[test]
    fn test_writes_in_order_keep_one_segment() {
        let data = content(BLOCK);
        let mut crcs = crcs();
        for offset in (0..BLOCK).step_by(8) {
            crcs.add(segment(&data, offset, 8));
            assert_eq!(crcs.segments_of(0).len(), 1);
        }
        assert_eq!(
            crcs.take_complete(0, BLOCK),
            BlockCrc::Known(crc32c::crc32c(&data))
        );
        assert_eq!(crcs.segment_count(), 0);
    }

    /// A hole splits the block into two segments; filling it merges them
    /// into one with both neighbours.
    #[test]
    fn test_a_write_merges_with_both_neighbours() {
        let data = content(2 * BLOCK);
        let mut crcs = crcs();
        crcs.add(segment(&data, BLOCK, 10));
        crcs.add(segment(&data, BLOCK + 20, 44));
        assert_eq!(crcs.segments_of(1).len(), 2);
        assert_eq!(crcs.segment_count(), 2);
        crcs.add(segment(&data, BLOCK + 10, 10));
        assert_eq!(crcs.segments_of(1), [segment(&data, BLOCK, BLOCK)]);
        assert_eq!(crcs.segment_count(), 1);
        assert_eq!(
            crcs.take_complete(1, BLOCK),
            BlockCrc::Known(crc32c::crc32c(&data[BLOCK as usize..]))
        );
    }

    proptest! {
        /// Whatever order the writes of a block complete in, the block ends
        /// with the CRC of its bytes, and every segment kept on the way is
        /// the CRC of the bytes it covers, with no two touching.
        #[test]
        fn test_segments_merge_in_any_order(
            cuts in prop::collection::btree_set(1..BLOCK, 0..20),
            order in any::<u64>(),
            last_len in 1..=BLOCK,
        ) {
            let data = content(BLOCK + last_len);
            // Writes of block 0 and of the short last block, block 1.
            let mut bounds: Vec<u64> = std::iter::once(0).chain(cuts).chain([BLOCK]).collect();
            bounds.dedup();
            let mut writes: Vec<(u64, u64)> =
                bounds.windows(2).map(|pair| (pair[0], pair[1] - pair[0])).collect();
            writes.push((BLOCK, last_len));
            // A shuffle the seed decides.
            let mut seed = order;
            for i in (1..writes.len()).rev() {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                writes.swap(i, (seed >> 33) as usize % (i + 1));
            }
            let mut crcs = crcs();
            for (offset, len) in writes {
                crcs.add(segment(&data, offset, len));
                for index in [0, 1] {
                    let kept = crcs.segments_of(index);
                    for pair in kept.windows(2) {
                        prop_assert!(pair[0].range().end < pair[1].offset, "{kept:?}");
                    }
                    for kept in kept {
                        prop_assert_eq!(*kept, segment(&data, kept.offset, u64::from(kept.len)));
                    }
                }
            }
            prop_assert_eq!(
                crcs.take_complete(0, BLOCK),
                BlockCrc::Known(crc32c::crc32c(&data[..BLOCK as usize]))
            );
            prop_assert_eq!(
                crcs.take_complete(1, last_len),
                BlockCrc::Known(crc32c::crc32c(&data[BLOCK as usize..]))
            );
            prop_assert_eq!(crcs.segment_count(), 0);
        }
    }

    /// A block with more than 64 segments drops them and reads its CRC back;
    /// its later writes add none.
    #[test]
    fn test_a_block_past_its_cap_reads_back() {
        let block = 1 << 20;
        let data = content(block);
        let mut crcs = BlockCrcs::new(NonZeroU64::new(block).unwrap());
        for i in 0..MAX_SEGMENTS_PER_BLOCK as u64 {
            crcs.add(segment(&data, 2 * i, 1));
        }
        assert_eq!(crcs.segments_of(0).len(), MAX_SEGMENTS_PER_BLOCK);
        assert!(!crcs.reads_back(0));
        crcs.add(segment(&data, 2 * MAX_SEGMENTS_PER_BLOCK as u64, 1));
        assert!(crcs.reads_back(0));
        assert_eq!(crcs.segment_count(), 0);
        crcs.add(segment(&data, 1, 1));
        assert_eq!(crcs.segment_count(), 0);
        assert_eq!(crcs.take_complete(0, block), BlockCrc::ReadBack);
        assert!(!crcs.reads_back(0));
    }

    /// Past 16384 segments in all, the block with the most reads back, and
    /// the others keep theirs.
    #[test]
    fn test_all_blocks_past_the_cap_read_back_the_fullest() {
        const BLOCK: u64 = 256;
        let mut crcs = BlockCrcs::new(NonZeroU64::new(BLOCK).unwrap());
        let data = content(BLOCK);
        let per_block = 32u64;
        let blocks = (MAX_SEGMENTS as u64).div_ceil(per_block);
        // Every block but block 7 has 32 segments, which totals the cap.
        let mut added = 0;
        for index in 0..blocks {
            let count = if index == 7 { per_block - 1 } else { per_block };
            for i in 0..count {
                let offset = 2 * i;
                let crc = segment(&data, offset, 1).crc;
                crcs.add(CrcSegment {
                    offset: index * BLOCK + offset,
                    len: 1,
                    crc,
                });
                added += 1;
            }
        }
        assert!(added <= MAX_SEGMENTS);
        assert_eq!(crcs.segment_count(), added);
        // Block 7 grows to 32, then block 3 to 33: the most, over the cap.
        let pad = MAX_SEGMENTS - added;
        for i in 0..pad as u64 {
            crcs.add(CrcSegment {
                offset: 7 * BLOCK + 2 * (per_block - 1 + i),
                len: 1,
                crc: 0,
            });
        }
        assert_eq!(crcs.segment_count(), MAX_SEGMENTS);
        crcs.add(CrcSegment {
            offset: 3 * BLOCK + 2 * per_block,
            len: 1,
            crc: 0,
        });
        assert!(crcs.reads_back(3));
        assert_eq!(crcs.segment_count(), MAX_SEGMENTS - per_block as usize);
        assert_eq!(crcs.segments_of(4).len(), per_block as usize);
        assert!(!crcs.reads_back(4));
    }
}
