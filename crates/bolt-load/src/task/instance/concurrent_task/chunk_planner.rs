#![expect(dead_code, reason = "not every planner query is used by the task yet")]

use std::{
    collections::{BTreeMap, HashMap},
    num::NonZeroUsize,
    ops::Range,
    time::Instant,
};

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
    /// Failed attempts on the range before this chunk's, carried over from
    /// the [`Cooldown`]s it was claimed from (design §8.1). They only set
    /// the backoff.
    pub attempts: u32,
    /// The failures among `attempts` charged to the range's failure budget,
    /// carried over the same way (design §14.13): the retryable failures
    /// that were not restarts. A restart, a timeout or a dropped stream
    /// after the attempt delivered bytes, is never charged, and neither is
    /// a congestion rejection (429/503).
    pub charged: u32,
    /// Which attempt holds the chunk: a fresh number for every chunk the
    /// planner hands out, so a runner ID that claims the same start again
    /// is a new attempt. A chunk keeps its own when it shrinks; the chunk
    /// split off it gets a new one.
    pub generation: u64,
    /// The endgame hedge of this attempt, from the fork until the attempt
    /// ends (design §9.6); `None` while the attempt was never hedged.
    pub hedge: Option<Hedge>,
}

/// Which side of a hedged attempt a runner is (design §9.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The runner that held the chunk when it was hedged.
    Original,
    /// The runner that requested the rest of the chunk again.
    Hedge,
}

/// How the side of a pair that failed first failed (design §9.6): what
/// the budget of the pair needs of it once the other side fails too.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SideFailure {
    /// A restart: a timeout or a dropped stream after its own bytes were
    /// accepted.
    pub restart: bool,
    /// Charged to the range's budget, see
    /// [`charges_range`](super::strategy::charges_range).
    pub charged: bool,
}

/// The hedge runner of an undecided pair, while it is alive and the
/// original runner still owns the chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Partner {
    pub runner_id: RunnerId,
    /// Whether it reported `Started`.
    pub running: bool,
}

/// The endgame hedge of one attempt on a chunk (design §9.6).
///
/// The arbitration ends at the first non-empty frame taken since the fork;
/// the budget of the pair lasts until the attempt ends, when the chunk is
/// complete or released.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hedge {
    /// The downloaded end when the hedge was forked: where its request
    /// starts.
    pub fork: u64,
    /// The side of the chunk's runner.
    pub owner_side: Side,
    /// The side that failed first, while the other ran on.
    pub first: Option<SideFailure>,
    /// No non-empty frame was taken since the fork: the pair is undecided,
    /// and its runners are neither judged nor split.
    pub undecided: bool,
    /// The hedge runner while both sides run; the chunk's runner is then
    /// the original one.
    pub partner: Option<Partner>,
}

impl Hedge {
    /// The bytes the owner's own attempt delivered, given the chunk's
    /// downloaded end: the original's from the chunk's start, the hedge's
    /// from the fork.
    fn owner_bytes(&self, downloaded: &Range<u64>) -> u64 {
        match self.owner_side {
            Side::Original => downloaded.end - downloaded.start,
            Side::Hedge => downloaded.end - self.fork,
        }
    }
}

/// The backoff of an available range that failed runners gave back
/// (design §8.1).
///
/// Available bytes outside every cooldown are fresh: no failed attempt,
/// and claimable at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cooldown {
    /// The end of the range, which starts at the cooldown's key.
    pub end: u64,
    /// Failed attempts on the range.
    pub attempts: u32,
    /// The failures among `attempts` charged to the range's failure budget,
    /// see [`ChunkState::charged`].
    pub charged: u32,
    /// The range may not be claimed before this instant.
    pub not_before: Instant,
}

impl Cooldown {
    /// One cooldown for two that overlap or touch: the larger counts and
    /// the later `not_before`, so no byte is claimed earlier than either
    /// allowed.
    fn merge(self, other: Self) -> Self {
        Self {
            end: self.end.max(other.end),
            attempts: self.attempts.max(other.attempts),
            charged: self.charged.max(other.charged),
            not_before: self.not_before.max(other.not_before),
        }
    }
}

/// The unfinished range of a failed runner, given back to the available
/// pool by [`ChunkPlanner::mark_failed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Released {
    pub range: Range<u64>,
    /// Failed attempts on `range`, the released one included.
    pub attempts: u32,
    /// Failures on `range` charged to its budget before the released
    /// attempt. Whether that one is charged depends on how it failed
    /// (design §8.2), which the planner does not know.
    pub charged: u32,
    /// Bytes the planner accepted from the failed attempt, after clipping
    /// to its chunk; they stay downloaded.
    pub received: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChunkStatus {
    /// Chunk is allocated but no runner assigned
    Idle,
    /// Runner is actively downloading
    Running,
    /// Chunk is pending for creation of a runner, connecting to the server, etc.
    Pending,
    /// Runner finished downloading this chunk
    Finished,
    /// Runner failed, chunk may be reassigned
    Failed,
}

impl ChunkStatus {
    #[inline]
    pub const fn is_active(&self) -> bool {
        matches!(self, ChunkStatus::Running | ChunkStatus::Pending)
    }
}

#[derive(Debug, snafu::Snafu)]
pub enum Error {
    #[snafu(display("Runner {runner_id} not found"))]
    RunnerNotFound { runner_id: RunnerId },
    #[snafu(display("Chunk for runner {runner_id} not found"))]
    ChunkNotFound { runner_id: RunnerId },
    #[snafu(display("Invalid split position: {position}"))]
    InvalidSplitPosition { position: u64 },
    #[snafu(display("Invalid resize of runner {runner_id} to {new_size} remaining bytes"))]
    InvalidResize { runner_id: RunnerId, new_size: u64 },
    #[snafu(display("The chunk of runner {runner_id} cannot be hedged"))]
    InvalidHedge { runner_id: RunnerId },
    #[snafu(display("Runner {runner_id} is no hedge of an undecided pair"))]
    PartnerNotFound { runner_id: RunnerId },
}

/// The outcome of [`ChunkPlanner::resize_runner_state`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resize {
    /// The chunk already ends there; nothing changed.
    Unchanged,
    /// The chunk shrank; the runner may download at most `limit` bytes in total.
    Shrunk { limit: u64 },
}

impl ChunkState {
    fn from_runner_id(allocated: Range<u64>, runner_id: Option<RunnerId>) -> Self {
        match runner_id {
            Some(runner_id) => Self::new_running(allocated, runner_id),
            None => Self::new_idle(allocated),
        }
    }

    pub fn new(allocated: Range<u64>, runner_id: Option<RunnerId>, status: ChunkStatus) -> Self {
        Self {
            allocated: allocated.clone(),
            downloaded: allocated.start..allocated.start,
            runner_id,
            status,
            attempts: 0,
            charged: 0,
            generation: 0,
            hedge: None,
        }
    }

    /// Create a new chunk state with the idle status
    fn new_idle(allocated: Range<u64>) -> Self {
        Self::new(allocated, None, ChunkStatus::Idle)
    }

    /// Create a new chunk state with the running status
    fn new_running(allocated: Range<u64>, runner_id: RunnerId) -> Self {
        Self::new(allocated, Some(runner_id), ChunkStatus::Running)
    }

    /// Create a new chunk state with the pending status
    fn new_pending(allocated: Range<u64>, runner_id: RunnerId) -> Self {
        Self::new(allocated, Some(runner_id), ChunkStatus::Pending)
    }

    /// Update the status of the chunk
    fn update_status(&mut self, new_status: ChunkStatus) -> ChunkStatus {
        std::mem::replace(&mut self.status, new_status)
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

    /// Whether the chunk is hedged and undecided (design §9.6): neither
    /// side may be judged slow, and the chunk may not be split.
    pub fn is_undecided(&self) -> bool {
        self.hedge.as_ref().is_some_and(|hedge| hedge.undecided)
    }

    /// The hedge runner of the undecided pair, while both sides run.
    pub fn partner(&self) -> Option<Partner> {
        self.hedge.as_ref().and_then(|hedge| hedge.partner)
    }
}

/// Enhanced chunk planner that maintains unified state
#[derive(Clone)]
pub struct ChunkPlanner {
    /// Total size of the content
    pub total: u64,
    /// Minimal size of a chunk
    pub min_chunk_size: u64,
    /// Maximum number of concurrent chunks
    pub max_chunk_count: Option<NonZeroUsize>,
    /// All chunks keyed by `allocated.start`; their allocated ranges never overlap
    chunks: BTreeMap<u64, ChunkState>,
    /// Runner ID to chunk start mapping for quick lookup; only the runner
    /// that owns each chunk
    runner_to_chunk: HashMap<RunnerId, u64>,
    /// The hedge runner of an undecided pair to its chunk's start, while
    /// it is alive and not the owner (design §9.6). An entry is removed
    /// before its runner's ID can be reclaimed.
    hedge_of: HashMap<RunnerId, u64>,
    /// Bytes accepted into all chunks; no operation ever drops accepted bytes.
    accepted_bytes: u64,
    /// The accepted bytes written to the file, as the writer's completions
    /// reported them (design §11.1, §14.21).
    written: WrittenSet,
    /// Available ranges backing off after a failure, keyed by start. They
    /// never overlap a chunk or each other, and keep their attempts after
    /// `not_before` until claimed.
    cooldowns: BTreeMap<u64, Cooldown>,
    /// The [`ChunkState::generation`] of the next chunk.
    next_generation: u64,
}

impl ChunkPlanner {
    pub fn new(total: u64) -> Self {
        Self {
            total,
            min_chunk_size: DEFAULT_MIN_CHUNK_SIZE,
            max_chunk_count: None,
            chunks: BTreeMap::new(),
            runner_to_chunk: HashMap::new(),
            hedge_of: HashMap::new(),
            accepted_bytes: 0,
            written: WrittenSet::default(),
            cooldowns: BTreeMap::new(),
            next_generation: 0,
        }
    }

    /// A generation no chunk has had.
    fn take_generation(&mut self) -> u64 {
        let generation = self.next_generation;
        self.next_generation += 1;
        generation
    }

    /// Get current number of chunks
    pub fn get_chunks_count(&self) -> usize {
        self.chunks.len()
    }

    /// Get number of active runners: the runner of every connecting or
    /// running chunk, and the hedge runner of every undecided pair whose
    /// sides both run (design §9.6).
    pub fn get_active_runners_count(&self) -> usize {
        self.chunks
            .values()
            .filter(|c| c.status.is_active())
            .count()
            + self.hedge_of.len()
    }

    /// Allocate a pending chunk or idle chunk based on the runner id
    #[inline]
    pub fn allocate_chunk(&mut self, range: Range<u64>, runner_id: Option<RunnerId>) -> bool {
        match runner_id {
            Some(runner_id) => self.allocate_pending_chunk(range, runner_id),
            None => self.allocate_idle_chunk(range),
        }
    }

    #[inline]
    /// Allocate a pending chunk
    pub fn allocate_pending_chunk(&mut self, range: Range<u64>, runner_id: RunnerId) -> bool {
        self.allocate_chunk_inner(range, Some(runner_id), ChunkStatus::Pending)
    }

    #[inline]
    /// Allocate an idle chunk
    pub fn allocate_idle_chunk(&mut self, range: Range<u64>) -> bool {
        self.allocate_chunk_inner(range, None, ChunkStatus::Idle)
    }

    /// Allocate a new chunk inner
    #[inline]
    fn allocate_chunk_inner(
        &mut self,
        range: Range<u64>,
        runner_id: Option<RunnerId>,
        status: ChunkStatus,
    ) -> bool {
        if range.start >= range.end || range.end > self.total {
            return false;
        }

        // Check max chunk count
        if let Some(max) = self.max_chunk_count
            && self.get_chunks_count() >= max.get()
        {
            return false;
        }

        // Chunks never overlap, so only the last chunk starting before the
        // end can overlap the range.
        if let Some((_, pred)) = self.chunks.range(..range.end).next_back()
            && pred.allocated.end > range.start
        {
            return false;
        }

        // Create new chunk state, counting on from the failed attempts on it
        let start = range.start;
        let (attempts, charged) = self.take_cooldowns(&range);
        let chunk_state = ChunkState {
            attempts,
            charged,
            generation: self.take_generation(),
            ..ChunkState::new(range, runner_id, status)
        };
        self.chunks.insert(start, chunk_state);

        // Update runner mapping
        if let Some(id) = runner_id {
            self.runner_to_chunk.insert(id, start);
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

        // Update downloaded range, adding at most the remaining length
        let new_end = chunk.downloaded.end + bytes_downloaded.min(chunk.remaining());
        let previous_end = std::mem::replace(&mut chunk.downloaded.end, new_end);
        self.accepted_bytes += new_end - previous_end;

        // Check if complete; the attempt ends, and its hedge with it.
        if chunk.is_complete() {
            chunk.status = ChunkStatus::Finished;
            chunk.hedge = None;
        }

        Ok(previous_end..new_end)
    }

    /// Mark a runner as running
    pub fn mark_running(&mut self, runner_id: RunnerId) -> Result<(), Error> {
        let range = self
            .runner_to_chunk
            .get(&runner_id)
            .ok_or(Error::RunnerNotFound { runner_id })?;

        if let Some(chunk) = self.chunks.get_mut(range) {
            debug_assert!(
                chunk.status == ChunkStatus::Pending,
                "chunk should be pending"
            );
            chunk.status = ChunkStatus::Running;
            Ok(())
        } else {
            Err(Error::ChunkNotFound { runner_id })
        }
    }

    /// Mark a runner as finished
    pub fn mark_finished(&mut self, runner_id: RunnerId) -> Result<(), Error> {
        let range = self
            .runner_to_chunk
            .remove(&runner_id)
            .ok_or(Error::RunnerNotFound { runner_id })?;

        if let Some(chunk) = self.chunks.get_mut(&range) {
            chunk.status = ChunkStatus::Finished;
            chunk.hedge = None;
            Ok(())
        } else {
            Err(Error::ChunkNotFound { runner_id })
        }
    }

    /// Mark a runner as failed and release the unfinished part of its chunk.
    ///
    /// The released range is fresh until [`Self::cool_down`] backs it off.
    pub fn mark_failed(&mut self, runner_id: RunnerId) -> Result<Released, Error> {
        let range = self
            .runner_to_chunk
            .remove(&runner_id)
            .ok_or(Error::RunnerNotFound { runner_id })?;

        if let Some(mut chunk) = self.chunks.remove(&range) {
            debug_assert!(
                chunk.partner().is_none(),
                "the last side of a pair fails, not the first"
            );
            // The attempt ends here, and its hedge with it.
            chunk.hedge = None;
            let attempts = chunk.attempts.saturating_add(1);
            let charged = chunk.charged;
            // A chunk is claimed by one attempt and starts empty.
            let received = chunk.downloaded.end - chunk.downloaded.start;
            // Return the unfinished portion
            let unfinished = chunk.downloaded.end..chunk.allocated.end;

            // If nothing was downloaded, remove the chunk entirely
            if received == 0 {
                let allocated = chunk.allocated.clone();
                // Chunk is already removed, just return the range
                Ok(Released {
                    range: allocated,
                    attempts,
                    charged,
                    received,
                })
            } else {
                // Keep the downloaded portion as a finished chunk
                chunk.allocated.end = chunk.downloaded.end;
                chunk.status = ChunkStatus::Finished; // The downloaded portion should be marked as finished
                chunk.runner_id = None;
                // Re-insert the chunk; its start is unchanged
                self.chunks.insert(range, chunk);
                Ok(Released {
                    range: unfinished,
                    attempts,
                    charged,
                    received,
                })
            }
        } else {
            Err(Error::ChunkNotFound { runner_id })
        }
    }

    /// Back the available `range` off: it may be claimed again from
    /// `not_before`, and has failed `attempts` times, `charged` of them
    /// charged to its budget.
    ///
    /// It merges with the cooldowns it overlaps or touches into one, which
    /// takes the larger counts and the later `not_before`. Fresh bytes next
    /// to it stay fresh.
    pub fn cool_down(
        &mut self,
        range: Range<u64>,
        attempts: u32,
        charged: u32,
        not_before: Instant,
    ) {
        if range.is_empty() {
            return;
        }
        debug_assert!(
            self.get_available_ranges()
                .iter()
                .any(|available| available.start <= range.start && range.end <= available.end),
            "{range:?} is not available"
        );
        let mut start = range.start;
        let mut merged = Cooldown {
            end: range.end,
            attempts,
            charged,
            not_before,
        };
        if let Some((&before, &cooldown)) = self.cooldowns.range(..range.start).next_back()
            && cooldown.end >= range.start
        {
            self.cooldowns.remove(&before);
            start = before;
            merged = merged.merge(cooldown);
        }
        while let Some((&after, &cooldown)) = self.cooldowns.range(range.start..=merged.end).next()
        {
            self.cooldowns.remove(&after);
            merged = merged.merge(cooldown);
        }
        self.cooldowns.insert(start, merged);
    }

    /// The starts of the cooldowns overlapping `range`.
    fn overlapping_cooldowns(&self, range: &Range<u64>) -> impl Iterator<Item = u64> + '_ {
        let before = self
            .cooldowns
            .range(..range.start)
            .next_back()
            .filter(|(_, cooldown)| cooldown.end > range.start);
        before
            .into_iter()
            .chain(self.cooldowns.range(range.start..range.end))
            .map(|(&start, _)| start)
    }

    /// Take `range` out of the cooldowns, for a chunk claiming it; returns
    /// the most failed and the most charged attempts among the cooldowns it
    /// overlaps, 0 for fresh bytes. What is left of them outside `range`
    /// keeps its backoff.
    fn take_cooldowns(&mut self, range: &Range<u64>) -> (u32, u32) {
        let starts: Vec<u64> = self.overlapping_cooldowns(range).collect();
        let (mut attempts, mut charged) = (0, 0);
        for start in starts {
            let cooldown = self
                .cooldowns
                .remove(&start)
                .expect("the start was just found");
            attempts = attempts.max(cooldown.attempts);
            charged = charged.max(cooldown.charged);
            if start < range.start {
                self.cooldowns.insert(
                    start,
                    Cooldown {
                        end: range.start,
                        ..cooldown
                    },
                );
            }
            if range.end < cooldown.end {
                self.cooldowns.insert(range.end, cooldown);
            }
        }
        (attempts, charged)
    }

    /// The latest instant a cooldown overlapping `range` holds it back
    /// until, if one still does at `now`.
    pub fn cooling_until(&self, range: &Range<u64>, now: Instant) -> Option<Instant> {
        self.overlapping_cooldowns(range)
            .map(|start| self.cooldowns[&start].not_before)
            .filter(|not_before| *not_before > now)
            .max()
    }

    /// The earliest instant after `now` a cooldown ends.
    pub fn next_cooldown_end(&self, now: Instant) -> Option<Instant> {
        self.cooldowns
            .values()
            .map(|cooldown| cooldown.not_before)
            .filter(|not_before| *not_before > now)
            .min()
    }

    /// The cooldowns, as ranges, in order.
    #[cfg(test)]
    pub fn cooldowns(&self) -> Vec<(Range<u64>, Cooldown)> {
        self.cooldowns
            .iter()
            .map(|(&start, &cooldown)| (start..cooldown.end, cooldown))
            .collect()
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
            .ok_or(Error::RunnerNotFound { runner_id })?;

        let chunk = self
            .chunks
            .remove(&range)
            .ok_or(Error::ChunkNotFound { runner_id })?;

        // Validate split position; a hedged chunk is never split.
        if split_pos <= chunk.downloaded.end
            || split_pos >= chunk.allocated.end
            || chunk.hedge.is_some()
        {
            self.chunks.insert(range, chunk);
            self.runner_to_chunk.insert(runner_id, range);
            return Err(Error::InvalidSplitPosition {
                position: split_pos,
            });
        }

        // Create two new chunks
        let first_range = chunk.allocated.start..split_pos;
        let second_range = split_pos..chunk.allocated.end;

        // First chunk keeps the downloaded progress
        let mut first_chunk = chunk;
        first_chunk.allocated = first_range.clone();

        // Second chunk starts fresh
        let second_chunk = ChunkState {
            generation: self.take_generation(),
            ..ChunkState::from_runner_id(second_range.clone(), Some(new_runner_id))
        };

        // Insert both chunks
        self.chunks.insert(first_range.start, first_chunk);
        self.chunks.insert(split_pos, second_chunk);

        // Update runner mappings
        self.runner_to_chunk.insert(runner_id, first_range.start);
        self.runner_to_chunk.insert(new_runner_id, split_pos);

        Ok(second_range)
    }

    /// Get available ranges (not allocated to any chunk)
    pub fn get_available_ranges(&self) -> Vec<Range<u64>> {
        let mut ranges = Vec::new();
        let mut cursor = 0;
        for chunk in self.chunks.values() {
            if cursor < chunk.allocated.start {
                ranges.push(cursor..chunk.allocated.start);
            }
            cursor = chunk.allocated.end;
        }
        if cursor < self.total {
            ranges.push(cursor..self.total);
        }
        ranges
    }

    /// The available ranges a runner may claim at `now`: those of
    /// [`Self::get_available_ranges`] without the cooldowns that have not
    /// ended. A cooldown that ended merges with the fresh bytes around it.
    pub fn get_eligible_ranges(&self, now: Instant) -> Vec<Range<u64>> {
        // Cooldowns lie within the available ranges, in the same order.
        let mut cooling = self
            .cooldowns
            .iter()
            .filter(|(_, cooldown)| cooldown.not_before > now)
            .map(|(&start, cooldown)| start..cooldown.end)
            .peekable();
        let mut ranges = Vec::new();
        for available in self.get_available_ranges() {
            let mut cursor = available.start;
            while let Some(cooldown) = cooling.next_if(|cooldown| cooldown.start < available.end) {
                if cursor < cooldown.start {
                    ranges.push(cursor..cooldown.start);
                }
                cursor = cursor.max(cooldown.end);
            }
            if cursor < available.end {
                ranges.push(cursor..available.end);
            }
        }
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
        debug_assert_eq!(
            self.accepted_bytes,
            ranges.iter().map(|r| r.end - r.start).sum::<u64>()
        );

        // Merge adjacent ranges
        merge_adjacent_ranges(ranges)
    }

    /// The write of `range` completed: its bytes are in the file (design
    /// §11.1). Only bytes the planner accepted are ever written; a frame it
    /// did not accept, a hedge loser's among them, never reaches the writer.
    pub fn note_written(&mut self, range: Range<u64>) {
        debug_assert!(
            range.is_empty()
                || self
                    .get_downloaded_ranges()
                    .iter()
                    .any(
                        |downloaded| downloaded.start <= range.start && range.end <= downloaded.end
                    ),
            "written {range:?} was never accepted"
        );
        self.written.insert(range);
    }

    /// The accepted bytes written to the file, see [`Self::note_written`].
    #[inline]
    pub fn written(&self) -> &WrittenSet {
        &self.written
    }

    /// Get total downloaded bytes
    #[inline]
    pub fn get_total_downloaded(&self) -> u64 {
        self.accepted_bytes
    }

    /// The running runner with the most left to download, and its
    /// incomplete range; `None` when no running chunk is incomplete.
    ///
    /// A connecting runner is left alone: its request already covers its
    /// whole range. So is every runner `skip` names, and every chunk hedged
    /// in this attempt (design §9.6).
    pub fn largest_running_chunk(
        &self,
        skip: impl Fn(RunnerId) -> bool,
    ) -> Option<(RunnerId, Range<u64>)> {
        self.chunks
            .values()
            .filter(|c| c.status == ChunkStatus::Running && !c.is_complete())
            .filter(|c| c.hedge.is_none())
            .filter_map(|c| Some((c.runner_id?, c)))
            .filter(|&(runner_id, _)| !skip(runner_id))
            .max_by_key(|(_, c)| c.remaining())
            .map(|(runner_id, c)| (runner_id, c.incomplete_range()))
    }

    /// Get state of a specific runner
    pub fn get_runner_state(&self, runner_id: RunnerId) -> Option<&ChunkState> {
        self.runner_to_chunk
            .get(&runner_id)
            .and_then(|range| self.chunks.get(range))
    }

    /// Fork a hedge of the chunk of `owner` for the runner `hedge` (design
    /// §9.6); returns the range it requests: the rest of the chunk, from
    /// the bytes accepted so far.
    ///
    /// Only a connecting or running chunk that is incomplete and was not
    /// hedged in this attempt may be hedged, by a runner without a chunk.
    pub fn fork_hedge(&mut self, owner: RunnerId, hedge: RunnerId) -> Result<Range<u64>, Error> {
        let invalid = Error::InvalidHedge { runner_id: owner };
        let start = *self
            .runner_to_chunk
            .get(&owner)
            .ok_or(Error::RunnerNotFound { runner_id: owner })?;
        if self.runner_to_chunk.contains_key(&hedge) || self.hedge_of.contains_key(&hedge) {
            return Err(invalid);
        }
        let chunk = self
            .chunks
            .get_mut(&start)
            .ok_or(Error::ChunkNotFound { runner_id: owner })?;
        if !chunk.status.is_active() || chunk.is_complete() || chunk.hedge.is_some() {
            return Err(invalid);
        }
        chunk.hedge = Some(Hedge {
            fork: chunk.downloaded.end,
            owner_side: Side::Original,
            first: None,
            undecided: true,
            partner: Some(Partner {
                runner_id: hedge,
                running: false,
            }),
        });
        self.hedge_of.insert(hedge, start);
        Ok(chunk.incomplete_range())
    }

    /// The chunk of the undecided pair whose hedge runner is `runner_id`,
    /// while both sides run.
    pub fn partner_chunk(&self, runner_id: RunnerId) -> Option<&ChunkState> {
        self.hedge_of
            .get(&runner_id)
            .and_then(|start| self.chunks.get(start))
    }

    /// The hedge runner `runner_id` reported `Started`.
    pub fn mark_partner_running(&mut self, runner_id: RunnerId) -> Result<(), Error> {
        let partner = self
            .hedge_of
            .get(&runner_id)
            .and_then(|start| self.chunks.get_mut(start))
            .and_then(|chunk| chunk.hedge.as_mut()?.partner.as_mut())
            .ok_or(Error::PartnerNotFound { runner_id })?;
        partner.running = true;
        Ok(())
    }

    /// The hedge runner `runner_id` leaves its undecided pair, failed or
    /// beaten: the original runner keeps the chunk. Returns the original
    /// runner.
    pub fn drop_partner(&mut self, runner_id: RunnerId) -> Result<RunnerId, Error> {
        let start = self
            .hedge_of
            .remove(&runner_id)
            .ok_or(Error::PartnerNotFound { runner_id })?;
        let chunk = self
            .chunks
            .get_mut(&start)
            .expect("an undecided pair has a chunk");
        chunk
            .hedge
            .as_mut()
            .expect("an undecided pair is hedged")
            .partner = None;
        Ok(chunk.runner_id.expect("an undecided pair has an owner"))
    }

    /// The original runner `owner` leaves its undecided pair, failed or
    /// beaten: the hedge runner takes the chunk over, with its downloaded
    /// part and its counts. Returns the hedge runner.
    pub fn hand_over(&mut self, owner: RunnerId) -> Result<RunnerId, Error> {
        let start = *self
            .runner_to_chunk
            .get(&owner)
            .ok_or(Error::RunnerNotFound { runner_id: owner })?;
        let chunk = self
            .chunks
            .get_mut(&start)
            .ok_or(Error::ChunkNotFound { runner_id: owner })?;
        let hedge = chunk
            .hedge
            .as_mut()
            .ok_or(Error::PartnerNotFound { runner_id: owner })?;
        let partner = hedge
            .partner
            .take()
            .ok_or(Error::PartnerNotFound { runner_id: owner })?;
        hedge.owner_side = Side::Hedge;
        chunk.runner_id = Some(partner.runner_id);
        chunk.status = if partner.running {
            ChunkStatus::Running
        } else {
            ChunkStatus::Pending
        };
        self.hedge_of.remove(&partner.runner_id);
        self.runner_to_chunk.remove(&owner);
        self.runner_to_chunk.insert(partner.runner_id, start);
        Ok(partner.runner_id)
    }

    /// The side that runs on in the chunk of `owner` took over from one
    /// that failed as `failure`, before either delivered since the fork.
    pub fn record_side_failure(&mut self, owner: RunnerId, failure: SideFailure) {
        if let Some(hedge) = self
            .runner_to_chunk
            .get(&owner)
            .and_then(|start| self.chunks.get_mut(start))
            .and_then(|chunk| chunk.hedge.as_mut())
        {
            debug_assert!(hedge.first.is_none(), "one side fails first");
            hedge.first = Some(failure);
        }
    }

    /// A non-empty frame of `owner` is taken: its pair, if undecided, is
    /// decided. The hedge stays, for the budget of the attempt.
    pub fn decide(&mut self, owner: RunnerId) {
        if let Some(hedge) = self
            .runner_to_chunk
            .get(&owner)
            .and_then(|start| self.chunks.get_mut(start))
            .and_then(|chunk| chunk.hedge.as_mut())
        {
            debug_assert!(hedge.partner.is_none(), "the beaten side left first");
            hedge.undecided = false;
        }
    }

    /// The bytes the attempt of `owner` delivered itself: of a hedged
    /// chunk's runner, only those of its own side (design §9.6).
    pub fn owner_bytes(&self, owner: RunnerId) -> Option<u64> {
        let chunk = self.get_runner_state(owner)?;
        Some(match &chunk.hedge {
            Some(hedge) => hedge.owner_bytes(&chunk.downloaded),
            None => chunk.downloaded.end - chunk.downloaded.start,
        })
    }

    /// Shrink the runner's chunk to end `new_size` bytes after its downloaded part.
    ///
    /// The chunk may only shrink and must stay non-empty; otherwise nothing
    /// changes and [`Error::InvalidResize`] is returned.
    pub fn resize_runner_state(
        &mut self,
        runner_id: RunnerId,
        new_size: u64,
    ) -> Result<Resize, Error> {
        let start = *self
            .runner_to_chunk
            .get(&runner_id)
            .ok_or(Error::RunnerNotFound { runner_id })?;
        let state = self
            .chunks
            .get_mut(&start)
            .ok_or(Error::ChunkNotFound { runner_id })?;
        // A hedged chunk is never split (design §9.6): its hedge runner
        // requested up to its end, and counts its limit from the fork.
        if state.hedge.is_some() {
            return Err(Error::InvalidResize {
                runner_id,
                new_size,
            });
        }
        let new_end = match state.downloaded.end.checked_add(new_size) {
            Some(end) if end == state.allocated.end => return Ok(Resize::Unchanged),
            Some(end) if state.allocated.start < end && end < state.allocated.end => end,
            _ => {
                return Err(Error::InvalidResize {
                    runner_id,
                    new_size,
                });
            }
        };
        // The start, and so the key, is unchanged.
        state.allocated.end = new_end;
        Ok(Resize::Shrunk {
            limit: new_end - state.allocated.start,
        })
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

pub struct PlannerGuard<'a> {
    planner: &'a mut ChunkPlanner,
    snapshot: ChunkPlanner,
    committed: bool,
}

impl<'a> PlannerGuard<'a> {
    pub fn new(planner: &'a mut ChunkPlanner) -> Self {
        let snapshot = planner.clone();
        Self {
            planner,
            snapshot,
            committed: false,
        }
    }

    pub fn planner(&mut self) -> &mut ChunkPlanner {
        self.planner
    }

    pub fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for PlannerGuard<'_> {
    fn drop(&mut self) {
        if !self.committed {
            *self.planner = self.snapshot.clone();
        }
    }
}

/// Merge adjacent ranges
/// Byte ranges, sorted, neither overlapping nor adjacent: the bytes written
/// to the file (design §11.1).
///
/// A set rather than an offset per chunk: the writer completes the writes
/// of one chunk out of order, and the set does not depend on how the
/// chunks are split, retried or hedged (design §14.21).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WrittenSet(Vec<Range<u64>>);

impl WrittenSet {
    /// Add `range`, merging it with the ranges it overlaps or touches.
    pub fn insert(&mut self, range: Range<u64>) {
        if range.is_empty() {
            return;
        }
        // The ranges from `first` on end at or after `range` starts; those
        // before `last` start at or before it ends.
        let first = self.0.partition_point(|r| r.end < range.start);
        let last = self.0.partition_point(|r| r.start <= range.end);
        let merged = if first < last {
            self.0[first].start.min(range.start)..self.0[last - 1].end.max(range.end)
        } else {
            range
        };
        self.0.splice(first..last, [merged]);
    }

    /// The ranges, in order.
    #[inline]
    pub fn ranges(&self) -> &[Range<u64>] {
        &self.0
    }

    #[inline]
    pub fn into_ranges(self) -> Vec<Range<u64>> {
        self.0
    }
}

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

    /// The written ranges merge whatever order the writes complete in, a
    /// range written again included.
    #[test]
    #[expect(clippy::single_range_in_vec_init, reason = "lists holding one range")]
    fn test_written_ranges_merge_in_any_order() {
        let mut set = WrittenSet::default();
        for range in [8..12, 0..4, 20..24, 4..8, 0..0, 2..6, 12..16, 30..31] {
            set.insert(range);
        }
        assert_eq!(set.ranges(), [0..16, 20..24, 30..31]);
        set.insert(16..30);
        assert_eq!(set.ranges(), [0..31]);
    }

    /// The written bytes lag the accepted ones until their writes complete,
    /// and a split, a failure or a retry leaves them as they are.
    #[test]
    #[expect(clippy::single_range_in_vec_init, reason = "lists holding one range")]
    fn test_written_lags_downloaded() {
        let now = Instant::now();
        let mut planner = ChunkPlanner::new(1000);
        assert!(planner.allocate_chunk(0..1000, Some(1)));
        planner.mark_running(1).unwrap();
        let first = planner.update_progress(1, 100).unwrap();
        let second = planner.update_progress(1, 100).unwrap();
        assert_eq!(planner.get_downloaded_ranges(), [0..200]);
        assert_eq!(planner.written().ranges(), []);

        // The second write completes first.
        planner.note_written(second);
        assert_eq!(planner.written().ranges(), [100..200]);
        planner.split_chunk(1, 600, 2).unwrap();
        fail(&mut planner, 1, now, std::time::Duration::from_secs(1));
        planner.note_written(first);
        assert_eq!(planner.written().ranges(), [0..200]);
        assert_eq!(planner.get_downloaded_ranges(), [0..200]);
    }

    // === Basic functionality tests ===

    // === Accepted bytes ===

    fn summed_downloaded(planner: &ChunkPlanner) -> u64 {
        planner
            .chunks
            .values()
            .map(|c| c.downloaded.end - c.downloaded.start)
            .sum()
    }

    /// A planner with runner 1 on `0..600` and runner 2 on `600..1000`,
    /// each part way through.
    fn planner_in_progress() -> ChunkPlanner {
        let mut planner = ChunkPlanner::new(1000);
        assert!(planner.allocate_chunk(0..600, Some(1)));
        assert!(planner.allocate_chunk(600..1000, Some(2)));
        planner.mark_running(1).unwrap();
        planner.mark_running(2).unwrap();
        planner.update_progress(1, 200).unwrap();
        planner.update_progress(2, 50).unwrap();
        assert_eq!(planner.get_total_downloaded(), 250);
        planner
    }

    #[test]
    fn test_accepted_bytes_matches_sum_after_split() {
        let mut planner = planner_in_progress();
        planner.split_chunk(1, 400, 3).unwrap();
        planner.update_progress(3, 10).unwrap();
        assert_eq!(planner.get_total_downloaded(), 260);
        assert_eq!(planner.get_total_downloaded(), summed_downloaded(&planner));
    }

    #[test]
    fn test_accepted_bytes_matches_sum_after_fail() {
        let mut planner = planner_in_progress();
        planner.mark_failed(1).unwrap();
        assert!(planner.allocate_chunk(200..600, Some(3)));
        planner.mark_failed(3).unwrap();
        assert_eq!(planner.get_total_downloaded(), 250);
        assert_eq!(planner.get_total_downloaded(), summed_downloaded(&planner));
    }

    #[test]
    fn test_accepted_bytes_matches_sum_after_resize() {
        let mut planner = planner_in_progress();
        planner.resize_runner_state(1, 100).unwrap();
        // Progress beyond the new end is clipped.
        assert_eq!(planner.update_progress(1, 500).unwrap(), 200..300);
        assert_eq!(planner.get_total_downloaded(), 350);
        assert_eq!(planner.get_total_downloaded(), summed_downloaded(&planner));
    }

    #[test]
    fn test_accepted_bytes_matches_sum_after_rollback() {
        let mut planner = planner_in_progress();
        {
            let mut guard = PlannerGuard::new(&mut planner);
            guard.planner().update_progress(2, 100).unwrap();
            guard.planner().mark_failed(1).unwrap();
        }
        assert_eq!(planner.get_total_downloaded(), 250);
        assert_eq!(planner.get_total_downloaded(), summed_downloaded(&planner));
        assert_eq!(planner.get_downloaded_ranges(), vec![0..200, 600..650]);
    }

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
        let state = ChunkState::from_runner_id(100..500, Some(1));
        assert_eq!(state.allocated, 100..500);
        assert_eq!(state.downloaded, 100..100);
        assert_eq!(state.runner_id, Some(1));
        assert_eq!(state.status, ChunkStatus::Running);
        assert_eq!(state.remaining(), 400);
        assert_eq!(state.incomplete_range(), 100..500);
        assert!(!state.is_complete());

        let state_idle = ChunkState::from_runner_id(0..100, None);
        assert_eq!(state_idle.status, ChunkStatus::Idle);
        assert_eq!(state_idle.runner_id, None);
    }

    #[test]
    fn test_basic_allocation() {
        let mut planner = ChunkPlanner::new(1000);

        // Normal allocation (creates Pending state)
        assert!(planner.allocate_chunk(0..300, Some(1)));
        assert_eq!(planner.get_chunks_count(), 1);
        assert_eq!(planner.get_active_runners_count(), 1);

        // Transition to running
        assert!(planner.mark_running(1).is_ok());

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

        // Allocate chunk with runner and mark as running
        assert!(planner.allocate_chunk(0..500, Some(1)));
        assert!(planner.mark_running(1).is_ok());

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

        // Allocate and mark as running
        assert!(planner.allocate_chunk(0..600, Some(1)));
        assert!(planner.mark_running(1).is_ok());

        // Partially download
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

        // Allocate, mark as running and partially download
        assert!(planner.allocate_chunk(100..500, Some(1)));
        assert!(planner.mark_running(1).is_ok());
        assert!(planner.update_progress(1, 150).is_ok());

        // Mark as failed
        let unfinished = planner.mark_failed(1).unwrap().range;
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
    #[expect(
        clippy::reversed_empty_ranges,
        reason = "tests that reversed ranges are rejected"
    )]
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
            Err(Error::RunnerNotFound { runner_id: 99 })
        ));

        // Allocate a chunk then try to mark another runner
        assert!(planner.allocate_chunk(0..100, Some(1)));
        assert!(matches!(
            planner.mark_finished(2),
            Err(Error::RunnerNotFound { runner_id: 2 })
        ));
    }

    #[test]
    fn test_mark_failed_nonexistent_runner() {
        let mut planner = ChunkPlanner::new(1000);

        // Try to mark non-existent runner as failed
        assert!(matches!(
            planner.mark_failed(99),
            Err(Error::RunnerNotFound { runner_id: 99 })
        ));
    }

    #[test]
    fn test_split_chunk_errors() {
        let mut planner = ChunkPlanner::new(1000);

        // Try to split non-existent chunk
        assert!(matches!(
            planner.split_chunk(99, 500, 2),
            Err(Error::RunnerNotFound { runner_id: 99 })
        ));

        // Allocate chunk and partially download
        assert!(planner.allocate_chunk(100..500, Some(1)));
        assert!(planner.update_progress(1, 100).is_ok());

        // Invalid split positions
        assert!(matches!(
            planner.split_chunk(1, 200, 2),
            Err(Error::InvalidSplitPosition { position: 200 })
        )); // Within downloaded range
        assert!(matches!(
            planner.split_chunk(1, 150, 2),
            Err(Error::InvalidSplitPosition { position: 150 })
        )); // Within downloaded range
        assert!(matches!(
            planner.split_chunk(1, 500, 2),
            Err(Error::InvalidSplitPosition { position: 500 })
        )); // On chunk boundary
        assert!(matches!(
            planner.split_chunk(1, 600, 2),
            Err(Error::InvalidSplitPosition { position: 600 })
        )); // Beyond chunk range
    }

    #[test]
    fn test_resize_runner_state_errors() {
        let mut planner = ChunkPlanner::new(1000);

        // Try to resize non-existent runner
        assert!(matches!(
            planner.resize_runner_state(99, 100),
            Err(Error::RunnerNotFound { runner_id: 99 })
        ));

        // Allocate chunk and test resizing
        assert!(planner.allocate_chunk(100..500, Some(1)));
        assert!(planner.update_progress(1, 100).is_ok());

        // Resize
        assert_eq!(
            planner.resize_runner_state(1, 200).unwrap(),
            Resize::Shrunk { limit: 300 }
        );
        let state = planner.get_runner_state(1).unwrap();
        assert_eq!(state.allocated, 100..400); // 200 + 200 = 400
    }

    /// Both maps, compared by content.
    fn snapshot(planner: &ChunkPlanner) -> (Vec<String>, Vec<(RunnerId, Range<u64>)>) {
        let mut chunks: Vec<_> = planner.iter_chunks().map(|c| format!("{c:?}")).collect();
        chunks.sort();
        let mut runners: Vec<_> = planner
            .iter_chunks()
            .filter_map(|c| c.runner_id)
            .map(|id| (id, planner.get_runner_state(id).unwrap().allocated.clone()))
            .collect();
        runners.sort_by_key(|(id, _)| *id);
        (chunks, runners)
    }

    fn assert_invalid_resize(planner: &mut ChunkPlanner, runner_id: RunnerId, new_size: u64) {
        let before = snapshot(planner);
        assert!(matches!(
            planner.resize_runner_state(runner_id, new_size),
            Err(Error::InvalidResize { .. })
        ));
        assert_eq!(
            snapshot(planner),
            before,
            "a rejected resize must not change the maps"
        );
    }

    #[test]
    fn test_resize_cannot_grow() {
        let mut planner = ChunkPlanner::new(100);
        assert!(planner.allocate_chunk(0..10, Some(1)));
        assert!(planner.allocate_chunk(20..30, Some(2)));
        assert_invalid_resize(&mut planner, 1, 40);
    }

    #[test]
    fn test_resize_to_empty_rejected() {
        let mut planner = ChunkPlanner::new(100);
        assert!(planner.allocate_chunk(10..20, Some(1)));
        assert_invalid_resize(&mut planner, 1, 0);
    }

    #[test]
    fn test_resize_overflow_rejected() {
        let mut planner = ChunkPlanner::new(100);
        assert!(planner.allocate_chunk(10..20, Some(1)));
        assert!(planner.update_progress(1, 5).is_ok());
        assert_invalid_resize(&mut planner, 1, u64::MAX);
    }

    #[test]
    fn test_resize_unchanged_is_noop() {
        let mut planner = ChunkPlanner::new(100);
        assert!(planner.allocate_chunk(10..20, Some(1)));
        assert!(planner.update_progress(1, 4).is_ok());
        let before = snapshot(&planner);
        assert_eq!(
            planner.resize_runner_state(1, 6).unwrap(),
            Resize::Unchanged
        );
        assert_eq!(snapshot(&planner), before);
    }

    #[test]
    fn test_progress_near_u64_max_does_not_overflow() {
        let mut planner = ChunkPlanner::new(u64::MAX);
        assert!(planner.allocate_chunk(u64::MAX - 10..u64::MAX, Some(1)));
        assert!(planner.update_progress(1, 5).is_ok());
        assert_eq!(
            planner.update_progress(1, u64::MAX).unwrap(),
            u64::MAX - 5..u64::MAX
        );
        assert!(planner.get_runner_state(1).unwrap().is_complete());
    }

    #[test]
    fn test_guard_drop_restores_both_maps() {
        let mut planner = ChunkPlanner::new(100);
        assert!(planner.allocate_chunk(0..50, Some(1)));
        let before = snapshot(&planner);
        {
            let mut guard = PlannerGuard::new(&mut planner);
            guard.planner().resize_runner_state(1, 20).unwrap();
            assert!(guard.planner().allocate_chunk(20..50, Some(2)));
        }
        assert_eq!(snapshot(&planner), before);
    }

    #[test]
    fn test_guard_commit_keeps_changes() {
        let mut planner = ChunkPlanner::new(100);
        assert!(planner.allocate_chunk(0..50, Some(1)));
        {
            let mut guard = PlannerGuard::new(&mut planner);
            guard.planner().resize_runner_state(1, 20).unwrap();
            assert!(guard.planner().allocate_chunk(20..50, Some(2)));
            guard.commit();
        }
        assert_eq!(planner.get_runner_state(1).unwrap().allocated, 0..20);
        assert_eq!(planner.get_runner_state(2).unwrap().allocated, 20..50);
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
        let unfinished = planner.mark_failed(2).unwrap().range;
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
        let unfinished = planner.mark_failed(2).unwrap().range;
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
    #[expect(clippy::single_range_in_vec_init, reason = "lists holding one range")]
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
    fn test_overlap_with_predecessor_rejected() {
        let mut planner = ChunkPlanner::new(100);
        assert!(planner.allocate_chunk(10..30, None));
        assert!(!planner.allocate_chunk(20..40, None));
        assert!(!planner.allocate_chunk(15..20, None));
    }

    #[test]
    fn test_overlap_with_chunk_starting_inside_request_rejected() {
        let mut planner = ChunkPlanner::new(100);
        assert!(planner.allocate_chunk(10..20, None));
        assert!(planner.allocate_chunk(50..60, None));
        assert!(!planner.allocate_chunk(30..55, None));
        assert!(!planner.allocate_chunk(0..100, None));
    }

    #[test]
    fn test_adjacent_allowed() {
        let mut planner = ChunkPlanner::new(100);
        assert!(planner.allocate_chunk(10..20, None));
        assert!(planner.allocate_chunk(20..30, None));
        assert!(planner.allocate_chunk(0..10, None));
        assert_eq!(planner.get_available_ranges(), vec![30..100]);
    }

    #[test]
    fn test_available_ranges_empty_full_and_gaps_at_both_ends() {
        let mut planner = ChunkPlanner::new(100);
        assert_eq!(planner.get_available_ranges(), vec![0..100]);
        assert!(planner.allocate_chunk(40..60, None));
        assert_eq!(planner.get_available_ranges(), vec![0..40, 60..100]);
        assert!(planner.allocate_chunk(0..40, None));
        assert!(planner.allocate_chunk(60..100, None));
        assert!(planner.get_available_ranges().is_empty());
        assert!(ChunkPlanner::new(0).get_available_ranges().is_empty());
    }

    #[test]
    fn test_failed_prefix_then_tail_reallocation() {
        let mut planner = ChunkPlanner::new(100);
        assert!(planner.allocate_chunk(0..100, Some(1)));
        planner.update_progress(1, 30).unwrap();
        assert_eq!(planner.mark_failed(1).unwrap().range, 30..100);
        assert_eq!(planner.get_available_ranges(), vec![30..100]);
        assert!(planner.allocate_chunk(30..100, Some(2)));
        assert_eq!(planner.get_runner_state(2).unwrap().allocated, 30..100);
        assert!(planner.get_available_ranges().is_empty());
    }

    #[test]
    fn test_rejected_split_restores_maps() {
        let mut planner = ChunkPlanner::new(100);
        assert!(planner.allocate_chunk(0..100, Some(1)));
        planner.update_progress(1, 50).unwrap();
        let before = snapshot(&planner);
        assert!(planner.split_chunk(1, 40, 2).is_err());
        assert_eq!(snapshot(&planner), before);
        assert!(planner.get_runner_state(2).is_none());
    }

    /// Every chunk handed out is a new generation, the same ID claiming the
    /// same start again included; a chunk keeps its own when it shrinks or
    /// is split.
    #[test]
    fn test_every_claim_is_a_new_generation() {
        let mut planner = ChunkPlanner::new(100);
        assert!(planner.allocate_chunk(0..50, Some(0)));
        let generation =
            |planner: &ChunkPlanner, id| planner.get_runner_state(id).unwrap().generation;
        let first = generation(&planner, 0);
        planner.resize_runner_state(0, 25).unwrap();
        assert_eq!(generation(&planner, 0), first);
        planner.split_chunk(0, 10, 1).unwrap();
        assert_eq!(generation(&planner, 0), first);
        let split = generation(&planner, 1);
        assert_ne!(split, first);

        // Failed before a byte: its range is free, and claimed again.
        assert_eq!(planner.mark_failed(0).unwrap().range, 0..10);
        assert!(planner.allocate_chunk(0..10, Some(0)));
        let retry = generation(&planner, 0);
        assert!(retry != first && retry != split, "{retry}");
    }

    #[test]
    fn test_split_mappings() {
        let mut planner = ChunkPlanner::new(100);
        assert!(planner.allocate_chunk(0..100, Some(1)));
        planner.update_progress(1, 10).unwrap();
        assert_eq!(planner.split_chunk(1, 60, 2).unwrap(), 60..100);
        let first = planner.get_runner_state(1).unwrap();
        assert_eq!(
            (first.allocated.clone(), first.downloaded.clone()),
            (0..60, 0..10)
        );
        let second = planner.get_runner_state(2).unwrap();
        assert_eq!(
            (second.allocated.clone(), second.downloaded.clone()),
            (60..100, 60..60)
        );
        planner.update_progress(2, 5).unwrap();
        assert_eq!(planner.get_runner_state(2).unwrap().downloaded, 60..65);
    }

    #[test]
    fn test_shrink_then_reallocate_freed_tail() {
        let mut planner = ChunkPlanner::new(100);
        assert!(planner.allocate_chunk(0..100, Some(1)));
        planner.update_progress(1, 20).unwrap();
        planner.resize_runner_state(1, 30).unwrap();
        assert_eq!(planner.get_available_ranges(), vec![50..100]);
        assert!(planner.allocate_chunk(50..100, Some(2)));
        assert_eq!(planner.get_runner_state(1).unwrap().allocated, 0..50);
        assert!(planner.get_available_ranges().is_empty());
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
        let unfinished = planner.mark_failed(1).unwrap().range;
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

    #[test]
    fn test_pending_chunk_allocation() {
        let mut planner = ChunkPlanner::new(1000);

        // Allocate pending chunk
        assert!(planner.allocate_pending_chunk(0..300, 1));
        assert_eq!(planner.get_chunks_count(), 1);
        assert_eq!(planner.get_active_runners_count(), 1);

        // Check state is pending
        let state = planner.get_runner_state(1).unwrap();
        assert_eq!(state.allocated, 0..300);
        assert_eq!(state.status, ChunkStatus::Pending);
    }

    #[test]
    fn test_pending_to_running_transition() {
        let mut planner = ChunkPlanner::new(1000);

        // Allocate pending chunk
        assert!(planner.allocate_pending_chunk(0..500, 1));
        let state = planner.get_runner_state(1).unwrap();
        assert_eq!(state.status, ChunkStatus::Pending);

        // Mark as running
        assert!(planner.mark_running(1).is_ok());

        // Check state is now running
        let state = planner.get_runner_state(1).unwrap();
        assert_eq!(state.status, ChunkStatus::Running);
        assert_eq!(state.allocated, 0..500);
    }

    #[test]
    fn test_pending_chunk_failure() {
        let mut planner = ChunkPlanner::new(1000);

        // Allocate pending chunk
        assert!(planner.allocate_pending_chunk(100..500, 1));

        // Mark as failed (simulating connection failure)
        let unfinished = planner.mark_failed(1).unwrap().range;
        assert_eq!(unfinished, 100..500);

        // Chunk should be removed
        assert!(planner.get_runner_state(1).is_none());
        assert_eq!(planner.get_chunks_count(), 0);
    }

    #[test]
    fn test_multiple_pending_chunks() {
        let mut planner = ChunkPlanner::new(1000);

        // Allocate multiple pending chunks
        assert!(planner.allocate_pending_chunk(0..250, 1));
        assert!(planner.allocate_pending_chunk(250..500, 2));
        assert!(planner.allocate_pending_chunk(500..750, 3));
        assert!(planner.allocate_pending_chunk(750..1000, 4));

        assert_eq!(planner.get_chunks_count(), 4);
        assert_eq!(planner.get_active_runners_count(), 4);

        // Transition some to running
        assert!(planner.mark_running(1).is_ok());
        assert!(planner.mark_running(3).is_ok());

        // Check states
        assert_eq!(
            planner.get_runner_state(1).unwrap().status,
            ChunkStatus::Running
        );
        assert_eq!(
            planner.get_runner_state(2).unwrap().status,
            ChunkStatus::Pending
        );
        assert_eq!(
            planner.get_runner_state(3).unwrap().status,
            ChunkStatus::Running
        );
        assert_eq!(
            planner.get_runner_state(4).unwrap().status,
            ChunkStatus::Pending
        );

        // Active count should still be 4 (pending is also active)
        assert_eq!(planner.get_active_runners_count(), 4);
    }

    #[test]
    fn test_pending_then_progress_update() {
        let mut planner = ChunkPlanner::new(1000);

        // Allocate pending chunk
        assert!(planner.allocate_pending_chunk(0..500, 1));

        // Mark as running
        assert!(planner.mark_running(1).is_ok());

        // Update progress
        assert!(planner.update_progress(1, 100).is_ok());
        assert!(planner.update_progress(1, 200).is_ok());

        let state = planner.get_runner_state(1).unwrap();
        assert_eq!(state.downloaded, 0..300);
        assert_eq!(state.status, ChunkStatus::Running);
    }

    // === Backoff of failed ranges (design §8.1) ===

    const SECOND: std::time::Duration = std::time::Duration::from_secs(1);
    const NANO: std::time::Duration = std::time::Duration::from_nanos(1);

    /// Fail runner `runner_id` at `now`, charging the failure to its range,
    /// and back its range off for `delay`.
    fn fail(
        planner: &mut ChunkPlanner,
        runner_id: RunnerId,
        now: Instant,
        delay: std::time::Duration,
    ) -> Released {
        let released = planner.mark_failed(runner_id).unwrap();
        planner.cool_down(
            released.range.clone(),
            released.attempts,
            released.charged + 1,
            now + delay,
        );
        released
    }

    /// A cooldown whose failures were all charged, as [`fail`] charges them.
    fn cooldown(end: u64, attempts: u32, not_before: Instant) -> Cooldown {
        Cooldown {
            end,
            attempts,
            charged: attempts,
            not_before,
        }
    }

    #[test]
    fn test_failed_range_is_claimable_only_after_its_cooldown() {
        let now = Instant::now();
        let mut planner = ChunkPlanner::new(1000);
        assert!(planner.allocate_chunk(0..100, Some(1)));
        planner.mark_running(1).unwrap();
        planner.update_progress(1, 30).unwrap();
        assert_eq!(
            fail(&mut planner, 1, now, SECOND),
            Released {
                range: 30..100,
                attempts: 1,
                charged: 0,
                received: 30,
            }
        );

        let end = now + SECOND;
        // Available, but not claimable yet.
        assert_eq!(planner.get_available_ranges(), vec![30..1000]);
        assert_eq!(planner.get_eligible_ranges(end - NANO), vec![100..1000]);
        assert_eq!(planner.cooling_until(&(30..100), end - NANO), Some(end));
        assert_eq!(planner.cooling_until(&(50..60), now), Some(end));
        assert_eq!(planner.cooling_until(&(0..31), now), Some(end));
        assert_eq!(planner.cooling_until(&(100..1000), now), None);
        assert_eq!(planner.next_cooldown_end(now), Some(end));
        // Its cooldown over, it merges with the fresh bytes after it.
        assert_eq!(planner.get_eligible_ranges(end), vec![30..1000]);
        assert_eq!(planner.cooling_until(&(30..100), end), None);
        assert_eq!(planner.next_cooldown_end(end), None);
    }

    #[test]
    fn test_claimed_range_counts_on_from_its_attempts() {
        let now = Instant::now();
        let mut planner = ChunkPlanner::new(1000);
        assert!(planner.allocate_chunk(0..1000, Some(1)));
        assert_eq!(planner.get_runner_state(1).unwrap().attempts, 0);
        fail(&mut planner, 1, now, SECOND);
        assert!(planner.allocate_chunk(0..1000, Some(2)));
        assert_eq!(planner.get_runner_state(2).unwrap().attempts, 1);
        assert!(planner.cooldowns().is_empty());
        assert_eq!(fail(&mut planner, 2, now, SECOND).attempts, 2);
        assert!(planner.allocate_chunk(0..1000, Some(3)));
        assert_eq!(fail(&mut planner, 3, now, SECOND).attempts, 3);
        assert_eq!(
            planner.cooldowns(),
            vec![(0..1000, cooldown(1000, 3, now + SECOND))]
        );
    }

    /// A claim of part of a cooldown carries its attempts; the rest keeps
    /// backing off.
    #[test]
    fn test_partial_claim_leaves_the_rest_backing_off() {
        let now = Instant::now();
        let end = now + SECOND;
        let mut planner = ChunkPlanner::new(1000);
        planner.cool_down(100..400, 2, 2, end);
        assert!(planner.allocate_chunk(200..300, Some(1)));
        assert_eq!(planner.get_runner_state(1).unwrap().attempts, 2);
        assert_eq!(
            planner.cooldowns(),
            vec![
                (100..200, cooldown(200, 2, end)),
                (300..400, cooldown(400, 2, end))
            ]
        );
        // Fresh bytes and part of a cooldown: the cooldown's attempts.
        assert!(planner.allocate_chunk(0..150, Some(2)));
        assert_eq!(planner.get_runner_state(2).unwrap().attempts, 2);
        // Fresh bytes only.
        assert!(planner.allocate_chunk(400..1000, Some(3)));
        assert_eq!(planner.get_runner_state(3).unwrap().attempts, 0);
        assert_eq!(
            planner.cooldowns(),
            vec![
                (150..200, cooldown(200, 2, end)),
                (300..400, cooldown(400, 2, end))
            ]
        );
    }

    /// Adjacent failed ranges merge into one, with the larger attempts and
    /// the later end of their cooldowns.
    #[test]
    fn test_adjacent_cooldowns_merge() {
        let now = Instant::now();
        let mut planner = ChunkPlanner::new(1000);
        assert!(planner.allocate_chunk(100..200, Some(1)));
        // Runner 2 counts on from two failed attempts.
        planner.cool_down(200..300, 2, 2, now);
        assert!(planner.allocate_chunk(200..300, Some(2)));
        fail(&mut planner, 1, now, 3 * SECOND);
        fail(&mut planner, 2, now, SECOND);
        planner.cool_down(400..500, 1, 1, now + SECOND);
        assert_eq!(
            planner.cooldowns(),
            vec![
                (100..300, cooldown(300, 3, now + 3 * SECOND)),
                (400..500, cooldown(500, 1, now + SECOND))
            ]
        );
        // Filling the gap merges all three.
        planner.cool_down(300..400, 1, 1, now);
        assert_eq!(
            planner.cooldowns(),
            vec![(100..500, cooldown(500, 3, now + 3 * SECOND))]
        );
        assert_eq!(
            planner.get_eligible_ranges(now + 2 * SECOND),
            vec![0..100, 500..1000]
        );
        assert_eq!(planner.get_eligible_ranges(now + 3 * SECOND), vec![0..1000]);
        assert!(planner.allocate_chunk(100..500, Some(3)));
        assert_eq!(planner.get_runner_state(3).unwrap().attempts, 3);
    }

    /// A split hands its new range over without attempts; the freed bytes
    /// next to a cooldown stay fresh.
    #[test]
    fn test_split_range_starts_without_attempts() {
        let now = Instant::now();
        let mut planner = ChunkPlanner::new(1000);
        planner.cool_down(0..500, 2, 2, now);
        assert!(planner.allocate_chunk(0..500, Some(1)));
        assert!(planner.allocate_chunk(500..1000, Some(2)));
        planner.mark_running(1).unwrap();
        fail(&mut planner, 2, now, SECOND);

        // As the loop splits: shrink runner 1, then hand the tail over.
        planner.resize_runner_state(1, 250).unwrap();
        assert_eq!(planner.get_eligible_ranges(now), vec![250..500]);
        assert!(planner.allocate_chunk(250..500, Some(3)));
        assert_eq!(planner.get_runner_state(3).unwrap().attempts, 0);
        assert_eq!(planner.get_runner_state(1).unwrap().attempts, 2);
        assert_eq!(
            planner.cooldowns(),
            vec![(500..1000, cooldown(1000, 1, now + SECOND))]
        );

        planner.split_chunk(1, 125, 4).unwrap();
        assert_eq!(planner.get_runner_state(4).unwrap().attempts, 0);
        assert_eq!(planner.get_runner_state(1).unwrap().attempts, 2);
    }

    /// The charged failures follow the attempts' rules on their own: the
    /// larger of each when merged or claimed, none for a split.
    #[test]
    fn test_charged_failures_are_carried_like_attempts() {
        let now = Instant::now();
        let mut planner = ChunkPlanner::new(1000);
        planner.cool_down(0..100, 3, 1, now);
        planner.cool_down(100..200, 1, 1, now);
        planner.cool_down(150..300, 2, 2, now);
        assert_eq!(
            planner.cooldowns(),
            vec![(
                0..300,
                Cooldown {
                    end: 300,
                    attempts: 3,
                    charged: 2,
                    not_before: now,
                }
            )]
        );

        assert!(planner.allocate_chunk(50..250, Some(1)));
        let state = planner.get_runner_state(1).unwrap();
        assert_eq!((state.attempts, state.charged), (3, 2));
        // The planner reports the charges before the failure.
        planner.mark_running(1).unwrap();
        planner.update_progress(1, 50).unwrap();
        assert_eq!(
            planner.mark_failed(1).unwrap(),
            Released {
                range: 100..250,
                attempts: 4,
                charged: 2,
                received: 50,
            }
        );

        // What is left of the cooldown after 250 carries both counts.
        assert!(planner.allocate_chunk(250..1000, Some(2)));
        let state = planner.get_runner_state(2).unwrap();
        assert_eq!((state.attempts, state.charged), (3, 2));
        planner.mark_running(2).unwrap();
        planner.split_chunk(2, 500, 3).unwrap();
        let state = planner.get_runner_state(3).unwrap();
        assert_eq!((state.attempts, state.charged), (0, 0));
    }

    #[test]
    fn test_guard_drop_restores_cooldowns() {
        let now = Instant::now();
        let mut planner = ChunkPlanner::new(1000);
        planner.cool_down(0..500, 1, 1, now + SECOND);
        {
            let mut guard = PlannerGuard::new(&mut planner);
            assert!(guard.planner().allocate_chunk(0..1000, Some(1)));
        }
        assert_eq!(
            planner.cooldowns(),
            vec![(0..500, cooldown(500, 1, now + SECOND))]
        );
    }

    /// A hedge joins a chunk as its partner (design §9.6): it counts as an
    /// active runner, and the chunk is neither resized nor split. When the
    /// original leaves, the hedge takes the chunk over with its counts, and
    /// the bytes each side delivered itself are told apart by the fork.
    #[test]
    fn test_a_hedge_joins_the_chunk_of_its_original() {
        let now = Instant::now();
        let mut planner = ChunkPlanner::new(1000);
        planner.cool_down(0..1000, 2, 1, now);
        assert!(planner.allocate_chunk(0..600, Some(1)));
        assert!(planner.allocate_chunk(600..1000, Some(2)));
        planner.mark_running(1).unwrap();
        planner.mark_running(2).unwrap();
        planner.update_progress(1, 100).unwrap();
        let generation = planner.get_runner_state(1).unwrap().generation;

        assert_eq!(planner.fork_hedge(1, 3).unwrap(), 100..600);
        // Once per attempt, by a runner without a chunk.
        assert!(planner.fork_hedge(1, 4).is_err());
        assert!(planner.fork_hedge(2, 3).is_err());
        assert!(planner.fork_hedge(2, 1).is_err());
        assert_eq!(planner.get_active_runners_count(), 3);
        assert!(planner.get_runner_state(3).is_none());
        assert_eq!(planner.partner_chunk(3).unwrap().runner_id, Some(1));
        assert!(planner.resize_runner_state(1, 100).is_err());
        assert!(planner.split_chunk(1, 300, 5).is_err());
        assert_eq!(planner.get_runner_state(1).unwrap().allocated, 0..600);
        assert_eq!(
            planner.largest_running_chunk(|_| false),
            Some((2, 600..1000))
        );

        // The original leaves; the hedge, still connecting, takes over.
        assert_eq!(planner.hand_over(1).unwrap(), 3);
        assert!(planner.partner_chunk(3).is_none());
        assert!(planner.get_runner_state(1).is_none());
        let state = planner.get_runner_state(3).unwrap();
        assert_eq!(
            (
                state.status.clone(),
                state.downloaded.clone(),
                state.attempts,
                state.charged,
                state.generation,
            ),
            (ChunkStatus::Pending, 0..100, 2, 1, generation)
        );
        assert_eq!(planner.owner_bytes(3), Some(0));
        planner.record_side_failure(
            3,
            SideFailure {
                restart: true,
                charged: false,
            },
        );
        assert!(planner.get_runner_state(3).unwrap().is_undecided());
        planner.mark_running(3).unwrap();
        planner.update_progress(3, 50).unwrap();
        planner.decide(3);
        let hedge = planner.get_runner_state(3).unwrap().hedge.clone().unwrap();
        assert_eq!(
            (hedge.fork, hedge.owner_side, hedge.undecided, hedge.partner),
            (100, Side::Hedge, false, None)
        );
        assert!(hedge.first.is_some(), "kept until the attempt ends");
        assert_eq!(planner.owner_bytes(3), Some(50));
        assert_eq!(planner.get_active_runners_count(), 2);

        // The attempt ends with its hedge.
        let released = planner.mark_failed(3).unwrap();
        assert_eq!((released.range, released.received), (150..600, 150));
        assert!(planner.chunks[&0].hedge.is_none());

        // A hedge that leaves gives the chunk back to the original.
        assert_eq!(planner.fork_hedge(2, 4).unwrap(), 600..1000);
        assert_eq!(planner.drop_partner(4).unwrap(), 2);
        assert_eq!(planner.get_active_runners_count(), 1);
        assert_eq!(planner.get_runner_state(2).unwrap().runner_id, Some(2));
        assert_eq!(planner.owner_bytes(2), Some(0));
    }
}
