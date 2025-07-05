#![allow(dead_code)]

use std::{collections::HashMap, num::NonZeroUsize, ops::Range};

use ranges::{GenericRange, OperationResult, Ranges};

use crate::task::RunnerId;

pub const DEFAULT_MIN_CHUNK_SIZE: u64 = 1024 * 1024; // 1MB

/// Unified chunk state that tracks both allocation and download progress
#[derive(Clone, Debug)]
pub struct ChunkState {
    /// The originally allocated range for this chunk
    pub allocated: Range<u64>,
    /// The currently downloaded range within the allocated range
    pub downloaded: Range<u64>,
    /// The runner ID assigned to this chunk (if any)
    pub runner_id: Option<RunnerId>,
    /// Runner status
    pub status: ChunkStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChunkStatus {
    /// Chunk is allocated but no runner assigned
    Idle,
    /// Runner is actively downloading
    Running,
    /// Runner finished downloading this chunk
    Finished,
    /// Runner failed, chunk may be reassigned
    Failed,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Runner {0} not found")]
    RunnerNotFound(RunnerId),
    #[error("Chunk for runner {0} not found")]
    ChunkNotFound(RunnerId),
    #[error("Invalid split position: {0}")]
    InvalidSplitPosition(u64),
}

impl ChunkState {
    fn new(allocated: Range<u64>, runner_id: Option<RunnerId>) -> Self {
        Self {
            allocated: allocated.clone(),
            downloaded: allocated.start..allocated.start,
            runner_id,
            status: if runner_id.is_some() {
                ChunkStatus::Running
            } else {
                ChunkStatus::Idle
            },
        }
    }

    /// Get the remaining bytes to download
    pub fn remaining(&self) -> u64 {
        self.allocated.end - self.downloaded.end
    }

    /// Get the incomplete range
    pub fn incomplete_range(&self) -> Range<u64> {
        self.downloaded.end..self.allocated.end
    }

    /// Check if download is complete
    pub fn is_complete(&self) -> bool {
        self.downloaded.end >= self.allocated.end
    }
}

/// Enhanced chunk planner that maintains unified state
pub struct ChunkPlanner {
    /// Total size of the content
    pub total: u64,
    /// Minimal size of a chunk
    pub min_chunk_size: u64,
    /// Maximum number of concurrent chunks
    pub max_chunk_count: Option<NonZeroUsize>,
    /// All chunks indexed by their allocated range
    chunks: HashMap<GenericRange<u64>, ChunkState>,
    /// Runner ID to chunk mapping for quick lookup
    runner_to_chunk: HashMap<RunnerId, GenericRange<u64>>,
}

impl ChunkPlanner {
    pub fn new(total: u64) -> Self {
        Self {
            total,
            min_chunk_size: DEFAULT_MIN_CHUNK_SIZE,
            max_chunk_count: None,
            chunks: HashMap::new(),
            runner_to_chunk: HashMap::new(),
        }
    }

    /// Get current number of chunks
    pub fn get_chunks_count(&self) -> usize {
        self.chunks.len()
    }

    /// Get number of active runners
    pub fn get_active_runners_count(&self) -> usize {
        self.chunks
            .values()
            .filter(|c| c.status == ChunkStatus::Running)
            .count()
    }

    /// Allocate a new chunk
    pub fn allocate_chunk(&mut self, range: Range<u64>, runner_id: Option<RunnerId>) -> bool {
        if range.start >= range.end || range.end > self.total {
            return false;
        }

        // Check max chunk count
        if let Some(max) = self.max_chunk_count {
            if self.get_chunks_count() >= max.get() {
                return false;
            }
        }

        let generic_range = GenericRange::from(range.clone());

        // Check for overlaps
        for existing_range in self.chunks.keys() {
            if existing_range.intersect(generic_range) != OperationResult::Empty {
                return false;
            }
        }

        // Create new chunk state
        let chunk_state = ChunkState::new(range, runner_id);
        self.chunks.insert(generic_range, chunk_state);

        // Update runner mapping
        if let Some(id) = runner_id {
            self.runner_to_chunk.insert(id, generic_range);
        }

        true
    }

    /// Update download progress for a runner
    pub fn update_progress(
        &mut self,
        runner_id: RunnerId,
        bytes_downloaded: u64,
    ) -> Result<Range<u64>, String> {
        let range = self
            .runner_to_chunk
            .get(&runner_id)
            .ok_or_else(|| format!("Runner {runner_id} not found"))?;

        let chunk = self
            .chunks
            .get_mut(range)
            .ok_or_else(|| format!("Chunk for runner {runner_id} not found"))?;

        // Update downloaded range
        let new_end = (chunk.downloaded.end + bytes_downloaded).min(chunk.allocated.end);
        let previous_end = std::mem::replace(&mut chunk.downloaded.end, new_end);

        // Check if complete
        if chunk.is_complete() {
            chunk.status = ChunkStatus::Finished;
        }

        Ok(previous_end..new_end)
    }

    /// Mark a runner as finished
    pub fn mark_finished(&mut self, runner_id: RunnerId) -> Result<(), Error> {
        let range = self
            .runner_to_chunk
            .get(&runner_id)
            .ok_or(Error::RunnerNotFound(runner_id))?;

        if let Some(chunk) = self.chunks.get_mut(range) {
            chunk.status = ChunkStatus::Finished;
            Ok(())
        } else {
            Err(Error::ChunkNotFound(runner_id))
        }
    }

    /// Mark a runner as failed and release the chunk
    pub fn mark_failed(&mut self, runner_id: RunnerId) -> Result<Range<u64>, Error> {
        let range = self
            .runner_to_chunk
            .remove(&runner_id)
            .ok_or(Error::RunnerNotFound(runner_id))?;

        if let Some(mut chunk) = self.chunks.remove(&range) {
            // Return the unfinished portion
            let unfinished = chunk.downloaded.end..chunk.allocated.end;

            // If nothing was downloaded, remove the chunk entirely
            if chunk.downloaded.start == chunk.downloaded.end {
                let allocated = chunk.allocated.clone();
                // Chunk is already removed, just return the range
                Ok(allocated)
            } else {
                // Keep the downloaded portion as a finished chunk
                chunk.allocated.end = chunk.downloaded.end;
                chunk.status = ChunkStatus::Finished; // The downloaded portion should be marked as finished
                chunk.runner_id = None;
                // Re-insert the chunk with updated range
                let new_range = GenericRange::from(chunk.allocated.clone());
                self.chunks.insert(new_range, chunk);
                Ok(unfinished)
            }
        } else {
            Err(Error::ChunkNotFound(runner_id))
        }
    }

    /// Split a chunk at the given position
    pub fn split_chunk(
        &mut self,
        runner_id: RunnerId,
        split_pos: u64,
        new_runner_id: RunnerId,
    ) -> Result<Range<u64>, Error> {
        let range = self
            .runner_to_chunk
            .remove(&runner_id)
            .ok_or(Error::RunnerNotFound(runner_id))?;

        let chunk = self
            .chunks
            .remove(&range)
            .ok_or(Error::ChunkNotFound(runner_id))?;

        // Validate split position
        if split_pos <= chunk.downloaded.end || split_pos >= chunk.allocated.end {
            self.chunks.insert(range, chunk);
            self.runner_to_chunk.insert(runner_id, range);
            return Err(Error::InvalidSplitPosition(split_pos));
        }

        // Create two new chunks
        let first_range = chunk.allocated.start..split_pos;
        let second_range = split_pos..chunk.allocated.end;

        // First chunk keeps the downloaded progress
        let mut first_chunk = chunk;
        first_chunk.allocated = first_range.clone();

        // Second chunk starts fresh
        let second_chunk = ChunkState::new(second_range.clone(), Some(new_runner_id));

        // Insert both chunks
        let first_generic_range = GenericRange::from(first_range);
        let second_generic_range = GenericRange::from(second_range.clone());

        self.chunks.insert(first_generic_range, first_chunk);
        self.chunks.insert(second_generic_range, second_chunk);

        // Update runner mappings
        self.runner_to_chunk.insert(runner_id, first_generic_range);
        self.runner_to_chunk
            .insert(new_runner_id, second_generic_range);

        Ok(second_range)
    }

    /// Get available ranges (not allocated to any chunk)
    pub fn get_available_ranges(&self) -> Vec<Range<u64>> {
        let occupied = Ranges::from_iter(self.chunks.keys().cloned());
        let full_range = Ranges::from(GenericRange::from(0..self.total));
        let available = full_range - occupied;

        let mut ranges: Vec<_> = available
            .as_slice()
            .iter()
            .map(convert_generic_range_to_std_range)
            .collect();
        ranges.sort_by_key(|r| r.start);
        ranges
    }

    /// Get all downloaded ranges (for progress reporting)
    pub fn get_downloaded_ranges(&self) -> Vec<Range<u64>> {
        let mut ranges: Vec<_> = self
            .chunks
            .values()
            .filter(|c| c.downloaded.start < c.downloaded.end)
            .map(|c| c.downloaded.clone())
            .collect();
        ranges.sort_by_key(|r| r.start);

        // Merge adjacent ranges
        merge_adjacent_ranges(ranges)
    }

    /// Get total downloaded bytes
    pub fn get_total_downloaded(&self) -> u64 {
        self.chunks
            .values()
            .map(|c| c.downloaded.end - c.downloaded.start)
            .sum()
    }

    /// Get incomplete states from occupied ranges
    pub fn get_incomplete_states(
        &self,
        min_chunk_size: Option<u64>,
    ) -> Vec<(RunnerId, Range<u64>)> {
        self.chunks
            .values()
            .filter(|c| {
                c.status != ChunkStatus::Finished
                    && c.downloaded.end < c.allocated.end
                    && c.remaining() // For split check
                        >= min_chunk_size
                            .map(|v| v.max(2 * self.min_chunk_size))
                            .unwrap_or(2 * self.min_chunk_size)
            })
            .map(|c| {
                (
                    c.runner_id
                        .expect("runner id should be some if not finished"),
                    c.downloaded.end..c.allocated.end,
                )
            })
            .collect::<Vec<_>>()
    }

    /// Find a chunk to split or evict based on download progress
    pub fn find_chunk_to_split(
        &self,
        required_size: u64,
    ) -> Option<(Option<RunnerId>, Range<u64>)> {
        // First try to find a suitable available range
        for range in self.get_available_ranges() {
            if range.end - range.start >= required_size {
                return Some((None, range.start..range.start + required_size));
            }
        }

        // Find the chunk with the most remaining work
        let best_candidate = self
            .chunks
            .values()
            .filter(|c| {
                c.status == ChunkStatus::Running
                    && c.runner_id.is_some()
                    && c.remaining() >= required_size.max(self.min_chunk_size)
            })
            .max_by_key(|c| c.remaining());

        if let Some(chunk) = best_candidate {
            let split_pos = chunk.downloaded.end + required_size;
            debug_assert!(chunk.runner_id.is_some(), "chunk should have a runner id");
            return Some((chunk.runner_id, chunk.downloaded.end..split_pos));
        }

        None
    }

    /// Get state of a specific runner
    pub fn get_runner_state(&self, runner_id: RunnerId) -> Option<&ChunkState> {
        self.runner_to_chunk
            .get(&runner_id)
            .and_then(|range| self.chunks.get(range))
    }

    /// Get mut state ref of a specific runner
    pub fn get_runner_state_mut(&mut self, runner_id: RunnerId) -> Option<&mut ChunkState> {
        self.runner_to_chunk
            .get(&runner_id)
            .and_then(|range| self.chunks.get_mut(range))
    }

    /// Resize the runner state by the given size
    pub fn resize_runner_state(&mut self, runner_id: RunnerId, new_size: u64) -> Result<(), Error> {
        let chunk = self
            .runner_to_chunk
            .remove(&runner_id)
            .ok_or(Error::RunnerNotFound(runner_id))?;
        let mut state = self
            .chunks
            .remove(&chunk)
            .ok_or(Error::ChunkNotFound(runner_id))?;
        state.allocated.end = state.downloaded.end + new_size;
        let new_chunk = GenericRange::from(state.allocated.clone());
        self.chunks.insert(new_chunk, state);
        self.runner_to_chunk.insert(runner_id, new_chunk);
        Ok(())
    }

    /// Check if all chunks are finished
    pub fn is_complete(&self) -> bool {
        !self.chunks.is_empty()
            && self
                .chunks
                .values()
                .all(|c| c.status == ChunkStatus::Finished)
            && self.get_available_ranges().is_empty()
    }

    /// Iterator over all chunks with their states
    pub fn iter_chunks(&self) -> impl Iterator<Item = &ChunkState> {
        self.chunks.values()
    }

    /// Try to arrange a chunk by length
    pub fn try_arrange_chunk_by_length(&self, length: u64) -> Option<Range<u64>> {
        self.get_available_ranges()
            .into_iter()
            .find(|r| r.end - r.start >= length)
            .map(|r| r.start..r.start + length)
    }
}

/// Convert a generic range to a standard range
fn convert_generic_range_to_std_range(range: &GenericRange<u64>) -> Range<u64> {
    use std::ops::{Bound, RangeBounds};
    let start = match range.start_bound() {
        Bound::Included(v) => *v,
        _ => unreachable!(),
    };
    let end = match range.end_bound() {
        Bound::Excluded(v) => *v,
        _ => unreachable!(),
    };
    start..end
}

/// Merge adjacent ranges
fn merge_adjacent_ranges(mut ranges: Vec<Range<u64>>) -> Vec<Range<u64>> {
    if ranges.is_empty() {
        return ranges;
    }

    ranges.sort_by_key(|r| r.start);
    let mut merged = vec![ranges[0].clone()];

    for range in ranges.into_iter().skip(1) {
        let last = merged.last_mut().unwrap();
        if last.end >= range.start {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }

    merged
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use super::*;

    // === Basic functionality tests ===

    #[test]
    fn test_chunk_planner_creation() {
        let planner = ChunkPlanner::new(1000);
        assert_eq!(planner.total, 1000);
        assert_eq!(planner.min_chunk_size, DEFAULT_MIN_CHUNK_SIZE);
        assert_eq!(planner.max_chunk_count, None);
        assert_eq!(planner.get_chunks_count(), 0);
        assert_eq!(planner.get_active_runners_count(), 0);
        assert_eq!(planner.get_total_downloaded(), 0);
        assert!(!planner.is_complete());
    }

    #[test]
    fn test_chunk_state_creation() {
        let state = ChunkState::new(100..500, Some(1));
        assert_eq!(state.allocated, 100..500);
        assert_eq!(state.downloaded, 100..100);
        assert_eq!(state.runner_id, Some(1));
        assert_eq!(state.status, ChunkStatus::Running);
        assert_eq!(state.remaining(), 400);
        assert_eq!(state.incomplete_range(), 100..500);
        assert!(!state.is_complete());

        let state_idle = ChunkState::new(0..100, None);
        assert_eq!(state_idle.status, ChunkStatus::Idle);
        assert_eq!(state_idle.runner_id, None);
    }

    #[test]
    fn test_basic_allocation() {
        let mut planner = ChunkPlanner::new(1000);

        // Normal allocation
        assert!(planner.allocate_chunk(0..300, Some(1)));
        assert_eq!(planner.get_chunks_count(), 1);
        assert_eq!(planner.get_active_runners_count(), 1);

        // Allocate to idle state
        assert!(planner.allocate_chunk(300..600, None));
        assert_eq!(planner.get_chunks_count(), 2);
        assert_eq!(planner.get_active_runners_count(), 1);

        // Check state
        let state = planner.get_runner_state(1).unwrap();
        assert_eq!(state.allocated, 0..300);
        assert_eq!(state.status, ChunkStatus::Running);
    }

    #[test]
    fn test_unified_progress_tracking() {
        let mut planner = ChunkPlanner::new(1000);

        // Allocate chunk with runner
        assert!(planner.allocate_chunk(0..500, Some(1)));

        // Update progress
        assert!(planner.update_progress(1, 100).is_ok());
        assert!(planner.update_progress(1, 150).is_ok());

        // Check state
        let state = planner.get_runner_state(1).unwrap();
        assert_eq!(state.downloaded, 0..250);
        assert_eq!(state.remaining(), 250);

        // Check total downloaded
        assert_eq!(planner.get_total_downloaded(), 250);
    }

    #[test]
    fn test_chunk_splitting() {
        let mut planner = ChunkPlanner::new(1000);

        // Allocate and partially download
        assert!(planner.allocate_chunk(0..600, Some(1)));
        assert!(planner.update_progress(1, 200).is_ok());

        // Split the chunk
        let new_range = planner.split_chunk(1, 400, 2).unwrap();
        assert_eq!(new_range, 400..600);

        // Verify both chunks exist
        assert_eq!(planner.get_chunks_count(), 2);

        let chunk1 = planner.get_runner_state(1).unwrap();
        assert_eq!(chunk1.allocated, 0..400);
        assert_eq!(chunk1.downloaded, 0..200);

        let chunk2 = planner.get_runner_state(2).unwrap();
        assert_eq!(chunk2.allocated, 400..600);
        assert_eq!(chunk2.downloaded, 400..400);
    }

    #[test]
    fn test_failure_handling() {
        let mut planner = ChunkPlanner::new(1000);

        // Allocate and partially download
        assert!(planner.allocate_chunk(100..500, Some(1)));
        assert!(planner.update_progress(1, 150).is_ok());

        // Mark as failed
        let unfinished = planner.mark_failed(1).unwrap();
        assert_eq!(unfinished, 250..500);

        // The downloaded portion should be kept
        let downloaded_ranges = planner.get_downloaded_ranges();
        assert_eq!(downloaded_ranges, vec![100..250]);

        // The unfinished portion should be available
        let available = planner.get_available_ranges();

        // Check if there's a range containing 250..500
        let contains_unfinished = available.iter().any(|r| r.start <= 250 && r.end >= 500);
        assert!(
            contains_unfinished,
            "Expected range 250..500 to be available in {available:?}"
        );
    }

    #[test]
    fn test_completion_detection() {
        let mut planner = ChunkPlanner::new(100);

        // Allocate all space
        assert!(planner.allocate_chunk(0..50, Some(1)));
        assert!(planner.allocate_chunk(50..100, Some(2)));

        assert!(!planner.is_complete());

        // Complete first chunk
        assert!(planner.update_progress(1, 50).is_ok());
        assert!(planner.mark_finished(1).is_ok());

        assert!(!planner.is_complete());

        // Complete second chunk
        assert!(planner.update_progress(2, 50).is_ok());
        assert!(planner.mark_finished(2).is_ok());

        assert!(planner.is_complete());
    }

    // === Edge case tests ===

    #[test]
    fn test_empty_file() {
        let planner = ChunkPlanner::new(0);
        assert_eq!(planner.total, 0);
        assert!(planner.get_available_ranges().is_empty());
        assert!(!planner.is_complete()); // No blocks in empty file, not considered complete
    }

    #[test]
    fn test_single_byte_file() {
        let mut planner = ChunkPlanner::new(1);
        assert!(planner.allocate_chunk(0..1, Some(1)));
        assert!(planner.update_progress(1, 1).is_ok());
        assert!(planner.mark_finished(1).is_ok());
        assert!(planner.is_complete());
    }

    #[test]
    #[allow(clippy::reversed_empty_ranges)]
    fn test_invalid_range_allocation() {
        let mut planner = ChunkPlanner::new(1000);

        // Start position greater than or equal to end position
        assert!(!planner.allocate_chunk(500..500, Some(1)));
        assert!(!planner.allocate_chunk(500..400, Some(2)));

        // Exceeds total size
        assert!(!planner.allocate_chunk(900..1100, Some(3)));
        assert!(!planner.allocate_chunk(1000..1001, Some(4)));

        // Ensure no blocks were allocated
        assert_eq!(planner.get_chunks_count(), 0);
    }

    #[test]
    fn test_overlapping_chunks() {
        let mut planner = ChunkPlanner::new(1000);

        // Allocate first chunk
        assert!(planner.allocate_chunk(100..300, Some(1)));

        // Try to allocate overlapping chunks
        assert!(!planner.allocate_chunk(200..400, Some(2))); // Overlapping
        assert!(!planner.allocate_chunk(50..150, Some(3))); // Overlapping
        assert!(!planner.allocate_chunk(100..300, Some(4))); // Completely overlapping
        assert!(!planner.allocate_chunk(150..250, Some(5))); // Contained within

        // Allocating non-overlapping chunks should succeed
        assert!(planner.allocate_chunk(0..100, Some(6)));
        assert!(planner.allocate_chunk(300..400, Some(7)));

        assert_eq!(planner.get_chunks_count(), 3);
    }

    #[test]
    fn test_max_chunk_count_limit() {
        let mut planner = ChunkPlanner::new(1000);
        planner.max_chunk_count = Some(NonZeroUsize::new(2).unwrap());

        // Allocating two chunks should succeed
        assert!(planner.allocate_chunk(0..100, Some(1)));
        assert!(planner.allocate_chunk(100..200, Some(2)));

        // Third chunk should fail
        assert!(!planner.allocate_chunk(200..300, Some(3)));
        assert_eq!(planner.get_chunks_count(), 2);
    }

    #[test]
    fn test_zero_length_ranges() {
        let mut planner = ChunkPlanner::new(1000);

        // Allocate normal chunk
        assert!(planner.allocate_chunk(100..300, Some(1)));

        // Trying to update 0 bytes should succeed but not change state
        assert!(planner.update_progress(1, 0).is_ok());
        let state = planner.get_runner_state(1).unwrap();
        assert_eq!(state.downloaded, 100..100);

        // Splitting at download position should fail
        assert!(planner.split_chunk(1, 100, 2).is_err());
    }

    // === Error handling tests ===

    #[test]
    fn test_update_progress_nonexistent_runner() {
        let mut planner = ChunkPlanner::new(1000);

        // Try to update non-existent runner
        assert!(planner.update_progress(99, 100).is_err());

        // Allocate a chunk then try to update another runner
        assert!(planner.allocate_chunk(0..100, Some(1)));
        assert!(planner.update_progress(2, 50).is_err());
    }

    #[test]
    fn test_mark_finished_nonexistent_runner() {
        let mut planner = ChunkPlanner::new(1000);

        // Try to mark non-existent runner as finished
        assert!(matches!(
            planner.mark_finished(99),
            Err(Error::RunnerNotFound(99))
        ));

        // Allocate a chunk then try to mark another runner
        assert!(planner.allocate_chunk(0..100, Some(1)));
        assert!(matches!(
            planner.mark_finished(2),
            Err(Error::RunnerNotFound(2))
        ));
    }

    #[test]
    fn test_mark_failed_nonexistent_runner() {
        let mut planner = ChunkPlanner::new(1000);

        // Try to mark non-existent runner as failed
        assert!(matches!(
            planner.mark_failed(99),
            Err(Error::RunnerNotFound(99))
        ));
    }

    #[test]
    fn test_split_chunk_errors() {
        let mut planner = ChunkPlanner::new(1000);

        // Try to split non-existent chunk
        assert!(matches!(
            planner.split_chunk(99, 500, 2),
            Err(Error::RunnerNotFound(99))
        ));

        // Allocate chunk and partially download
        assert!(planner.allocate_chunk(100..500, Some(1)));
        assert!(planner.update_progress(1, 100).is_ok());

        // Invalid split positions
        assert!(matches!(
            planner.split_chunk(1, 200, 2),
            Err(Error::InvalidSplitPosition(200))
        )); // Within downloaded range
        assert!(matches!(
            planner.split_chunk(1, 150, 2),
            Err(Error::InvalidSplitPosition(150))
        )); // Within downloaded range
        assert!(matches!(
            planner.split_chunk(1, 500, 2),
            Err(Error::InvalidSplitPosition(500))
        )); // On chunk boundary
        assert!(matches!(
            planner.split_chunk(1, 600, 2),
            Err(Error::InvalidSplitPosition(600))
        )); // Beyond chunk range
    }

    #[test]
    fn test_resize_runner_state_errors() {
        let mut planner = ChunkPlanner::new(1000);

        // Try to resize non-existent runner
        assert!(matches!(
            planner.resize_runner_state(99, 100),
            Err(Error::RunnerNotFound(99))
        ));

        // Allocate chunk and test resizing
        assert!(planner.allocate_chunk(100..500, Some(1)));
        assert!(planner.update_progress(1, 100).is_ok());

        // Resize
        assert!(planner.resize_runner_state(1, 200).is_ok());
        let state = planner.get_runner_state(1).unwrap();
        assert_eq!(state.allocated, 100..400); // 200 + 200 = 400
    }

    // === Complex scenario tests ===

    #[test]
    fn test_multiple_chunks_concurrent_operations() {
        let mut planner = ChunkPlanner::new(1000);

        // Allocate multiple chunks
        assert!(planner.allocate_chunk(0..200, Some(1)));
        assert!(planner.allocate_chunk(200..400, Some(2)));
        assert!(planner.allocate_chunk(400..600, Some(3)));

        // Update progress simultaneously
        assert!(planner.update_progress(1, 100).is_ok());
        assert!(planner.update_progress(2, 150).is_ok());
        assert!(planner.update_progress(3, 50).is_ok());

        // Check state
        assert_eq!(planner.get_total_downloaded(), 300);
        assert_eq!(planner.get_active_runners_count(), 3);

        // Mark one as finished
        assert!(planner.update_progress(1, 100).is_ok());
        assert!(planner.mark_finished(1).is_ok());

        // Mark one as failed
        let unfinished = planner.mark_failed(2).unwrap();
        assert_eq!(unfinished, 350..400);

        // Check final state
        // Failed chunk is partially retained: chunk1(finished), chunk2(failed but downloaded portion retained), chunk3(still running)
        assert_eq!(planner.get_chunks_count(), 3);
        assert_eq!(planner.get_active_runners_count(), 1); // Only runner 3 is still running

        // Check failed chunk state
        let available = planner.get_available_ranges();
        assert!(available.contains(&(350..400))); // Unfinished portion should be available
        assert!(available.contains(&(600..1000))); // Unallocated portion should be available
    }

    #[test]
    fn test_chunk_splitting_and_reallocation() {
        let mut planner = ChunkPlanner::new(1000);

        // Allocate a large chunk
        assert!(planner.allocate_chunk(0..800, Some(1)));
        assert!(planner.update_progress(1, 200).is_ok());

        // Split chunk
        let new_range = planner.split_chunk(1, 500, 2).unwrap();
        assert_eq!(new_range, 500..800);

        // Check state after splitting
        assert_eq!(planner.get_chunks_count(), 2);

        let chunk1 = planner.get_runner_state(1).unwrap();
        assert_eq!(chunk1.allocated, 0..500);
        assert_eq!(chunk1.downloaded, 0..200);

        let chunk2 = planner.get_runner_state(2).unwrap();
        assert_eq!(chunk2.allocated, 500..800);
        assert_eq!(chunk2.downloaded, 500..500);

        // Continue downloading
        assert!(planner.update_progress(1, 300).is_ok());
        assert!(planner.update_progress(2, 100).is_ok());

        // Complete download
        assert!(planner.mark_finished(1).is_ok());
        assert!(planner.update_progress(2, 200).is_ok());
        assert!(planner.mark_finished(2).is_ok());

        // Allocate remaining space
        assert!(planner.allocate_chunk(800..1000, Some(3)));
        assert!(planner.update_progress(3, 200).is_ok());
        assert!(planner.mark_finished(3).is_ok());

        // Check completion state
        assert!(planner.is_complete());
        assert_eq!(planner.get_total_downloaded(), 1000);
    }

    #[test]
    fn test_failure_recovery_workflow() {
        let mut planner = ChunkPlanner::new(1000);

        // Allocate multiple chunks
        assert!(planner.allocate_chunk(0..300, Some(1)));
        assert!(planner.allocate_chunk(300..600, Some(2)));
        assert!(planner.allocate_chunk(600..900, Some(3)));

        // Partial download
        assert!(planner.update_progress(1, 100).is_ok());
        assert!(planner.update_progress(2, 200).is_ok());
        assert!(planner.update_progress(3, 50).is_ok());

        // Runner 2 failed
        let unfinished = planner.mark_failed(2).unwrap();
        assert_eq!(unfinished, 500..600);

        // Check available ranges
        let available = planner.get_available_ranges();
        assert!(available.iter().any(|r| r.start <= 500 && r.end >= 600));
        assert!(available.contains(&(900..1000)));

        // Reallocate failed portion
        assert!(planner.allocate_chunk(500..600, Some(4)));
        assert!(planner.update_progress(4, 100).is_ok());
        assert!(planner.mark_finished(4).is_ok());

        // Complete remaining download
        assert!(planner.update_progress(1, 200).is_ok());
        assert!(planner.mark_finished(1).is_ok());
        assert!(planner.update_progress(3, 250).is_ok());
        assert!(planner.mark_finished(3).is_ok());

        // Allocate final space
        assert!(planner.allocate_chunk(900..1000, Some(5)));
        assert!(planner.update_progress(5, 100).is_ok());
        assert!(planner.mark_finished(5).is_ok());

        // Check completion state
        assert!(planner.is_complete());

        // Check that downloaded ranges are continuous
        let downloaded = planner.get_downloaded_ranges();
        assert_eq!(downloaded, vec![0..1000]);
    }

    #[test]
    fn test_find_chunk_to_split() {
        let mut planner = ChunkPlanner::new(10 * 1024 * 1024); // 10MB total

        // When no chunks exist, entire file is available, should return available range
        let result = planner.find_chunk_to_split(100);
        assert!(result.is_some());
        let (runner_id, range) = result.unwrap();
        assert_eq!(runner_id, None); // Should return available range instead of runner
        assert!(range.end - range.start >= 100);

        // Allocate some chunks, leaving gaps
        let chunk_size = 3 * 1024 * 1024; // 3MB
        assert!(planner.allocate_chunk(0..chunk_size, Some(1)));
        assert!(planner.allocate_chunk(2 * chunk_size..3 * chunk_size, Some(2)));

        // Should return available range when available ranges exist
        let result = planner.find_chunk_to_split(100);
        assert!(result.is_some());
        let (runner_id, range) = result.unwrap();
        assert_eq!(runner_id, None);
        // Check if range is reasonable, exact match not required
        assert!(range.end - range.start >= 100);

        // Allocate all available space
        assert!(planner.allocate_chunk(chunk_size..2 * chunk_size, Some(3)));
        assert!(planner.allocate_chunk(3 * chunk_size..10 * 1024 * 1024, Some(4)));

        // Add small download progress, retain large space for splitting
        assert!(planner.update_progress(1, 500 * 1024).is_ok()); // 500KB
        assert!(planner.update_progress(2, 600 * 1024).is_ok()); // 600KB
        assert!(planner.update_progress(4, 700 * 1024).is_ok()); // 700KB

        // Should now find chunk with most remaining work
        let result = planner.find_chunk_to_split(DEFAULT_MIN_CHUNK_SIZE);
        assert!(result.is_some());
        let (runner_id, _) = result.unwrap();
        assert!(runner_id.is_some());

        // Test edge case: required chunk size close to minimum but insufficient remaining space
        // Complete all chunks first, so no splittable chunks remain
        assert!(
            planner
                .update_progress(1, 3 * 1024 * 1024 - 500 * 1024)
                .is_ok()
        );
        assert!(
            planner
                .update_progress(2, 3 * 1024 * 1024 - 600 * 1024)
                .is_ok()
        );
        assert!(planner.update_progress(3, 3 * 1024 * 1024).is_ok());
        assert!(
            planner
                .update_progress(4, 3 * 1024 * 1024 + 1024 * 1024 - 700 * 1024)
                .is_ok()
        );

        // Should now have no chunks large enough to split
        let result = planner.find_chunk_to_split(DEFAULT_MIN_CHUNK_SIZE);
        assert!(result.is_none());
    }

    #[test]
    fn test_get_incomplete_states() {
        let mut planner = ChunkPlanner::new(10 * 1024 * 1024); // 10MB total size
        let large_chunk_size = 3 * 1024 * 1024; // 3MB per chunk

        // Allocate some large chunks to meet splitting conditions
        assert!(planner.allocate_chunk(0..large_chunk_size, Some(1)));
        assert!(planner.allocate_chunk(large_chunk_size..2 * large_chunk_size, Some(2)));
        assert!(planner.allocate_chunk(2 * large_chunk_size..3 * large_chunk_size, Some(3)));

        // Add some download progress, but retain sufficient remaining space for splitting
        assert!(planner.update_progress(1, 500 * 1024).is_ok()); // 500KB downloaded
        assert!(planner.update_progress(2, 1024 * 1024).is_ok()); // 1MB downloaded

        // Mark one as finished
        assert!(planner.update_progress(3, large_chunk_size).is_ok());
        assert!(planner.mark_finished(3).is_ok());

        // Get incomplete states
        let incomplete = planner.get_incomplete_states(None);
        assert_eq!(incomplete.len(), 2);

        // Check results
        let incomplete_ids: Vec<_> = incomplete.iter().map(|(id, _)| *id).collect();
        assert!(incomplete_ids.contains(&1));
        assert!(incomplete_ids.contains(&2));
        assert!(!incomplete_ids.contains(&3));
    }

    #[test]
    fn test_try_arrange_chunk_by_length() {
        let mut planner = ChunkPlanner::new(1000);

        // Allocate some chunks, leaving gaps
        assert!(planner.allocate_chunk(0..200, Some(1)));
        assert!(planner.allocate_chunk(400..600, Some(2)));

        // Try to arrange blocks of different lengths
        assert_eq!(planner.try_arrange_chunk_by_length(150), Some(200..350));
        assert_eq!(planner.try_arrange_chunk_by_length(200), Some(200..400));

        // Check lengths that cannot be arranged within current available ranges
        assert_eq!(planner.try_arrange_chunk_by_length(250), Some(600..850));
        assert_eq!(planner.try_arrange_chunk_by_length(400), Some(600..1000));

        // Lengths that cannot be arranged (exceed maximum available range)
        assert_eq!(planner.try_arrange_chunk_by_length(500), None); // Maximum available range is 400 bytes
    }

    // === Helper function tests ===

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn test_merge_adjacent_ranges() {
        // Empty vector
        assert_eq!(merge_adjacent_ranges(vec![]), vec![]);

        // Single range
        assert_eq!(merge_adjacent_ranges(vec![0..100]), vec![0..100]);

        // Adjacent ranges
        assert_eq!(
            merge_adjacent_ranges(vec![0..100, 100..200, 200..300]),
            vec![0..300]
        );

        // Overlapping ranges
        assert_eq!(
            merge_adjacent_ranges(vec![0..150, 100..200, 180..300]),
            vec![0..300]
        );

        // Separate ranges
        assert_eq!(
            merge_adjacent_ranges(vec![0..100, 200..300, 400..500]),
            vec![0..100, 200..300, 400..500]
        );

        // Unordered input
        assert_eq!(
            merge_adjacent_ranges(vec![200..300, 0..100, 100..200]),
            vec![0..300]
        );
    }

    #[test]
    fn test_convert_generic_range_to_std_range() {
        let generic_range = GenericRange::from(100..500);
        let std_range = convert_generic_range_to_std_range(&generic_range);
        assert_eq!(std_range, 100..500);
    }

    // === Integration tests ===

    #[test]
    fn test_complete_download_workflow() {
        let mut planner = ChunkPlanner::new(1000);

        // Simulate complete download workflow

        // 1. Initial allocation
        assert!(planner.allocate_chunk(0..500, Some(1)));
        assert!(planner.allocate_chunk(500..1000, Some(2)));

        // 2. Partial download
        assert!(planner.update_progress(1, 200).is_ok());
        assert!(planner.update_progress(2, 100).is_ok());

        // 3. Runner 1 failed, reallocate
        let unfinished = planner.mark_failed(1).unwrap();
        assert_eq!(unfinished, 200..500);

        // Check available ranges
        let available = planner.get_available_ranges();
        assert!(available.contains(&(200..500)));

        // Reallocate failed portion
        assert!(planner.allocate_chunk(200..500, Some(3)));

        // 4. Continue downloading
        assert!(planner.update_progress(2, 400).is_ok());
        assert!(planner.update_progress(3, 300).is_ok());

        // 5. Complete download
        assert!(planner.mark_finished(2).is_ok());
        assert!(planner.mark_finished(3).is_ok());

        // 6. Verify completion state
        assert!(planner.is_complete());
        assert_eq!(planner.get_total_downloaded(), 1000);

        let downloaded = planner.get_downloaded_ranges();
        assert_eq!(downloaded, vec![0..1000]);
    }

    #[test]
    fn test_progress_overflow_protection() {
        let mut planner = ChunkPlanner::new(1000);

        // Allocate a small chunk
        assert!(planner.allocate_chunk(0..100, Some(1)));

        // Try to download data exceeding allocated size
        assert!(planner.update_progress(1, 150).is_ok());

        // Check that download amount is limited to allocated size
        let state = planner.get_runner_state(1).unwrap();
        assert_eq!(state.downloaded, 0..100);
        assert!(state.is_complete());
    }

    #[test]
    fn test_large_number_of_chunks() {
        let mut planner = ChunkPlanner::new(10000);

        // Allocate many small chunks
        let chunk_size = 100;
        let num_chunks = 100usize;

        for i in 0..num_chunks {
            let start = (i * chunk_size) as u64;
            let end = ((i + 1) * chunk_size) as u64;
            assert!(planner.allocate_chunk(start..end, Some(i + 1)));
        }

        assert_eq!(planner.get_chunks_count(), num_chunks);
        assert_eq!(planner.get_active_runners_count(), num_chunks);

        // Randomly update some progress
        for i in (0..num_chunks).step_by(10) {
            assert!(planner.update_progress(i + 1, 50).is_ok());
        }

        // Mark some as finished
        for i in (0..num_chunks).step_by(20) {
            assert!(planner.update_progress(i + 1, 50).is_ok());
            assert!(planner.mark_finished(i + 1).is_ok());
        }

        // Check statistics
        assert!(planner.get_total_downloaded() > 0);
        assert!(planner.get_active_runners_count() < num_chunks);
    }
}
