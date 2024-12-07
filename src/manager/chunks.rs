//! a mod maintains the x dimension of the chunks
//! It is used to maintain the progress of the download, and the ranges of the chunks

use std::{
    collections::HashMap,
    ops::{Bound, Range, RangeBounds},
    sync::Mutex,
};

use ranges::{GenericRange, OperationResult, Ranges};
/// the main progress of the download
pub struct Progress {
    /// the total size of the content
    /// possible None if the total size is unknown
    /// It requires the single-thread task can finished until the stream is None
    pub total: Option<u64>,

    /// the current downloaded size
    pub current: u64,
}

/// the chunks of the downloads, only used in multi-thread mode
/// It should be hold by the task manager, and should not be cloned
pub struct ChunkPlanner {
    /// the total number of chunks
    pub total: u64,
    /// hold occupied chunks
    chunks: Mutex<HashMap<GenericRange<u64>, usize>>,
}

impl ChunkPlanner {
    pub fn new(total: u64) -> Self {
        Self {
            total,
            chunks: Mutex::new(HashMap::new()),
        }
    }

    /// add the chunk
    pub fn add_chunk(&self, range: Range<u64>, id: usize) -> bool {
        let range = GenericRange::from(range);
        // first check whether the range is overlapped with the existing chunks
        let mut chunks = self.chunks.lock().unwrap();
        for (chunk, _) in chunks.iter() {
            let res = chunk.intersect(range);
            if res != OperationResult::Empty {
                return false;
            }
        }
        // check whether the range is out of the total range
        match range.end_bound() {
            Bound::Excluded(end) => {
                if *end > self.total {
                    return false;
                }
            }
            _ => unreachable!(),
        }
        chunks.insert(range, id);
        true
    }

    /// remove the chunk
    pub fn remove_chunk(&self, range: Range<u64>) -> bool {
        let range = GenericRange::from(range);
        self.chunks.lock().unwrap().remove(&range).is_some()
    }

    /// get the occupied ranges
    pub fn get_occupied_ranges(&self) -> Vec<GenericRange<u64>> {
        let mut ranges: Vec<_> = self.chunks.lock().unwrap().keys().cloned().collect();
        ranges.sort_by_key(|v| match v.start_bound() {
            Bound::Included(start) => *start,
            _ => unreachable!(),
        });
        ranges
    }

    /// get the available chunks ranges
    pub fn get_available_ranges(&self) -> Vec<Range<u64>> {
        let chunks = Ranges::from_iter(self.chunks.lock().unwrap().keys().cloned());
        let full_range = Ranges::from(GenericRange::from(0..self.total));
        let available_ranges = full_range - chunks;
        let mut ranges: Vec<_> = available_ranges
            .as_slice()
            .iter()
            .map(|v| {
                let start = v.start_bound();
                let end = v.end_bound();
                match (start, end) {
                    (Bound::Included(start), Bound::Excluded(end)) => *start..*end,
                    _ => unreachable!(),
                }
            })
            .collect();
        ranges.sort_by_key(|v| v.start);
        ranges
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use test_log::test;

    #[test]
    fn test_add_chunk() {
        let chunk_strategy = ChunkPlanner::new(100);
        assert!(chunk_strategy.add_chunk(50..55, 0));
        assert!(chunk_strategy.add_chunk(30..40, 1));
        assert!(!chunk_strategy.add_chunk(50..60, 2));
        assert!(!chunk_strategy.add_chunk(30..40, 3));
        assert_eq!(
            chunk_strategy.get_occupied_ranges(),
            vec![GenericRange::from(30..40), GenericRange::from(50..55)]
        );
    }

    #[test]
    fn test_remove_chunk() {
        let chunk_strategy = ChunkPlanner::new(100);
        assert!(chunk_strategy.add_chunk(50..55, 0));
        assert!(chunk_strategy.remove_chunk(50..55));
        assert!(!chunk_strategy.remove_chunk(50..55));
        assert_eq!(chunk_strategy.get_occupied_ranges(), vec![]);
    }

    #[test]
    fn test_get_available_ranges() {
        let chunk_strategy = ChunkPlanner::new(100);
        assert!(chunk_strategy.add_chunk(50..55, 0));
        assert!(chunk_strategy.add_chunk(30..40, 1));
        let available_chunks = chunk_strategy.get_available_ranges();
        assert_eq!(available_chunks, vec![0..30, 40..50, 55..100]);
    }
}
