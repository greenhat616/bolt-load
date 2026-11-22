//! Checkpoints in the loop: the CRCs a checkpoint records are those of the
//! bytes in the file, after a hedge, a clipped frame, a restarted range and
//! a failed write; the schedule; the teardown checkpoint; and no cost
//! without them (design §11.5.5, §11.5.6).
#![expect(clippy::single_range_in_vec_init, reason = "lists holding one range")]

use std::{collections::BTreeSet, ops::Range};

use futures::FutureExt;
use pretty_assertions::assert_eq;

use super::{
    super::super::checkpoint::tests::{Call, assert_records_match},
    hedge::{Checkpointed, Harness, options},
    *,
};
use crate::runner::LifecycleEvent;

const KIB: u64 = 1 << 10;
/// Two frames.
const BLOCK: u64 = 64 * KIB;
const FRAME: u64 = crate::runner::BUFFER_SIZE as u64;

/// Checkpoints after every write, but not by time.
fn eager() -> TaskOptions {
    options(4)
        .checkpoint_bytes(std::num::NonZeroU64::new(1).unwrap())
        .checkpoint_interval(Duration::from_secs(3600))
}

/// Checkpoints at teardown only.
fn at_teardown() -> TaskOptions {
    options(4)
        .checkpoint_bytes(std::num::NonZeroU64::new(1 << 40).unwrap())
        .checkpoint_interval(Duration::from_secs(3600))
}

/// End the loop with `result` once its writes completed; the teardown
/// checkpoint included.
async fn finish(mut h: Harness, result: Result<()>, now: Instant) -> (Result<()>, Progress) {
    h.settle_writes(now);
    let mut progress = Progress::default();
    let result = h.download.finish(result, &mut progress).await;
    (result, progress)
}

/// Complete the writes in flight, and take their completions.
fn completed(h: &mut Harness) -> Vec<WriteCompletion> {
    for mut write in h.rt.take_parked() {
        assert!((&mut write).now_or_never().is_some(), "a write completes");
    }
    h.download.writer.tick().now_or_never().unwrap().unwrap()
}

/// The blocks of a file of `total` bytes that `written` covers.
fn complete_blocks(written: &[Range<u64>], total: u64, block: u64) -> BTreeSet<u64> {
    (0..total.div_ceil(block))
        .filter(|&index| {
            let range = index * block..((index + 1) * block).min(total);
            written
                .iter()
                .any(|w| w.start <= range.start && range.end <= w.end)
        })
        .collect()
}

/// Every complete block is recorded once, with the CRC of its bytes in the
/// file, synced before; no other block is.
fn assert_recorded(c: &Checkpointed, written: &[Range<u64>], total: u64) {
    let recorded = c.received.recorded();
    assert_records_match(&recorded, &c.memory, c.block);
    assert_eq!(c.received.unsynced(), Vec::<u64>::new(), "synced first");
    let indices: BTreeSet<u64> = recorded.iter().map(|block| block.index).collect();
    assert_eq!(indices, complete_blocks(written, total, c.block));
}

/// Whichever side of a hedge wins, the blocks it wrote after the other's
/// are recorded with the CRCs of the bytes in the file: the loser's frames
/// never reach it.
#[tokio::test]
async fn crcs_match_the_file_after_a_hedge() {
    const TOTAL: u64 = 3 * BLOCK + 10 * KIB;
    for hedge_first in [false, true] {
        let now = Instant::now();
        let (mut h, c) = Harness::checkpointed(TOTAL, &eager(), Vec::new(), BLOCK);
        let (o, mut eo) = h.spawn_running(0..TOTAL, now);
        eo.send_pattern(48 * KIB);
        h.go(now);
        let (hd, mut eh) = h.hedge(o, now);
        eh.start();
        h.go(now);
        assert!(h.undecided(o) && h.undecided(hd));
        let (mut winner, mut loser) = if hedge_first { (eh, eo) } else { (eo, eh) };
        winner.send_pattern(FRAME);
        h.go(now);
        loser.send_pattern(2 * FRAME);
        loser.end(LifecycleEvent::finished());
        while let Some(tick) = h.download.manager.tick().now_or_never() {
            assert!(h.download.on_runner(tick, now).unwrap().is_continue());
        }
        winner.send_pattern(TOTAL - 48 * KIB - FRAME);
        h.go(now);
        h.settle_writes(now);
        let (result, progress) = finish(h, Ok(()), now).await;
        result.unwrap();
        assert_eq!(progress.downloaded_chunks, [0..TOTAL], "{hedge_first}");
        assert_recorded(&c, &progress.downloaded_chunks, TOTAL);
        assert_eq!(c.received.recorded().len(), 4, "{hedge_first}");
    }
}

/// A frame past its chunk's end is clipped, as after a split: the block
/// that holds the end of one chunk and the start of the next is recorded
/// with the CRC of both parts.
#[tokio::test]
async fn crcs_match_the_file_after_a_clipped_frame() {
    const TOTAL: u64 = 4 * BLOCK;
    let now = Instant::now();
    let (mut h, c) = Harness::checkpointed(TOTAL, &eager(), Vec::new(), BLOCK);
    let cut = 40 * KIB;
    let (_a, mut ea) = h.spawn_running(0..cut, now);
    let (_b, mut eb) = h.spawn_running(cut..TOTAL, now);
    ea.send_pattern(2 * FRAME);
    h.go(now);
    for _ in 0..(TOTAL - cut) / (8 * KIB) {
        eb.send_pattern(8 * KIB);
        h.go(now);
        h.settle_writes(now);
    }
    h.settle_writes(now);
    let (result, progress) = finish(h, Ok(()), now).await;
    result.unwrap();
    assert_eq!(progress.downloaded_chunks, [0..TOTAL]);
    assert_recorded(&c, &progress.downloaded_chunks, TOTAL);
}

/// A runner fails part way through a block, and another downloads the
/// rest of its range: the block is recorded with the CRC of both.
#[tokio::test]
async fn crcs_match_the_file_after_a_restarted_range() {
    const TOTAL: u64 = 3 * BLOCK;
    let now = Instant::now();
    let (mut h, c) = Harness::checkpointed(TOTAL, &eager(), Vec::new(), BLOCK);
    let (a, mut ea) = h.spawn_running(0..TOTAL, now);
    let first = BLOCK + 7 * KIB;
    ea.send_pattern(first);
    h.go(now);
    assert_eq!(h.chunk(a).downloaded, 0..first);
    ea.fail(crate::runner::TaskError::Timeout);
    assert!(h.step(now).unwrap().is_continue());
    h.settle_writes(now);
    let (_b, mut eb) = h.spawn_running(first..TOTAL, now);
    eb.send_pattern(TOTAL - first);
    h.go(now);
    h.settle_writes(now);
    let (result, progress) = finish(h, Ok(()), now).await;
    result.unwrap();
    assert_eq!(progress.downloaded_chunks, [0..TOTAL]);
    assert_recorded(&c, &progress.downloaded_chunks, TOTAL);
}

/// A failed write carries no CRC and its block is never complete: it is
/// not recorded, and the blocks around it are, with their CRCs.
#[tokio::test]
async fn a_failed_write_is_not_recorded() {
    const TOTAL: u64 = 3 * BLOCK;
    let now = Instant::now();
    let failed = BLOCK + FRAME..2 * BLOCK;
    let (mut h, c) = Harness::checkpointed(TOTAL, &at_teardown(), vec![failed.clone()], BLOCK);
    let (_a, mut ea) = h.spawn_running(0..TOTAL, now);
    // Blocks 0 and the first frame of 1 are written; the failed write and
    // block 2 complete together.
    for _ in 0..3 {
        ea.send_pattern(FRAME);
        h.go(now);
        h.settle_writes(now);
    }
    for _ in 3..TOTAL / FRAME {
        ea.send_pattern(FRAME);
        h.go(now);
    }
    let completions = completed(&mut h);
    assert_eq!(completions.len(), 3);
    assert!(
        completions
            .iter()
            .all(|completion| completion.result.is_ok() != completion.crcs.is_empty()),
        "only the successful writes carry CRCs"
    );
    let error = h
        .download
        .record_completions(completions, false, now)
        .unwrap_err();
    let (result, progress) = finish(h, Err(error), now).await;
    assert!(result.is_err());
    assert_eq!(
        progress.downloaded_chunks,
        [0..failed.start, failed.end..TOTAL]
    );
    assert_recorded(&c, &progress.downloaded_chunks, TOTAL);
    assert_eq!(c.received.calls(), [Call::Record(vec![0, 2])]);
}

/// The loop starts a checkpoint once it is due, takes its end as an event,
/// and records the blocks durable; one at a time.
#[tokio::test]
async fn the_loop_takes_checkpoints_as_it_goes() {
    const TOTAL: u64 = 4 * BLOCK;
    let now = Instant::now();
    let options = options(4)
        .checkpoint_bytes(std::num::NonZeroU64::new(2 * BLOCK).unwrap())
        .checkpoint_interval(Duration::from_secs(3600));
    let (mut h, c) = Harness::checkpointed(TOTAL, &options, Vec::new(), BLOCK);
    let (_a, mut ea) = h.spawn_running(0..TOTAL, now);
    for _ in 0..3 {
        ea.send_pattern(FRAME);
        h.go(now);
        h.settle_writes(now);
    }
    assert_eq!(c.syncs.count(), 0, "96 KiB are not due");
    ea.send_pattern(FRAME);
    h.go(now);
    h.settle_writes(now);
    // 128 KiB: due; blocks 0 and 1 are complete.
    let checkpoints = h.download.checkpoints.as_mut().unwrap();
    let outcome = std::future::poll_fn(|cx| checkpoints.poll(cx)).await;
    assert!(
        h.download
            .handle_event_at(LoopEvent::Checkpoint(outcome), now)
            .unwrap()
            .is_continue()
    );
    assert_eq!(c.received.calls(), [Call::Record(vec![0, 1])]);
    assert_eq!(
        h.download.checkpoints.as_ref().unwrap().durable().ranges(),
        [0..2]
    );
    ea.send_pattern(TOTAL - 4 * FRAME);
    h.go(now);
    let (result, progress) = finish(h, Ok(()), now).await;
    result.unwrap();
    assert_eq!(
        c.received.calls(),
        [Call::Record(vec![0, 1]), Call::Record(vec![2, 3])]
    );
    assert_recorded(&c, &progress.downloaded_chunks, TOTAL);
}

/// A failed or cancelled task records its complete blocks at teardown; a
/// changed resource does not, its data is of the old version.
#[tokio::test]
async fn the_teardown_checkpoint() {
    const TOTAL: u64 = 4 * BLOCK;
    let changed = || DownloadError::ResourceChanged {
        source: crate::adapter::UnretryableError::ResourceChanged.into(),
    };
    for (result, recorded) in [
        (Err(DownloadError::Cancelled), true),
        (
            Err(DownloadError::transfer(crate::runner::TaskError::Timeout)),
            true,
        ),
        (Err(changed()), false),
    ] {
        let now = Instant::now();
        let (mut h, c) = Harness::checkpointed(TOTAL, &at_teardown(), Vec::new(), BLOCK);
        let (_a, mut ea) = h.spawn_running(0..TOTAL, now);
        ea.send_pattern(BLOCK + FRAME);
        h.go(now);
        h.settle_writes(now);
        assert_eq!(c.syncs.count(), 0);
        let (_, progress) = finish(h, result, now).await;
        assert_eq!(
            c.received.calls(),
            if recorded {
                vec![Call::Record(vec![0])]
            } else {
                vec![]
            }
        );
        if recorded {
            assert_recorded(&c, &progress.downloaded_chunks, TOTAL);
        }
        assert_eq!(c.syncs.count(), usize::from(recorded));
    }
}

/// Without checkpoints, no write carries a CRC and nothing is synced, at
/// teardown neither.
#[tokio::test]
async fn no_crc_and_no_sync_without_checkpoints() {
    const TOTAL: u64 = 4 * BLOCK;
    let now = Instant::now();
    let (mut h, syncs) = Harness::probed(TOTAL, &eager());
    let (_a, mut ea) = h.spawn_running(0..TOTAL, now);
    for _ in 0..TOTAL / FRAME {
        ea.send_pattern(FRAME);
        h.go(now);
        let completions = completed(&mut h);
        assert_eq!(completions.len(), 1);
        assert!(
            completions
                .iter()
                .all(|completion| completion.crcs.is_empty())
        );
        h.download
            .record_completions(completions, false, now)
            .unwrap();
    }
    let (result, _) = finish(h, Ok(()), now + Duration::from_secs(7200)).await;
    result.unwrap();
    assert_eq!(syncs.count(), 0);
}

/// Teardown waits for a checkpoint whose receiver is still recording,
/// whatever ended the task, before the loop hands the file over to be
/// committed or removed; a changed resource takes no new checkpoint after
/// it, the others record the rest. The drain group is held meanwhile.
#[tokio::test]
async fn teardown_waits_for_a_running_checkpoint() {
    const TOTAL: u64 = 2 * BLOCK;
    let changed = || DownloadError::ResourceChanged {
        source: crate::adapter::UnretryableError::ResourceChanged.into(),
    };
    for (result, teardown) in [
        (Err(changed()), false),
        (Err(DownloadError::Cancelled), true),
        (
            Err(DownloadError::transfer(crate::runner::TaskError::Timeout)),
            true,
        ),
        (Ok(()), true),
    ] {
        let now = Instant::now();
        let (mut h, mut c) = Harness::checkpointed_gated(TOTAL, &eager(), BLOCK);
        let gate = c.gate.take().unwrap();
        let (_a, mut ea) = h.spawn_running(0..TOTAL, now);
        ea.send_pattern(FRAME);
        h.go(now);
        h.settle_writes(now);
        ea.send_pattern(FRAME);
        h.go(now);
        h.settle_writes(now);
        // Block 0 is complete: its checkpoint runs, and its record waits.
        let checkpoints = h.download.checkpoints.as_mut().unwrap();
        assert!(futures::poll!(std::future::poll_fn(|cx| checkpoints.poll(cx))).is_pending());
        gate.entered
            .recv_timeout(Duration::from_secs(10))
            .expect("the record starts");
        ea.send_pattern(TOTAL - 2 * FRAME);
        h.go(now);
        let ended = result.is_err();
        let mut finished = std::pin::pin!(finish(h, result, now));
        assert!(
            futures::poll!(finished.as_mut()).is_pending(),
            "teardown waits for the record"
        );
        let drain = std::mem::replace(&mut c.drain, WaitGroup::new());
        let mut drained = std::pin::pin!(drain.wait());
        assert!(futures::poll!(drained.as_mut()).is_pending());
        gate.release.send(()).unwrap();
        let (outcome, progress) = tokio::time::timeout(Duration::from_secs(10), finished)
            .await
            .expect("teardown ends once the record returned");
        assert_eq!(outcome.is_err(), ended);
        assert!(drained.now_or_never().is_some(), "the work ended");
        let mut calls = vec![Call::Record(vec![0])];
        if teardown {
            calls.push(Call::Record(vec![1]));
        }
        assert_eq!(c.received.calls(), calls, "{teardown}");
        assert_recorded_subset(&c, &progress.downloaded_chunks, TOTAL);
    }
}

/// Every block recorded has the CRC of its bytes and was synced first.
fn assert_recorded_subset(c: &Checkpointed, written: &[Range<u64>], total: u64) {
    let recorded = c.received.recorded();
    assert_records_match(&recorded, &c.memory, c.block);
    assert_eq!(c.received.unsynced(), Vec::<u64>::new());
    let complete = complete_blocks(written, total, c.block);
    assert!(recorded.iter().all(|block| complete.contains(&block.index)));
}

/// Once a failed rollback stopped the checkpoints, the writes carry no CRC
/// and nothing is kept for them.
#[tokio::test]
async fn no_crc_after_the_checkpoints_stopped() {
    const TOTAL: u64 = 4 * BLOCK;
    let now = Instant::now();
    let (mut h, c) = Harness::checkpointed(TOTAL, &eager(), Vec::new(), BLOCK);
    c.received.fail_records(1);
    c.received.fail_rollbacks(1);
    let (_a, mut ea) = h.spawn_running(0..TOTAL, now);
    for _ in 0..2 {
        ea.send_pattern(FRAME);
        h.go(now);
        h.settle_writes(now);
    }
    let checkpoints = h.download.checkpoints.as_mut().unwrap();
    let outcome = std::future::poll_fn(|cx| checkpoints.poll(cx)).await;
    assert!(
        h.download
            .handle_event_at(LoopEvent::Checkpoint(outcome), now)
            .unwrap()
            .is_continue()
    );
    assert!(h.download.checkpoints.as_ref().unwrap().is_stopped());
    for _ in 2..TOTAL / FRAME {
        ea.send_pattern(FRAME);
        h.go(now);
        let completions = completed(&mut h);
        assert!(
            completions
                .iter()
                .all(|completion| completion.crcs.is_empty())
        );
        h.download
            .record_completions(completions, false, now)
            .unwrap();
    }
    assert_eq!(h.download.checkpoints.as_ref().unwrap().kept(), (0, 0, 0));
    let (result, _) = finish(h, Ok(()), now).await;
    result.unwrap();
    assert_eq!(c.received.calls(), [Call::Record(vec![0]), Call::RollBack]);
}
