//! a mod maintains the x dimension of the chunks
//! It is used to maintain the progress of the download, and the ranges of the chunks

use std::{
    collections::HashMap,
    num::NonZeroUsize,
    ops::{Bound, Range, RangeBounds},
};

use ranges::{GenericRange, OperationResult, Ranges};

use super::TaskId;

pub const DEFAULT_MIN_CHUNK_SIZE: u64 = 1024 * 1024; // 1MB

/// the chunks of the downloads, only used in multi-thread mode
/// It should be hold by the task manager, and should not be cloned
pub struct ChunkPlanner {
    /// the total size of the content
    pub total: u64,
    /// the minimal size of the chunk
    pub min_chunk_size: u64,
    /// the maximal count of the chunks
    pub max_chunk_count: Option<NonZeroUsize>,
    /// hold occupied chunks
    chunks: HashMap<GenericRange<u64>, TaskId>,
}

impl ChunkPlanner {
    pub fn new(total: u64) -> Self {
        Self {
            total,
            chunks: HashMap::new(),
            min_chunk_size: DEFAULT_MIN_CHUNK_SIZE,
            max_chunk_count: None,
        }
    }

    /// get the current chunks count
    pub fn get_chunks_count(&self) -> usize {
        self.chunks.keys().len()
    }

    /// check whether the range is occupied
    pub fn check_range(&self, range: Range<u64>) -> bool {
        let range = GenericRange::from(range);
        if self.chunks.contains_key(&range) {
            return false;
        }
        // first check whether the range is overlapped with the existing chunks
        for (chunk, _) in self.chunks.iter() {
            let res = chunk.intersect(range);
            if res != OperationResult::Empty {
                return false;
            }
        }
        // check whether the range is out of the total range
        let end = unwrap_range_end_bound(range.end_bound());
        if *end > self.total {
            return false;
        }
        true
    }

    /// add the chunk
    pub fn add_chunk(&mut self, range: Range<u64>, id: usize) -> bool {
        if let Some(max_chunk_count) = self.max_chunk_count {
            if self.get_chunks_count() >= max_chunk_count.get() {
                return false;
            }
        }
        if !self.check_range(range.clone()) {
            return false;
        }
        self.chunks.insert(range.into(), id);
        true
    }

    /// remove the chunk
    pub fn remove_chunk(&mut self, range: Range<u64>) -> bool {
        let range = GenericRange::from(range);
        self.chunks.remove(&range).is_some()
    }

    /// get the occupied ranges
    pub fn get_occupied_ranges(&self) -> Vec<Range<u64>> {
        let mut ranges: Vec<_> = self.chunks.keys().cloned().collect();
        ranges.sort_by_key(|v| *unwrap_range_start_bound(v.start_bound()));
        ranges
            .as_slice()
            .iter()
            .map(convert_generic_range_to_std_range)
            .collect()
    }

    /// get the available chunks ranges
    pub fn get_available_ranges(&self) -> Vec<Range<u64>> {
        let chunks = Ranges::from_iter(self.chunks.keys().cloned());
        let full_range = Ranges::from(GenericRange::from(0..self.total));
        let available_ranges = full_range - chunks;
        let mut ranges: Vec<_> = available_ranges
            .as_slice()
            .iter()
            .map(convert_generic_range_to_std_range)
            .collect();
        ranges.sort_by_key(|v| v.start);
        ranges
    }

    /// try to arrange a chunk by the length
    pub fn try_arrange_chunk_by_length(&self, length: u64) -> Option<Range<u64>> {
        let available_ranges = self.get_available_ranges();
        for range in available_ranges {
            if range.end - range.start >= length {
                return Some(range.start..range.start + length);
            }
        }
        None
    }

    /// add a chunk by the length,
    /// it will arrange the chunk to the available range with the smallest start
    pub fn add_chunk_by_length(&mut self, length: u64, id: usize) -> Option<Range<u64>> {
        if let Some(max_chunk_count) = self.max_chunk_count {
            if self.get_chunks_count() >= max_chunk_count.get() {
                return None;
            }
        }
        self.try_arrange_chunk_by_length(length).inspect(|range| {
            self.add_chunk(range.clone(), id);
        })
    }

    /// find the chunk range to be removed or resized
    /// return the task id that will be removed or resized, and the suggested range
    pub fn find_or_evict_chunk_range(
        &self,
        downloaded_range: &[Range<u64>],
        plan_size: u64,
    ) -> (Option<TaskId>, Option<Range<u64>>) {
        let range = self.try_arrange_chunk_by_length(plan_size);
        if let Some(range) = range {
            return (None, Some(range));
        };
        let downloaded_ranges =
            Ranges::from_iter(downloaded_range.iter().cloned().map(GenericRange::from));
        let full_range = Ranges::from(GenericRange::from(0..self.total));
        let available_ranges = full_range - downloaded_ranges;
        // find the first available range, that can be used to arrange the chunk
        for range in available_ranges.as_slice() {
            let start = unwrap_range_start_bound(range.start_bound());
            let end = unwrap_range_end_bound(range.end_bound());
            let size = end - start;

            if size >= self.min_chunk_size {
                // Find the task_id that intersects with this range
                let task_id = self
                    .chunks
                    .iter()
                    .find(|(k, _)| k.intersect(*range) != OperationResult::Empty)
                    .map(|(_, id)| *id)
                    .unwrap();

                // Determine the range to return
                let new_range = if size >= plan_size && size - plan_size > self.min_chunk_size {
                    *start..*start + plan_size
                // TODO: we should add a strategy whether to do so according to the progress of this chunk
                } else {
                    convert_generic_range_to_std_range(range)
                };

                return (Some(task_id), Some(new_range));
            }
        }
        (None, None)
    }

    /// Split the range into multiple chunks
    pub fn split_chunks(&mut self) {
        let range = GenericRange::from(0..self.total);
        let start = unwrap_range_start_bound(range.start_bound());
        let end = unwrap_range_end_bound(range.end_bound());
        let mut chunks_count: usize = ((end - start) / self.min_chunk_size) as usize;
        if let Some(max_chunk_count) = self.max_chunk_count {
            chunks_count = std::cmp::min(chunks_count, max_chunk_count.get());
        }
        for i in 0..chunks_count {
            let key = i;
            let i = i as u64;
            let chunk_range =
                *start + i * self.min_chunk_size..*start + (i + 1) * self.min_chunk_size;
            self.add_chunk(chunk_range, key);
        }
    }
}

/// It is safe to convert the generic range to the std range, because the normal range in Rust is [start, end)
fn convert_generic_range_to_std_range(range: &GenericRange<u64>) -> Range<u64> {
    let start = unwrap_range_start_bound(range.start_bound());
    let end = unwrap_range_end_bound(range.end_bound());
    *start..*end
}

/// It is safe to unwrap the end bound of the range, because the end bound in the Rust normal range is always Excluded
fn unwrap_range_end_bound<T>(bound: Bound<T>) -> T {
    match bound {
        Bound::Excluded(v) => v,
        _ => unreachable!(),
    }
}

/// It is safe to unwrap the start bound of the range, because the start bound in the Rust normal range is always Included
fn unwrap_range_start_bound<T>(bound: Bound<T>) -> T {
    match bound {
        Bound::Included(v) => v,
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use test_log::test;

    #[test]
    fn test_add_chunk() {
        let mut chunk_planner = ChunkPlanner::new(100);
        assert!(chunk_planner.add_chunk(50..55, 0));
        assert!(chunk_planner.add_chunk(30..40, 1));
        assert!(!chunk_planner.add_chunk(50..60, 2));
        assert!(!chunk_planner.add_chunk(30..40, 3));
        assert_eq!(chunk_planner.get_occupied_ranges(), vec![30..40, 50..55]);
    }

    #[test]
    fn test_remove_chunk() {
        let mut chunk_planner = ChunkPlanner::new(100);
        assert!(chunk_planner.add_chunk(50..55, 0));
        assert!(chunk_planner.remove_chunk(50..55));
        assert!(!chunk_planner.remove_chunk(50..55));
        assert_eq!(chunk_planner.get_occupied_ranges(), vec![]);
    }

    #[test]
    fn test_get_available_ranges() {
        let mut chunk_planner = ChunkPlanner::new(100);
        assert!(chunk_planner.add_chunk(50..55, 0));
        assert!(chunk_planner.add_chunk(30..40, 1));
        let available_chunks = chunk_planner.get_available_ranges();
        assert_eq!(available_chunks, vec![0..30, 40..50, 55..100]);
    }

    #[test]
    fn test_split_chunks() {
        let mut chunk_planner = ChunkPlanner::new(1024 * 1024 * 100);
        chunk_planner.split_chunks();
        assert_eq!(chunk_planner.get_chunks_count(), 100);

        let mut chunk_planner = ChunkPlanner::new(1024 * 1024 * 100);
        chunk_planner.max_chunk_count = NonZeroUsize::new(50);
        chunk_planner.split_chunks();
        assert_eq!(chunk_planner.get_chunks_count(), 50);
    }
}
