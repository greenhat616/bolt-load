//! Checkpoints: scheduling, serial runs, rollback, read back, teardown
//! (design §11.5.5, §11.5.6).
#![expect(clippy::single_range_in_vec_init, reason = "lists holding one range")]

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_waitgroup::WaitGroup;
use bytes::Bytes;
use pretty_assertions::assert_eq;

use super::*;
use crate::task::instance::concurrent_task::file_writer::{
    FileRangeWriter, Memory, PendingWriter, SyncProbe,
};

/// What a [`RecordingReceiver`] was asked to do, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Call {
    /// Record these blocks, by index.
    Record(Vec<u64>),
    RollBack,
}

#[derive(Default)]
struct ReceiverState {
    calls: Vec<Call>,
    /// The blocks recorded, in order.
    recorded: Vec<CheckpointBlock>,
    /// Blocks it was asked to record whose data was not synced.
    unsynced: Vec<u64>,
    fail_records: usize,
    fail_rollbacks: usize,
}

/// What a [`RecordingReceiver`] saw, shared with the test.
#[derive(Clone, Default)]
pub(crate) struct Received(Arc<Mutex<ReceiverState>>);

impl Received {
    pub(crate) fn calls(&self) -> Vec<Call> {
        self.0.lock().unwrap().calls.clone()
    }

    pub(crate) fn recorded(&self) -> Vec<CheckpointBlock> {
        self.0.lock().unwrap().recorded.clone()
    }

    /// The blocks it was asked to record before their data was synced.
    pub(crate) fn unsynced(&self) -> Vec<u64> {
        self.0.lock().unwrap().unsynced.clone()
    }

    /// Fail the next `n` records.
    pub(crate) fn fail_records(&self, n: usize) {
        self.0.lock().unwrap().fail_records = n;
    }

    /// Fail the next `n` rollbacks.
    pub(crate) fn fail_rollbacks(&self, n: usize) {
        self.0.lock().unwrap().fail_rollbacks = n;
    }
}

/// A receiver that records what it is asked to do, checks that the blocks
/// it records were synced first, and fails as the test says.
pub(crate) struct RecordingReceiver {
    received: Received,
    syncs: SyncProbe,
    block_size: u64,
    total: u64,
    gate: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
}

/// Holds the first record of a gated [`RecordingReceiver`]: `entered`
/// reports that it started, and it returns once `release` sends or drops.
pub(crate) struct RecordGate {
    pub(crate) entered: std::sync::mpsc::Receiver<()>,
    pub(crate) release: std::sync::mpsc::Sender<()>,
}

impl RecordingReceiver {
    pub(crate) fn new(syncs: SyncProbe, block_size: u64, total: u64) -> (Box<Self>, Received) {
        let received = Received::default();
        let receiver = Box::new(Self {
            received: received.clone(),
            syncs,
            block_size,
            total,
            gate: None,
        });
        (receiver, received)
    }

    /// Hold its first record at a gate the test opens.
    pub(crate) fn gated(mut self: Box<Self>) -> (Box<Self>, RecordGate) {
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        self.gate = Some((entered_tx, release_rx));
        (self, RecordGate { entered, release })
    }
}

impl CheckpointReceiver for RecordingReceiver {
    fn record(&mut self, blocks: &[CheckpointBlock]) -> io::Result<()> {
        if let Some((entered, release)) = self.gate.take() {
            entered.send(()).unwrap();
            // Released by the test, or by its end; bounded all the same.
            let _ = release.recv_timeout(Duration::from_secs(30));
        }
        let mut state = self.received.0.lock().unwrap();
        state.calls.push(Call::Record(
            blocks.iter().map(|block| block.index).collect(),
        ));
        for block in blocks {
            let start = block.index * self.block_size;
            let range = start..(start + self.block_size).min(self.total);
            if !self.syncs.is_synced(&range) {
                state.unsynced.push(block.index);
            }
        }
        if state.fail_records > 0 {
            state.fail_records -= 1;
            return Err(io::Error::other("injected record failure"));
        }
        state.recorded.extend_from_slice(blocks);
        Ok(())
    }

    fn roll_back(&mut self) -> io::Result<()> {
        let mut state = self.received.0.lock().unwrap();
        state.calls.push(Call::RollBack);
        if state.fail_rollbacks > 0 {
            state.fail_rollbacks -= 1;
            return Err(io::Error::other("injected rollback failure"));
        }
        Ok(())
    }
}

/// The ranges a [`MemoryBlocks`] read, each with whether its data was
/// synced then.
pub(crate) type Reads = Arc<Mutex<Vec<(Range<u64>, bool)>>>;

/// Reads blocks from a [`Memory`], notes the reads, and fails as many as
/// the test says.
pub(crate) struct MemoryBlocks {
    memory: Memory,
    syncs: SyncProbe,
    pub(crate) reads: Reads,
    pub(crate) fail: Arc<Mutex<usize>>,
}

impl MemoryBlocks {
    pub(crate) fn new(memory: Memory, syncs: SyncProbe) -> Self {
        Self {
            memory,
            syncs,
            reads: Reads::default(),
            fail: Arc::default(),
        }
    }
}

impl BlockReader for MemoryBlocks {
    fn read(&self, range: Range<u64>) -> io::Result<Vec<u8>> {
        let synced = self.syncs.is_synced(&range);
        self.reads.lock().unwrap().push((range.clone(), synced));
        let mut fail = self.fail.lock().unwrap();
        if *fail > 0 {
            *fail -= 1;
            return Err(io::Error::other("injected read failure"));
        }
        Ok(self.memory.lock().unwrap()[range.start as usize..range.end as usize].to_vec())
    }
}

/// Bytes that differ from offset to offset, and with `salt`.
pub(crate) fn pattern(offset: u64, len: u64, salt: u8) -> Bytes {
    (offset..offset + len)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8 ^ salt)
        .collect::<Vec<u8>>()
        .into()
}

/// Every recorded block has the CRC of its bytes in `memory`, and is
/// recorded once.
pub(crate) fn assert_records_match(recorded: &[CheckpointBlock], memory: &Memory, block_size: u64) {
    let memory = memory.lock().unwrap();
    let total = memory.len() as u64;
    let mut seen = BTreeSet::new();
    for block in recorded {
        assert!(
            seen.insert(block.index),
            "block {} recorded twice",
            block.index
        );
        let start = block.index * block_size;
        let end = (start + block_size).min(total);
        assert_eq!(u64::from(block.len), end - start, "{block:?}");
        assert_eq!(
            block.crc,
            crc32c::crc32c(&memory[start as usize..end as usize]),
            "block {}",
            block.index
        );
    }
}

const BLOCK: u64 = 16;
const TOTAL: u64 = 100;

/// Checkpoints of a file of `TOTAL` bytes in blocks of `BLOCK`, taken
/// after `bytes` or `interval`, through a pending writer that keeps the
/// bytes in memory.
struct Setup {
    checkpoints: Checkpoints,
    writer: PendingWriter<FileWriter>,
    memory: Memory,
    syncs: SyncProbe,
    received: Received,
    reads: Reads,
    fail_reads: Arc<Mutex<usize>>,
    drain: WaitGroup,
    gate: Option<RecordGate>,
    written: WrittenSet,
    t0: Instant,
}

impl Setup {
    fn new(bytes: u64, interval: Duration) -> Self {
        Self::with(BLOCK, TOTAL, bytes, interval)
    }

    fn with(block: u64, total: u64, bytes: u64, interval: Duration) -> Self {
        Self::build(block, total, bytes, interval, false)
    }

    /// Checkpoints after every write, whose first record waits at a gate.
    fn gated() -> Self {
        Self::build(BLOCK, TOTAL, 1, Duration::from_secs(3600), true)
    }

    fn build(block: u64, total: u64, bytes: u64, interval: Duration, gated: bool) -> Self {
        let (file_writer, _, memory, syncs) = FileWriter::in_memory(total, Vec::new());
        let (receiver, received) = RecordingReceiver::new(syncs.clone(), block, total);
        let (receiver, gate) = if gated {
            let (receiver, gate) = receiver.gated();
            (receiver, Some(gate))
        } else {
            (receiver, None)
        };
        let reader = MemoryBlocks::new(memory.clone(), syncs.clone());
        let reads = reader.reads.clone();
        let fail_reads = reader.fail.clone();
        let drain = WaitGroup::new();
        let t0 = Instant::now();
        let schedule = Schedule {
            block_size: NonZeroU64::new(block).unwrap(),
            bytes: NonZeroU64::new(bytes).unwrap(),
            interval,
            min_spacing: Duration::ZERO,
        };
        let checkpoints = Checkpoints::new(
            total,
            schedule,
            receiver,
            file_writer.clone(),
            Arc::new(reader),
            drain.clone(),
            t0,
        );
        let mut writer = PendingWriter::new(
            Arc::new(file_writer),
            crate::runtime::ThreadedRuntimeImpl::new_tokio_rt(),
            4,
        );
        writer.checksum(schedule.block_size);
        Self {
            checkpoints,
            writer,
            memory,
            syncs,
            received,
            reads,
            fail_reads,
            drain,
            gate,
            written: WrittenSet::default(),
            t0,
        }
    }

    /// Write `range`, wait for it, and hand it to the checkpoints.
    async fn write(&mut self, range: Range<u64>) {
        let data = pattern(range.start, range.end - range.start, 0x5a);
        self.writer.write_range(range, data).unwrap();
        for completion in self.writer.flush().await {
            completion.result.unwrap();
            self.checkpoints
                .note_written(completion.range.clone(), &completion.crcs);
            self.written.insert(completion.range);
        }
    }

    /// Start a checkpoint at `t0 + after` if one is due; `true` if one runs.
    fn maybe_start(&mut self, after: Duration) -> bool {
        self.checkpoints.maybe_start(&self.written, self.t0 + after);
        self.checkpoints.running.is_some()
    }

    /// Wait for the running checkpoint and account for it.
    async fn settle(&mut self) {
        let outcome = std::future::poll_fn(|cx| self.checkpoints.poll(cx)).await;
        self.checkpoints.apply(outcome);
    }

    fn durable(&self) -> Vec<Range<u64>> {
        self.checkpoints.durable().ranges().to_vec()
    }
}

/// A checkpoint is due once enough bytes were written since the last one
/// started, and then records every complete block, synced first.
#[tokio::test]
async fn test_a_checkpoint_after_enough_bytes() {
    let mut s = Setup::new(2 * BLOCK, Duration::from_secs(3600));
    s.write(0..BLOCK).await;
    assert!(!s.maybe_start(Duration::ZERO), "16 bytes are not due");
    s.write(BLOCK..2 * BLOCK + 4).await;
    assert!(s.maybe_start(Duration::ZERO));
    s.settle().await;
    assert_eq!(s.received.calls(), [Call::Record(vec![0, 1])]);
    assert_eq!(s.durable(), [0..2]);
    assert_eq!(s.syncs.count(), 1);
    assert_eq!(s.received.unsynced(), Vec::<u64>::new());
    assert_records_match(&s.received.recorded(), &s.memory, BLOCK);
    // The count starts over: 4 bytes of block 2 are not due.
    s.write(2 * BLOCK + 4..3 * BLOCK).await;
    assert!(!s.maybe_start(Duration::ZERO));
}

/// A checkpoint is due once enough time passed since the last one started,
/// however few bytes were written; not before.
#[tokio::test]
async fn test_a_checkpoint_after_enough_time() {
    let mut s = Setup::new(1 << 30, Duration::from_secs(2));
    s.write(0..BLOCK).await;
    assert!(!s.maybe_start(Duration::from_millis(1999)));
    assert!(s.maybe_start(Duration::from_secs(2)));
    s.settle().await;
    assert_eq!(s.durable(), [0..1]);
    s.write(BLOCK..2 * BLOCK).await;
    assert!(
        !s.maybe_start(Duration::from_millis(3999)),
        "2 s after the last"
    );
    assert!(s.maybe_start(Duration::from_secs(4)));
    s.settle().await;
    assert_eq!(s.durable(), [0..2]);
}

/// A due checkpoint with no complete block records nothing and syncs
/// nothing.
#[tokio::test]
async fn test_no_checkpoint_without_a_complete_block() {
    let mut s = Setup::new(1, Duration::from_secs(1));
    s.write(0..BLOCK - 1).await;
    s.write(BLOCK + 1..2 * BLOCK).await;
    assert!(!s.maybe_start(Duration::from_secs(5)));
    assert_eq!(s.syncs.count(), 0);
    assert_eq!(s.received.calls(), []);
}

/// One checkpoint at a time: while one runs, a due one waits, and records
/// only the blocks completed since, after it.
#[tokio::test]
async fn test_checkpoints_are_serial() {
    let mut s = Setup::new(1, Duration::from_secs(3600));
    s.write(0..BLOCK).await;
    assert!(s.maybe_start(Duration::ZERO));
    s.write(BLOCK..2 * BLOCK).await;
    s.checkpoints
        .maybe_start(&s.written, s.t0 + Duration::from_secs(1));
    s.settle().await;
    assert_eq!(s.received.calls(), [Call::Record(vec![0])]);
    assert!(s.maybe_start(Duration::from_secs(1)));
    s.settle().await;
    assert_eq!(
        s.received.calls(),
        [Call::Record(vec![0]), Call::Record(vec![1])]
    );
    assert_eq!(s.durable(), [0..2]);
}

/// A failed sync rolls back before anything is recorded; its blocks stay
/// unrecorded, and the next checkpoint records them.
#[tokio::test]
async fn test_a_failed_sync_rolls_back_and_the_next_retries() {
    let mut s = Setup::new(1, Duration::from_secs(3600));
    s.write(0..2 * BLOCK).await;
    s.syncs.fail_next(1);
    assert!(s.maybe_start(Duration::ZERO));
    s.settle().await;
    assert_eq!(s.received.calls(), [Call::RollBack]);
    assert_eq!(s.durable(), []);

    s.write(2 * BLOCK..3 * BLOCK).await;
    assert!(s.maybe_start(Duration::ZERO));
    s.settle().await;
    assert_eq!(
        s.received.calls(),
        [Call::RollBack, Call::Record(vec![0, 1, 2])]
    );
    assert_eq!(s.durable(), [0..3]);
    assert_eq!(s.received.unsynced(), Vec::<u64>::new());
    assert_records_match(&s.received.recorded(), &s.memory, BLOCK);
}

/// A failed record rolls back; its blocks stay unrecorded, and the next
/// checkpoint records them again, with the same CRCs.
#[tokio::test]
async fn test_a_failed_record_rolls_back_and_the_next_retries() {
    let mut s = Setup::new(1, Duration::from_secs(3600));
    s.write(0..BLOCK).await;
    s.received.fail_records(1);
    assert!(s.maybe_start(Duration::ZERO));
    s.settle().await;
    assert_eq!(s.received.calls(), [Call::Record(vec![0]), Call::RollBack]);
    assert_eq!(s.durable(), []);
    // Not due again until the schedule says so, then retried.
    assert!(!s.maybe_start(Duration::ZERO));
    assert!(
        s.maybe_start(Duration::from_secs(3600)),
        "the block is still to record"
    );
    s.settle().await;
    assert_eq!(s.durable(), [0..1]);
    assert_records_match(&s.received.recorded(), &s.memory, BLOCK);
}

/// A failed rollback stops the checkpoints: no more syncs or calls, at
/// teardown neither.
#[tokio::test]
async fn test_a_failed_rollback_stops_the_checkpoints() {
    let mut s = Setup::new(1, Duration::from_secs(3600));
    s.write(0..BLOCK).await;
    s.received.fail_records(1);
    s.received.fail_rollbacks(1);
    assert!(s.maybe_start(Duration::ZERO));
    s.settle().await;
    assert_eq!(s.received.calls(), [Call::Record(vec![0]), Call::RollBack]);
    assert!(s.checkpoints.is_stopped());
    s.write(BLOCK..2 * BLOCK).await;
    s.write(3 * BLOCK..3 * BLOCK + 4).await;
    // Nothing is kept for checkpoints that will not come.
    assert_eq!(s.checkpoints.kept(), (0, 0, 0));
    assert!(!s.maybe_start(Duration::from_secs(10)));
    let syncs = s.syncs.count();
    let Setup {
        checkpoints,
        written,
        received,
        ..
    } = s;
    checkpoints.finish(&written).await;
    assert_eq!(received.calls().len(), 2);
    assert_eq!(syncs, 1);
}

/// A block past its segment cap reads its CRC back from the file, once the
/// data is synced, and records the CRC of its bytes; a block under the cap
/// is never read.
#[tokio::test]
async fn test_a_block_past_the_cap_reads_back_after_the_sync() {
    const WIDE: u64 = 256;
    let mut s = Setup::with(WIDE, 2 * WIDE, 1 << 30, Duration::from_secs(3600));
    // Block 0: 65 one-byte writes with holes between them, then the rest.
    for i in 0..=blocks::MAX_SEGMENTS_PER_BLOCK as u64 {
        s.write(2 * i..2 * i + 1).await;
    }
    assert!(s.checkpoints.crcs.reads_back(0));
    for i in 0..=blocks::MAX_SEGMENTS_PER_BLOCK as u64 {
        s.write(2 * i + 1..2 * i + 2).await;
    }
    s.write(2 * (blocks::MAX_SEGMENTS_PER_BLOCK as u64 + 1)..WIDE)
        .await;
    // Block 1: 64 segments at most, under the cap.
    for i in 0..blocks::MAX_SEGMENTS_PER_BLOCK as u64 {
        s.write(WIDE + 2 * i..WIDE + 2 * i + 1).await;
    }
    for i in 0..blocks::MAX_SEGMENTS_PER_BLOCK as u64 {
        s.write(WIDE + 2 * i + 1..WIDE + 2 * i + 2).await;
    }
    s.write(WIDE + 2 * blocks::MAX_SEGMENTS_PER_BLOCK as u64..2 * WIDE)
        .await;
    assert!(!s.checkpoints.crcs.reads_back(1));
    let Setup {
        checkpoints,
        written,
        received,
        memory,
        reads,
        syncs,
        ..
    } = s;
    checkpoints.finish(&written).await;
    assert_eq!(received.calls(), [Call::Record(vec![0, 1])]);
    assert_eq!(syncs.count(), 1);
    assert_eq!(
        *reads.lock().unwrap(),
        [(0..WIDE, true)],
        "read once synced"
    );
    assert_records_match(&received.recorded(), &memory, WIDE);
}

/// A failed read back is rolled back like a failed sync: the block stays to
/// record and the next checkpoint records it; when the rollback fails too,
/// no checkpoint follows.
#[tokio::test]
async fn test_a_failed_read_back_rolls_back() {
    const WIDE: u64 = 256;
    for rollback_fails in [false, true] {
        let mut s = Setup::with(WIDE, WIDE, 1, Duration::from_secs(3600));
        for i in 0..=blocks::MAX_SEGMENTS_PER_BLOCK as u64 {
            s.write(2 * i..2 * i + 1).await;
        }
        assert!(s.checkpoints.crcs.reads_back(0));
        for i in 0..=blocks::MAX_SEGMENTS_PER_BLOCK as u64 {
            s.write(2 * i + 1..2 * i + 2).await;
        }
        *s.fail_reads.lock().unwrap() = 1;
        s.received.fail_rollbacks(usize::from(rollback_fails));
        s.write(2 * (blocks::MAX_SEGMENTS_PER_BLOCK as u64 + 1)..WIDE)
            .await;
        assert!(s.maybe_start(Duration::ZERO));
        s.settle().await;
        assert_eq!(s.received.calls(), [Call::RollBack], "{rollback_fails}");
        assert_eq!(s.durable(), []);
        assert_eq!(s.checkpoints.is_stopped(), rollback_fails);
        let retried = s.maybe_start(Duration::from_secs(3600));
        assert_eq!(retried, !rollback_fails);
        if retried {
            s.settle().await;
            assert_eq!(s.received.calls(), [Call::RollBack, Call::Record(vec![0])]);
            assert_eq!(s.durable(), [0..1]);
            assert_records_match(&s.received.recorded(), &s.memory, WIDE);
        } else {
            assert_eq!(s.checkpoints.kept(), (0, 0, 0));
        }
    }
}

/// The blocking work of a running checkpoint holds the drain group until it
/// ended: dropping the checkpoints, as an unwind does, does not release it
/// while the receiver still records.
#[tokio::test]
async fn test_a_running_checkpoint_holds_the_drain() {
    let mut s = Setup::gated();
    s.write(0..BLOCK).await;
    assert!(s.maybe_start(Duration::ZERO));
    // The first poll syncs and hands the record to a blocking thread.
    assert!(futures::poll!(std::future::poll_fn(|cx| s.checkpoints.poll(cx))).is_pending());
    let gate = s.gate.take().unwrap();
    gate.entered
        .recv_timeout(Duration::from_secs(10))
        .expect("the record starts");
    let Setup {
        checkpoints,
        drain,
        received,
        ..
    } = s;
    drop(checkpoints);
    let mut wait = std::pin::pin!(drain.wait());
    assert!(
        futures::poll!(wait.as_mut()).is_pending(),
        "still recording"
    );
    gate.release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), wait)
        .await
        .expect("the drain ends once the record returned");
    assert_eq!(received.calls(), [Call::Record(vec![0])]);
}

/// At teardown, the running checkpoint ends first, then every complete
/// block left is recorded, due or not; the last, short block included.
#[tokio::test]
async fn test_the_teardown_checkpoint() {
    let mut s = Setup::new(1, Duration::from_secs(3600));
    s.write(0..BLOCK).await;
    assert!(s.maybe_start(Duration::ZERO));
    s.write(BLOCK..TOTAL).await;
    let Setup {
        checkpoints,
        written,
        received,
        memory,
        ..
    } = s;
    checkpoints.finish(&written).await;
    assert_eq!(
        received.calls(),
        [Call::Record(vec![0]), Call::Record(vec![1, 2, 3, 4, 5, 6])]
    );
    let recorded = received.recorded();
    assert_eq!(recorded.last().unwrap().len, (TOTAL % BLOCK) as u32);
    assert_records_match(&recorded, &memory, BLOCK);
}

/// A writer that is not asked for CRCs computes none, and a failed write
/// carries none, even when asked.
#[tokio::test]
async fn test_crcs_only_for_successful_writes_when_asked() {
    let (file_writer, ..) = FileWriter::in_memory(TOTAL, vec![BLOCK..2 * BLOCK]);
    let file_writer = Arc::new(file_writer);
    let rt = crate::runtime::ThreadedRuntimeImpl::new_tokio_rt();
    let mut plain = PendingWriter::new(file_writer.clone(), rt.clone(), 4);
    plain.write_range(0..BLOCK, pattern(0, BLOCK, 1)).unwrap();
    let completions = plain.finish().await;
    assert!(completions[0].result.is_ok());
    assert_eq!(completions[0].crcs, []);

    let mut asked = PendingWriter::new(file_writer, rt, 4);
    asked.checksum(NonZeroU64::new(BLOCK).unwrap());
    asked
        .write_range(8..2 * BLOCK, pattern(8, 2 * BLOCK - 8, 1))
        .unwrap();
    asked
        .write_range(BLOCK..2 * BLOCK, pattern(BLOCK, BLOCK, 1))
        .unwrap();
    let mut completions = asked.finish().await;
    completions.sort_by_key(|completion| completion.range.start);
    assert!(completions[0].result.is_ok());
    let data = pattern(8, 2 * BLOCK - 8, 1);
    assert_eq!(
        completions[0].crcs,
        [
            CrcSegment {
                offset: 8,
                len: 8,
                crc: crc32c::crc32c(&data[..8]),
            },
            CrcSegment {
                offset: BLOCK,
                len: BLOCK as u32,
                crc: crc32c::crc32c(&data[8..]),
            },
        ]
    );
    assert!(completions[1].result.is_err());
    assert_eq!(completions[1].crcs, []);
}

/// Every backend syncs: the pool, the mapping and compio each report a
/// sync that succeeds, and the data is in the file.
#[tokio::test]
async fn test_every_backend_syncs() {
    use crate::task::instance::concurrent_task::file_writer::FileRangeWriterKind;

    let kinds = [
        FileRangeWriterKind::Pool,
        FileRangeWriterKind::Null,
        #[cfg(feature = "mmap")]
        FileRangeWriterKind::Mmap,
        #[cfg(feature = "compio")]
        FileRangeWriterKind::Compio,
    ];
    for kind in kinds {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("file.tmp");
        let writer = FileWriter::new_with_kind(&path, 8, kind).await.unwrap();
        writer
            .write_range(0..8, Bytes::from_static(b"datadata"))
            .await
            .unwrap();
        writer
            .sync()
            .await
            .unwrap_or_else(|e| panic!("{kind:?}: {e}"));
        writer
            .write_range(0..4, Bytes::from_static(b"DATA"))
            .await
            .unwrap();
        writer.sync().await.unwrap();
        writer.finalize().await.unwrap();
        if kind != FileRangeWriterKind::Null {
            assert_eq!(std::fs::read(&path).unwrap(), b"DATAdata", "{kind:?}");
        }
    }
}

/// The partial file, read by path.
#[test]
fn test_file_blocks_read_a_range() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("file.tmp");
    std::fs::write(&path, b"0123456789").unwrap();
    let blocks = FileBlocks(path);
    assert_eq!(blocks.read(3..7).unwrap(), b"3456");
    assert!(blocks.read(8..12).is_err(), "past the end");
}

/// When dropped, records whether the drain group was still open. It holds
/// the group's only waiter, so only the checkpoint's clone keeps it open.
/// Recorded, not asserted: a panic in a drop during an unwind aborts.
struct DropProbe {
    wait: std::pin::Pin<Box<async_waitgroup::Wait>>,
    seen: Arc<std::sync::atomic::AtomicU8>,
}

const ALIVE: u8 = 0;
const DRAIN_OPEN: u8 = 1;
const DRAIN_DONE: u8 = 2;

impl Drop for DropProbe {
    fn drop(&mut self) {
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let seen = if self.wait.as_mut().poll(&mut cx).is_pending() {
            DRAIN_OPEN
        } else {
            DRAIN_DONE
        };
        self.seen.store(seen, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A receiver that records nothing, and panics in `record` when asked to.
struct PanickingReceiver {
    _probe: DropProbe,
    panic: bool,
    /// When set, `record` waits until the test sends on it, or drops it.
    release: Option<std::sync::mpsc::Receiver<()>>,
}

impl CheckpointReceiver for PanickingReceiver {
    fn record(&mut self, _: &[CheckpointBlock]) -> io::Result<()> {
        if let Some(release) = self.release.take() {
            let _ = release.recv_timeout(Duration::from_secs(30));
        }
        assert!(!self.panic, "injected receiver panic");
        Ok(())
    }

    fn roll_back(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A reader that panics when asked to.
struct PanickingReader {
    _probe: DropProbe,
    panic: bool,
}

impl BlockReader for PanickingReader {
    fn read(&self, range: Range<u64>) -> io::Result<Vec<u8>> {
        assert!(!self.panic, "injected reader panic");
        Ok(vec![0; (range.end - range.start) as usize])
    }
}

/// The work of a checkpoint held with a drain group, and what the probes in
/// its receiver and its reader saw when they dropped. The group's other
/// members are the probes' waiters, so only the work's clone keeps it open.
fn probed_work(
    receiver_panics: bool,
    reader_panics: bool,
) -> (
    crate::task::instance::drained_file::Drained<Work>,
    [Arc<std::sync::atomic::AtomicU8>; 2],
) {
    probed_work_released_by(receiver_panics, reader_panics, None)
}

/// [`probed_work`], whose receiver records only once `release` sends.
fn probed_work_released_by(
    receiver_panics: bool,
    reader_panics: bool,
    release: Option<std::sync::mpsc::Receiver<()>>,
) -> (
    crate::task::instance::drained_file::Drained<Work>,
    [Arc<std::sync::atomic::AtomicU8>; 2],
) {
    let drain = WaitGroup::new();
    let receiver_wait = drain.clone();
    let reader_wait = drain.clone();
    let held = drain.clone();
    drop(drain);
    let seen = [
        Arc::new(std::sync::atomic::AtomicU8::new(ALIVE)),
        Arc::new(std::sync::atomic::AtomicU8::new(ALIVE)),
    ];
    let work = Work {
        receiver: Box::new(PanickingReceiver {
            _probe: DropProbe {
                wait: Box::pin(receiver_wait.wait()),
                seen: seen[0].clone(),
            },
            panic: receiver_panics,
            release,
        }),
        reader: Arc::new(PanickingReader {
            _probe: DropProbe {
                wait: Box::pin(reader_wait.wait()),
                seen: seen[1].clone(),
            },
            panic: reader_panics,
        }),
    };
    (
        crate::task::instance::drained_file::Drained::new(work, Some(held)),
        seen,
    )
}

fn seen(seen: &[Arc<std::sync::atomic::AtomicU8>; 2]) -> [u8; 2] {
    seen.each_ref()
        .map(|seen| seen.load(std::sync::atomic::Ordering::SeqCst))
}

/// One block, whose CRC is read back when `read_back`.
fn one_block(read_back: bool) -> Vec<(u64, Range<u64>, BlockCrc)> {
    let crc = if read_back {
        BlockCrc::ReadBack
    } else {
        BlockCrc::Known(0)
    };
    vec![(0, 0..BLOCK, crc)]
}

/// The receiver and the reader of a checkpoint drop before its drain group
/// is released, when the work panics in the receiver or in the reader, and
/// when it completes and its result is read; the drain never ends before
/// them (C31's `Drained`).
#[tokio::test]
async fn test_the_work_drops_before_its_drain() {
    use futures::FutureExt;

    let (writer, ..) = FileWriter::in_memory(TOTAL, Vec::new());
    for (receiver_panics, reader_panics) in [(true, false), (false, true)] {
        let (held, probes) = probed_work(receiver_panics, reader_panics);
        let running = run(
            writer.clone(),
            held,
            one_block(reader_panics),
            #[cfg(test)]
            SyncTimes::default(),
        );
        let result = std::panic::AssertUnwindSafe(running).catch_unwind().await;
        assert!(result.is_err(), "{receiver_panics} {reader_panics}");
        assert_eq!(
            seen(&probes),
            [DRAIN_OPEN, DRAIN_OPEN],
            "{receiver_panics} {reader_panics}"
        );
    }
    // A completed checkpoint hands the receiver back; the reader went with
    // the work, before its drain.
    let (held, probes) = probed_work(false, false);
    let (receiver, outcome) =
        run(writer.clone(), held, one_block(true), SyncTimes::default()).await;
    assert!(matches!(outcome, Outcome::Recorded(_)), "{outcome:?}");
    assert_eq!(seen(&probes), [ALIVE, DRAIN_OPEN]);
    drop(receiver);
    assert_eq!(seen(&probes), [DRAIN_DONE, DRAIN_OPEN]);
}

/// A checkpoint dropped before its work ran drops the receiver and the
/// reader before the drain group is released.
#[tokio::test]
async fn test_a_checkpoint_dropped_before_it_ran_drops_its_work_first() {
    let (writer, ..) = FileWriter::in_memory(TOTAL, Vec::new());
    let (held, probes) = probed_work(false, false);
    let running = run(writer, held, one_block(false), SyncTimes::default());
    drop(running);
    assert_eq!(seen(&probes), [DRAIN_OPEN, DRAIN_OPEN]);
}

/// Wakes by sending on a channel: the test learns that the blocking work
/// completed without polling, so without reading, its result.
struct WakeSignal(Mutex<std::sync::mpsc::Sender<()>>);

impl futures::task::ArcWake for WakeSignal {
    fn wake_by_ref(this: &Arc<Self>) {
        let _ = this.0.lock().unwrap().send(());
    }
}

/// A checkpoint whose work completed, dropped before its result was read:
/// the result drops with the receiver and the reader first, then the drain.
#[tokio::test]
async fn test_an_unread_result_drops_its_work_before_its_drain() {
    let (writer, ..) = FileWriter::in_memory(TOTAL, Vec::new());
    let (release, released) = std::sync::mpsc::channel();
    let (held, probes) = probed_work_released_by(false, false, Some(released));
    let mut running = Box::pin(run(writer, held, one_block(true), SyncTimes::default()));
    let (woken_tx, woken) = std::sync::mpsc::channel();
    let waker = futures::task::waker(Arc::new(WakeSignal(Mutex::new(woken_tx))));
    let mut cx = std::task::Context::from_waker(&waker);
    // The writer syncs at once; the work goes to a blocking thread and
    // waits in the receiver, so the first poll cannot see it complete.
    assert!(running.as_mut().poll(&mut cx).is_pending());
    release.send(()).unwrap();
    woken
        .recv_timeout(Duration::from_secs(10))
        .expect("the work completes");
    assert_eq!(seen(&probes), [ALIVE, ALIVE], "held by the unread result");
    drop(running);
    assert_eq!(seen(&probes), [DRAIN_OPEN, DRAIN_OPEN]);
}

/// A due checkpoint waits until the last one ended at least the minimum
/// spacing ago, however many bytes were written meanwhile.
#[tokio::test]
async fn test_checkpoints_keep_their_minimum_spacing() {
    let mut s = Setup::new(1, Duration::from_secs(3600));
    s.checkpoints.schedule.min_spacing = Duration::from_secs(10);
    s.write(0..BLOCK).await;
    assert!(
        s.maybe_start(Duration::ZERO),
        "the first one is not held back"
    );
    s.settle().await;
    s.write(BLOCK..2 * BLOCK).await;
    // Times from when the first one ended, as the checkpoints saw it.
    let ended = s.checkpoints.ended.unwrap();
    let spacing = s.checkpoints.schedule.min_spacing;
    s.checkpoints
        .maybe_start(&s.written, ended + spacing - Duration::from_nanos(1));
    assert!(
        s.checkpoints.running.is_none(),
        "just too soon after the last"
    );
    s.checkpoints.maybe_start(&s.written, ended + spacing);
    assert!(s.checkpoints.running.is_some(), "exactly the spacing after");
    s.settle().await;
    assert_eq!(
        s.received.calls(),
        [Call::Record(vec![0]), Call::Record(vec![1])]
    );
}
