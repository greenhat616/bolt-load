//! The bytes written, as the writer's completions report them (design
//! §11.1, §14.21).

use futures::FutureExt;
use pretty_assertions::assert_eq;

use super::{
    super::super::chunk_planner::ChunkStatus,
    hedge::{Harness, options},
    *,
};

const TOTAL: u64 = 1 << 30;
const FRAME: u64 = crate::runner::BUFFER_SIZE as u64;

/// The accepted bytes are written once their write completes: before, the
/// planner holds them as downloaded, not as written, and so does the
/// progress the loop publishes.
#[tokio::test]
#[expect(clippy::single_range_in_vec_init, reason = "lists holding one range")]
async fn written_lags_downloaded_until_the_write_completes() {
    let now = Instant::now();
    let mut h = Harness::new(TOTAL, &options(2));
    let (a, mut ea) = h.spawn_running(0..TOTAL, now);
    ea.send(FRAME);
    ea.send(FRAME);
    h.go(now);
    h.go(now);
    assert_eq!(h.chunk(a).downloaded, 0..2 * FRAME);
    assert_eq!(h.download.manager.written().ranges(), []);
    let published = |h: &mut Harness| {
        h.download.on_throughput_tick();
        let progress = h.status.borrow_and_update().progress.clone();
        (progress.downloaded, progress.downloaded_chunks)
    };
    assert_eq!(published(&mut h), (2 * FRAME, vec![]));

    h.settle_writes(now);
    assert_eq!(h.download.manager.written().ranges(), [0..2 * FRAME]);
    assert_eq!(published(&mut h), (2 * FRAME, vec![0..2 * FRAME]));
    assert_eq!(h.chunk(a).status, ChunkStatus::Running);
}

/// Runner A delivers `frames` frames; the writes of the first complete and
/// are handled, those of the others are left in flight.
fn in_flight(
    h: &mut Harness,
    frames: u64,
    now: Instant,
) -> Vec<futures::future::FutureObj<'static, ()>> {
    let (_a, mut ea) = h.spawn_running(0..TOTAL, now);
    ea.send(FRAME);
    h.go(now);
    h.settle_writes(now);
    for _ in 1..frames {
        ea.send(FRAME);
        h.go(now);
    }
    h.rt.take_parked()
}

/// End the loop with `result`, its writes in flight completed in `order`.
fn finish_with(
    h: Harness,
    mut writes: Vec<futures::future::FutureObj<'static, ()>>,
    order: &[usize],
    result: Result<()>,
) -> (Result<()>, Progress) {
    for &index in order {
        assert!(
            (&mut writes[index]).now_or_never().is_some(),
            "a write completes"
        );
    }
    let mut progress = Progress::default();
    let result = h
        .download
        .finish(result, &mut progress)
        .now_or_never()
        .expect("nothing left to wait for");
    (result, progress)
}

/// A cancelled task counts the writes that completed in its flush.
#[tokio::test]
#[expect(clippy::single_range_in_vec_init, reason = "lists holding one range")]
async fn a_cancelled_task_counts_its_flushed_writes() {
    let now = Instant::now();
    let mut h = Harness::new(TOTAL, &options(2));
    let writes = in_flight(&mut h, 2, now);
    let (result, progress) = finish_with(h, writes, &[0], Err(DownloadError::Cancelled));
    assert!(
        matches!(result, Err(DownloadError::Cancelled)),
        "{result:?}"
    );
    assert_eq!(progress.downloaded, 2 * FRAME);
    assert_eq!(progress.downloaded_chunks, [0..2 * FRAME]);
}

/// A task that failed keeps its error over a failed write of its flush; the
/// flush's other writes count, the failed one does not.
#[tokio::test]
#[expect(clippy::single_range_in_vec_init, reason = "lists holding one range")]
async fn a_failed_task_keeps_its_error_over_the_flush() {
    let now = Instant::now();
    let mut h = Harness::failing(TOTAL, &options(2), vec![FRAME..2 * FRAME]);
    let writes = in_flight(&mut h, 3, now);
    let failed = DownloadError::transfer(crate::runner::TaskError::Timeout);
    let (result, progress) = finish_with(h, writes, &[0, 1], Err(failed));
    assert!(
        matches!(
            result.as_ref().map_err(|e| e.as_transfer()),
            Err(Some(crate::runner::TaskError::Timeout))
        ),
        "{result:?}"
    );
    assert_eq!(progress.downloaded, 3 * FRAME);
    assert_eq!(progress.downloaded_chunks, [0..FRAME, 2 * FRAME..3 * FRAME]);
}

/// Of two failed writes in the flush, the first to complete is the task's
/// error, whatever its offset; neither counts.
#[tokio::test]
#[expect(clippy::single_range_in_vec_init, reason = "lists holding one range")]
async fn the_first_failed_write_of_the_flush_is_the_error() {
    let now = Instant::now();
    let mut h = Harness::failing(
        TOTAL,
        &options(2),
        vec![FRAME..2 * FRAME, 2 * FRAME..3 * FRAME],
    );
    let writes = in_flight(&mut h, 3, now);
    // The later range completes first.
    let (result, progress) = finish_with(h, writes, &[1, 0], Ok(()));
    let Some(crate::runner::TaskError::Write { range, .. }) =
        result.as_ref().err().and_then(|e| e.as_transfer())
    else {
        panic!("expected a write error, got {result:?}");
    };
    assert_eq!(*range, 2 * FRAME..3 * FRAME);
    assert_eq!(progress.downloaded_chunks, [0..FRAME]);
}

/// The writes still in flight when the loop ends count in its final
/// progress once the flush completes them.
#[tokio::test]
#[expect(clippy::single_range_in_vec_init, reason = "lists holding one range")]
async fn the_final_flush_counts_its_writes() {
    let now = Instant::now();
    let mut h = Harness::new(TOTAL, &options(2));
    let (_a, mut ea) = h.spawn_running(0..TOTAL, now);
    ea.send(FRAME);
    h.go(now);
    h.settle_writes(now);
    ea.send(FRAME);
    h.go(now);
    // The second write completes, but the loop has not handled it.
    for mut write in h.rt.take_parked() {
        assert!((&mut write).now_or_never().is_some(), "a write completes");
    }
    assert_eq!(h.download.manager.written().ranges(), [0..FRAME]);

    let mut progress = Progress::default();
    h.download
        .finish(Ok(()), &mut progress)
        .now_or_never()
        .expect("nothing left to wait for")
        .unwrap();
    assert_eq!(progress.downloaded, 2 * FRAME);
    assert_eq!(progress.downloaded_chunks, [0..2 * FRAME]);
}
