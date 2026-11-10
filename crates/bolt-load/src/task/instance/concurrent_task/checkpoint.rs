//! Checkpoints: the complete blocks of the partial file, with their CRCs,
//! handed to a receiver once their data is synced (design §11.5.5,
//! §11.5.6).
//!
//! The receiver is crate-internal: the state file of C33. Without one, no
//! checkpoint is scheduled, no CRC is computed and nothing is synced.

mod blocks;
#[cfg(test)]
pub(super) mod tests;

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, io,
    num::NonZeroU64,
    ops::Range,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use async_waitgroup::WaitGroup;
use bolt_load_utils::telemetry::*;
use futures::FutureExt;

pub(crate) use self::blocks::{BlockCrc, BlockCrcs};
use super::{
    chunk_planner::WrittenSet,
    file_writer::{CrcSegment, FileRangeWriter, FileWriter},
};
use crate::task::{TaskOptions, instance::drained_file::Drained};

/// The size of a block, the unit a checkpoint records: 1 MiB (design
/// §11.5.6, D19).
pub(crate) const BLOCK_SIZE: NonZeroU64 = NonZeroU64::new(1 << 20).unwrap();

/// A complete block a checkpoint records: all of its bytes are written and
/// synced, and `crc` is the CRC32C of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CheckpointBlock {
    pub(crate) index: u64,
    /// `block_size`, or less for the last block.
    pub(crate) len: u32,
    pub(crate) crc: u32,
}

/// Where checkpoints go: the state file (design §11.5.5).
///
/// It is called on a blocking thread, one call at a time.
pub(crate) trait CheckpointReceiver: Send + 'static {
    /// Record `blocks`, whose data the writer has synced: append them after
    /// the end of the last checkpoint, and sync. On success they are
    /// durable.
    ///
    /// # Errors
    ///
    /// The blocks could not be recorded; [`Self::roll_back`] follows.
    fn record(&mut self, blocks: &[CheckpointBlock]) -> io::Result<()>;

    /// Undo a failed checkpoint, after the writer's sync or [`Self::record`]
    /// failed: cut what it may have appended, back to the end of the last
    /// checkpoint, and sync. Its blocks count as not recorded, and the next
    /// checkpoint records them again.
    ///
    /// # Errors
    ///
    /// The cut or its sync failed: the task takes no more checkpoints.
    fn roll_back(&mut self) -> io::Result<()>;
}

/// Reads blocks of the partial file, for the blocks whose CRC is read back.
///
/// It is called on a blocking thread, after the writer's sync.
pub(crate) trait BlockReader: Send + Sync + 'static {
    /// The bytes of `range`.
    ///
    /// # Errors
    ///
    /// They could not be read.
    fn read(&self, range: Range<u64>) -> io::Result<Vec<u8>>;
}

/// The partial file, read by path.
pub(crate) struct FileBlocks(pub(crate) PathBuf);

impl BlockReader for FileBlocks {
    fn read(&self, range: Range<u64>) -> io::Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};

        let mut file = fs_err::File::open(&self.0)?;
        file.seek(SeekFrom::Start(range.start))?;
        let mut data = vec![0; (range.end - range.start) as usize];
        file.read_exact(&mut data)?;
        Ok(data)
    }
}

/// When checkpoints are taken: after this many bytes were written since the
/// last one started, or this long after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Schedule {
    pub(crate) block_size: NonZeroU64,
    pub(crate) bytes: NonZeroU64,
    pub(crate) interval: Duration,
}

impl Schedule {
    /// The schedule of `options`, with blocks of [`BLOCK_SIZE`].
    pub(crate) fn of(options: &TaskOptions) -> Self {
        Self {
            block_size: BLOCK_SIZE,
            bytes: options.checkpoint_bytes,
            interval: options.checkpoint_interval,
        }
    }
}

/// How a checkpoint ended.
pub(super) enum Outcome {
    /// The blocks are recorded: durable.
    Recorded(Vec<CheckpointBlock>),
    /// The sync, a read back or the record failed and was rolled back: the
    /// blocks, with their CRCs as they were taken, are still to record.
    Failed(Vec<(u64, BlockCrc)>, io::Error),
    /// The rollback failed too: no more checkpoints.
    Stopped(io::Error, io::Error),
}

impl fmt::Debug for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Recorded(blocks) => f.debug_tuple("Recorded").field(&blocks.len()).finish(),
            Self::Failed(blocks, error) => f
                .debug_tuple("Failed")
                .field(&blocks.len())
                .field(error)
                .finish(),
            Self::Stopped(error, rollback) => f
                .debug_tuple("Stopped")
                .field(error)
                .field(rollback)
                .finish(),
        }
    }
}

type Running = Pin<Box<dyn Future<Output = (Box<dyn CheckpointReceiver>, Outcome)> + Send>>;

/// How long each sync of the writer took, for the benchmark.
#[cfg(test)]
pub(super) type SyncTimes = Arc<std::sync::Mutex<Vec<Duration>>>;

/// The checkpoints of one concurrent attempt: serial, at most one at a time
/// (design §11.5.5).
pub(super) struct Checkpoints {
    schedule: Schedule,
    total: u64,
    /// `None` while a checkpoint runs with it, and once checkpoints stopped.
    receiver: Option<Box<dyn CheckpointReceiver>>,
    writer: FileWriter,
    reader: Arc<dyn BlockReader>,
    /// The CRCs of the blocks not complete yet.
    crcs: BlockCrcs,
    /// The blocks written to since they were last looked at, not complete
    /// then.
    touched: BTreeSet<u64>,
    /// The complete blocks no checkpoint recorded yet.
    ready: BTreeMap<u64, BlockCrc>,
    /// The recorded blocks, by index.
    durable: WrittenSet,
    running: Option<Running>,
    /// The bytes written since the last checkpoint started.
    unrecorded: u64,
    /// When the last checkpoint started, or the attempt.
    since: Instant,
    stopped: bool,
    /// Held by the blocking work of a running checkpoint until it ends, so
    /// that teardown and the panic handler wait for it (design §11.5.4).
    drain: WaitGroup,
    #[cfg(test)]
    pub(super) sync_times: SyncTimes,
}

impl Checkpoints {
    /// Checkpoints of a file of `total` bytes written through `writer`,
    /// handed to `receiver`; the work of a running checkpoint holds a clone
    /// of `drain`.
    pub(super) fn new(
        total: u64,
        schedule: Schedule,
        receiver: Box<dyn CheckpointReceiver>,
        writer: FileWriter,
        reader: Arc<dyn BlockReader>,
        drain: WaitGroup,
        now: Instant,
    ) -> Self {
        Self {
            schedule,
            total,
            receiver: Some(receiver),
            writer,
            reader,
            crcs: BlockCrcs::new(schedule.block_size),
            touched: BTreeSet::new(),
            ready: BTreeMap::new(),
            durable: WrittenSet::default(),
            running: None,
            unrecorded: 0,
            since: now,
            stopped: false,
            drain,
            #[cfg(test)]
            sync_times: SyncTimes::default(),
        }
    }

    /// Whether a failed rollback stopped the checkpoints.
    pub(super) fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// What is kept for the checkpoints to come: the segments, the blocks
    /// touched and the blocks ready to record.
    #[cfg(test)]
    pub(super) fn kept(&self) -> (usize, usize, usize) {
        (
            self.crcs.segment_count(),
            self.touched.len(),
            self.ready.len(),
        )
    }

    pub(super) fn block_size(&self) -> NonZeroU64 {
        self.schedule.block_size
    }

    /// The blocks recorded so far, by index.
    #[cfg_attr(not(test), expect(dead_code, reason = "C33 reads it"))]
    pub(super) fn durable(&self) -> &WrittenSet {
        &self.durable
    }

    fn block_range(&self, index: u64) -> Range<u64> {
        let size = self.schedule.block_size.get();
        index * size..((index + 1) * size).min(self.total)
    }

    /// A successful write of `range`, with the CRCs of its bytes; ignored
    /// once the checkpoints stopped.
    pub(super) fn note_written(&mut self, range: Range<u64>, crcs: &[CrcSegment]) {
        if self.stopped || range.is_empty() {
            return;
        }
        for &segment in crcs {
            self.crcs.add(segment);
        }
        let size = self.schedule.block_size.get();
        self.touched
            .extend(range.start / size..=(range.end - 1) / size);
        self.unrecorded += range.end - range.start;
    }

    /// Move the touched blocks that `written` covers to the ready ones.
    fn collect_complete(&mut self, written: &WrittenSet) {
        let complete: Vec<u64> = self
            .touched
            .iter()
            .copied()
            .filter(|&index| written.contains(&self.block_range(index)))
            .collect();
        for index in complete {
            self.touched.remove(&index);
            let len = {
                let range = self.block_range(index);
                range.end - range.start
            };
            let crc = self.crcs.take_complete(index, len);
            self.ready.insert(index, crc);
        }
    }

    /// Start a checkpoint at `now` if one is due and none runs: enough
    /// bytes were written since the last, or enough time passed, and there
    /// are complete blocks to record.
    pub(super) fn maybe_start(&mut self, written: &WrittenSet, now: Instant) {
        if self.stopped || self.running.is_some() {
            return;
        }
        let due = self.unrecorded >= self.schedule.bytes.get()
            || now.saturating_duration_since(self.since) >= self.schedule.interval;
        if !due {
            return;
        }
        self.collect_complete(written);
        self.start(now);
    }

    /// Start a checkpoint of the ready blocks, if there are any.
    fn start(&mut self, now: Instant) {
        if self.ready.is_empty() {
            return;
        }
        let Some(receiver) = self.receiver.take() else {
            return;
        };
        let blocks: Vec<(u64, BlockCrc)> = std::mem::take(&mut self.ready).into_iter().collect();
        let ranges = blocks
            .iter()
            .map(|&(index, crc)| (index, self.block_range(index), crc))
            .collect();
        let held = Drained::new(
            Work {
                receiver,
                reader: self.reader.clone(),
            },
            Some(self.drain.clone()),
        );
        self.running = Some(Box::pin(run(
            self.writer.clone(),
            held,
            ranges,
            #[cfg(test)]
            self.sync_times.clone(),
        )));
        self.unrecorded = 0;
        self.since = now;
    }

    /// The outcome of the running checkpoint, once it ended.
    pub(super) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Outcome> {
        let Some(running) = self.running.as_mut() else {
            return Poll::Pending;
        };
        let (receiver, outcome) = std::task::ready!(running.poll_unpin(cx));
        self.running = None;
        self.receiver = Some(receiver);
        Poll::Ready(outcome)
    }

    /// Account for the `outcome` of a checkpoint.
    pub(super) fn apply(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Recorded(blocks) => {
                for block in blocks {
                    self.durable.insert(block.index..block.index + 1);
                }
            }
            Outcome::Failed(blocks, error) => {
                warn!(
                    "checkpoint of {} blocks failed and was rolled back, retried at the next: \
                     {error}",
                    blocks.len()
                );
                self.ready.extend(blocks);
            }
            Outcome::Stopped(error, rollback) => {
                warn!(
                    "checkpoint failed ({error}) and its rollback failed ({rollback}): no more \
                     checkpoints"
                );
                self.stop();
            }
        }
    }

    fn stop(&mut self) {
        self.stopped = true;
        self.receiver = None;
        self.ready.clear();
        self.touched.clear();
        self.crcs = BlockCrcs::new(self.schedule.block_size);
    }

    /// Wait for the running checkpoint, if any, and account for it.
    pub(super) async fn settle(&mut self) {
        if self.running.is_some() {
            let outcome = std::future::poll_fn(|cx| self.poll(cx)).await;
            self.apply(outcome);
        }
    }

    /// The checkpoint at teardown: wait for the running one, then record
    /// every complete block left (design §11.5.9).
    pub(super) async fn finish(mut self, written: &WrittenSet) {
        self.settle().await;
        if self.stopped {
            return;
        }
        self.collect_complete(written);
        self.start(Instant::now());
        if self.running.is_some() {
            let outcome = std::future::poll_fn(|cx| self.poll(cx)).await;
            self.apply(outcome);
        }
    }
}

/// What the blocking work of a checkpoint uses, which it releases before
/// the drain group.
pub(super) struct Work {
    pub(super) receiver: Box<dyn CheckpointReceiver>,
    pub(super) reader: Arc<dyn BlockReader>,
}

/// Sync the writer, read back the CRCs of the blocks that need it, and
/// record the blocks; roll back if any step failed.
///
/// `held` keeps the receiver and the reader with a clone of the attempt's
/// drain group, released after them on every path: when the work returns
/// and its result is read or dropped unread, when it panics, and when it,
/// or this future, is dropped before it ran. Dropping this future does not
/// stop the work; teardown and the panic handler wait for the drain group
/// before they touch the files (design §11.5.4, the pattern of C31's
/// [`Drained`]).
pub(super) async fn run(
    writer: FileWriter,
    held: Drained<Work>,
    blocks: Vec<(u64, Range<u64>, BlockCrc)>,
    #[cfg(test)] sync_times: SyncTimes,
) -> (Box<dyn CheckpointReceiver>, Outcome) {
    #[cfg(test)]
    let started = Instant::now();
    let synced = writer.sync().await.map_err(|error| error.into_io_error());
    #[cfg(test)]
    sync_times.lock().unwrap().push(started.elapsed());
    drop(writer);
    let (held, outcome) = blocking::unblock(move || {
        // Moved in whole, never field by field.
        let mut held = held;
        let work = &mut held.value;
        let recorded = synced.and_then(|()| {
            let records = resolve(&blocks, &*work.reader)?;
            work.receiver.record(&records)?;
            Ok(records)
        });
        let outcome = match recorded {
            Ok(records) => Outcome::Recorded(records),
            Err(error) => match work.receiver.roll_back() {
                Ok(()) => Outcome::Failed(
                    blocks
                        .into_iter()
                        .map(|(index, _, crc)| (index, crc))
                        .collect(),
                    error,
                ),
                Err(rollback) => Outcome::Stopped(error, rollback),
            },
        };
        (held, outcome)
    })
    .await;
    // The reader goes before the drain; the receiver goes back to the
    // checkpoints, which hold the drain group themselves.
    let (Work { receiver, reader }, drain) = held.into_parts();
    drop(reader);
    drop(drain);
    (receiver, outcome)
}

/// The records of `blocks`: their CRCs, read back from the synced file for
/// the blocks that need it.
fn resolve(
    blocks: &[(u64, Range<u64>, BlockCrc)],
    reader: &dyn BlockReader,
) -> io::Result<Vec<CheckpointBlock>> {
    blocks
        .iter()
        .map(|(index, range, crc)| {
            let crc = match *crc {
                BlockCrc::Known(crc) => crc,
                BlockCrc::ReadBack => crc32c::crc32c(&reader.read(range.clone())?),
            };
            Ok(CheckpointBlock {
                index: *index,
                len: (range.end - range.start) as u32,
                crc,
            })
        })
        .collect()
}
