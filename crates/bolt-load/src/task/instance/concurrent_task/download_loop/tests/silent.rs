//! Restarting silent runners, and the growth past them, under the doubling
//! strategy (design §9.2, §9.5, §14.19).
//!
//! Runners are spawned through the manager with channels the test holds,
//! as in the hedge's tests; runner A delivers a MiB before every tick,
//! through the manager, and the runner judged stays silent. Every step
//! gets its time passed in.

use std::task::Poll;

use futures::FutureExt;
use pretty_assertions::assert_eq;

use super::{
    super::{
        super::{
            chunk_planner::{ChunkStatus, DEFAULT_MIN_CHUNK_SIZE},
            strategy::SILENT_RESTART_AFTER,
        },
        hedge::HEDGE_IDLE,
    },
    hedge::{Ends, Harness, options},
    *,
};
use crate::{
    adapter::UnretryableError,
    runner::{DATA_FRAME_CHANNEL_CAPACITY, LifecycleEvent, LimitReceiver},
    task::RunnerId,
};

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
const MIN: u64 = DEFAULT_MIN_CHUNK_SIZE;
const TOTAL: u64 = 64 * MIB;
const HALF: u64 = TOTAL / 2;
const TICK: Duration = DEFAULT_STRATEGY_TICK_INTERVAL;
const NANO: Duration = Duration::from_nanos(1);
/// A runner's frame.
const FRAME: u64 = crate::runner::BUFFER_SIZE as u64;

/// Runner A on the first half and runner B on the second, both running at
/// `base`, where B delivers `first` bytes, if any, and the first tick
/// samples them.
fn two_runners(
    options: &TaskOptions,
    first: u64,
    base: Instant,
) -> (Harness, RunnerId, RunnerId, Ends, Ends) {
    let mut h = Harness::new(TOTAL, options);
    let (a, ea) = h.spawn_running(0..HALF, base);
    let (b, eb) = h.spawn_running(HALF..TOTAL, base);
    if first > 0 {
        h.deliver(b, first, base);
    }
    h.download.on_strategy_tick(base);
    (h, a, b, ea, eb)
}

/// A tick at `now`, after `fed` deliver a MiB each.
fn tick(h: &mut Harness, fed: &[RunnerId], now: Instant) {
    for &runner_id in fed {
        h.deliver(runner_id, MIB, now);
    }
    h.download.on_strategy_tick(now);
}

/// Fill the writer with frames of A's.
fn fill_writer(h: &mut Harness, ea: &mut Ends, now: Instant) {
    while h.download.writer.can_write() {
        ea.send(16 * KIB);
        h.go(now);
    }
}

/// Spawn a running runner for `range` at `now` that keeps its limits, so
/// that a split can shrink it.
fn spawn_splittable(
    h: &mut Harness,
    range: std::ops::Range<u64>,
    now: Instant,
) -> (RunnerId, Ends, LimitReceiver) {
    let slot = h.download.manager.try_reserve_slot().unwrap();
    let runner_id = slot.runner_id;
    let (mut ends, channels) = Ends::new();
    let mut limits = None;
    assert!(
        h.download
            .manager
            .spawn_in(slot, range, now, |_, receiver| {
                limits = Some(receiver);
                Ok(channels)
            })
    );
    ends.start();
    h.go(now);
    (runner_id, ends, limits.unwrap())
}

fn stopping(h: &Harness, runner_id: RunnerId) -> bool {
    h.download.manager.is_stopping(runner_id)
}

/// A running runner with no bytes accepted for 4.5 s, among one that
/// delivers, is stopped at the tick its silence reaches 4.5 s, and not a
/// nanosecond before. Its range comes back from the accepted bytes: after
/// data, a restart, not charged; without, charged, as a timeout without
/// data is (design §14.13, §14.19).
#[tokio::test]
async fn a_silent_runner_is_restarted() {
    for first in [0, 64 * KIB] {
        let base = Instant::now();
        let (mut h, a, b, _ea, mut eb) = two_runners(&options(2), first, base);
        tick(&mut h, &[a], base + TICK);
        tick(&mut h, &[a], base + 2 * TICK);
        let at = base + SILENT_RESTART_AFTER;
        tick(&mut h, &[a], at - NANO);
        assert!(!stopping(&h, b), "{first}");
        tick(&mut h, &[a], at);
        assert!(stopping(&h, b) && eb.stop.is_cancelled(), "{first}");
        assert!(!stopping(&h, a));

        eb.fail(TaskError::Cancelled);
        h.go(at);
        let cooldowns = h.download.manager.cooldowns();
        let [(range, cooldown)] = cooldowns.as_slice() else {
            panic!("expected one cooldown, got {cooldowns:?}");
        };
        assert_eq!(*range, HALF + first..TOTAL);
        assert_eq!(
            (cooldown.attempts, cooldown.charged),
            (1, u32::from(first == 0)),
            "{first}"
        );
        assert_eq!(h.slots(), 1);
    }
}

/// A silent runner with less than twice `min_chunk_size` left is left to
/// the endgame hedge, whatever the hedge can do: exactly twice is
/// restarted (design §14.19).
#[tokio::test]
async fn a_runner_near_its_end_is_left_to_the_hedge() {
    for (left, restarted) in [(2 * MIN - 1, false), (2 * MIN, true)] {
        let base = Instant::now();
        let (mut h, a, b, _ea, _eb) = two_runners(&options(2), HALF - left, base);
        assert_eq!(h.chunk(b).remaining(), left);
        for n in 1..=7 {
            tick(&mut h, &[a], base + TICK * n);
        }
        assert_eq!(stopping(&h, b), restarted, "{left} left");
    }
}

/// A side of an undecided hedged pair is never restarted, however long it
/// is silent (design §9.6).
#[tokio::test]
async fn a_hedged_runner_is_not_restarted() {
    let base = Instant::now();
    let (mut h, a, b, _ea, _eb) = two_runners(&options(3), 0, base);
    let (hd, _eh) = h.hedge(b, base);
    for n in 1..=7 {
        tick(&mut h, &[a], base + TICK * n);
    }
    assert!(h.undecided(b) && h.undecided(hd));
    assert!(!stopping(&h, b) && !stopping(&h, hd));
}

/// A runner still connecting is never silent: the connect timeout deals
/// with it. One that connects late is silent from the first tick it ran
/// at, not from its spawn.
#[tokio::test]
async fn a_runner_is_silent_from_the_tick_it_runs_at() {
    let base = Instant::now();
    let mut h = Harness::new(TOTAL, &options(2));
    let (a, _ea) = h.spawn_running(0..HALF, base);
    let (b, mut eb) = h.spawn(HALF..TOTAL, base);
    h.download.on_strategy_tick(base);
    for n in 1..=3 {
        tick(&mut h, &[a], base + TICK * n);
        assert!(!stopping(&h, b), "connecting, tick {n}");
    }
    // Connected just before the tick at 6 s.
    eb.start();
    h.go(base + TICK * 4);
    assert_eq!(h.chunk(b).status, ChunkStatus::Running);
    for n in 4..=6 {
        tick(&mut h, &[a], base + TICK * n);
        assert!(!stopping(&h, b), "tick {n}");
    }
    let at = base + TICK * 4 + SILENT_RESTART_AFTER;
    tick(&mut h, &[a], at - NANO);
    assert!(!stopping(&h, b));
    tick(&mut h, &[a], at);
    assert!(stopping(&h, b));
}

/// While the writer is full, no runner is judged silent. Its recovery does
/// not restart a silence: the runner measures it by its own reads, and a
/// runner the writer held back was pushing. So a runner stalled since its
/// spawn is stopped at the first tick the writer can write, the hedge's
/// silence restarting there all the same (design §9.6, §14.19).
#[tokio::test]
async fn a_full_writer_is_no_silence() {
    let base = Instant::now();
    let (mut h, a, b, mut ea, _eb) = two_runners(&options(2), 0, base);
    fill_writer(&mut h, &mut ea, base);
    for n in 1..=7 {
        tick(&mut h, &[a], base + TICK * n);
        assert!(!stopping(&h, b), "tick {n}");
    }

    // One write completes.
    let mut writes = h.rt.take_parked();
    assert!((&mut writes[0]).now_or_never().is_some());
    let recovered = base + TICK * 7;
    let event = futures::poll!(futures::future::poll_fn(|cx| h.download.poll_writer(cx)));
    let Poll::Ready(event @ LoopEvent::WriterCompleted(_, true)) = event else {
        panic!("expected the completion of a full writer");
    };
    assert!(
        h.download
            .handle_event_at(event, recovered)
            .unwrap()
            .is_continue()
    );
    assert!(h.download.writer.can_write());
    assert_eq!(h.download.manager.last_delivery(b), Some(recovered));
    tick(&mut h, &[a], recovered + TICK);
    assert!(stopping(&h, b), "silent since its spawn");
    drop(writes);
}

/// No runner is restarted while the task is congested: its range could not
/// connect again before the congestion ends (design §8.1).
#[tokio::test]
async fn no_silent_restart_while_congested() {
    let base = Instant::now();
    let (mut h, a, b, _ea, _eb) = two_runners(&options(2), 0, base);
    h.download.backoff.on_failure(
        &TaskError::StreamError {
            source: AdapterError::Unretryable {
                source: UnretryableError::Congestion {
                    status: 503,
                    retry_after: Some(Duration::from_secs(9)),
                },
            },
        },
        1,
        base,
    );
    let until = h.download.backoff.congestion_until().unwrap();
    for n in 1..=5 {
        let now = base + TICK * n;
        assert!(now < until);
        tick(&mut h, &[a], now);
        assert!(!stopping(&h, b), "tick {n}");
    }
    tick(&mut h, &[a], until);
    assert!(stopping(&h, b));
}

/// At most one runner is stopped a tick: of two silent ones, the one
/// silent the longest, the lower ID first, then the other at the next
/// tick.
#[tokio::test]
async fn one_silent_runner_is_stopped_a_tick() {
    let base = Instant::now();
    let mut h = Harness::new(TOTAL, &options(3));
    let (a, _ea) = h.spawn_running(0..HALF, base);
    let (b, _eb) = h.spawn_running(HALF..HALF + HALF / 2, base);
    let (c, _ec) = h.spawn_running(HALF + HALF / 2..TOTAL, base);
    h.download.on_strategy_tick(base);
    for n in 1..=2 {
        tick(&mut h, &[a], base + TICK * n);
    }
    let at = base + SILENT_RESTART_AFTER;
    tick(&mut h, &[a], at);
    assert_eq!((stopping(&h, b), stopping(&h, c)), (true, false));
    tick(&mut h, &[a], at + TICK);
    assert!(stopping(&h, c));
}

/// The slow and the silent restart share their one stop a tick: when the
/// slow one stops a runner, a silent one waits for the next tick.
///
/// A and D deliver a MiB a tick, C a frame; B connects at 6 s and stays
/// silent. At 10.5 s the slow window is full for A, C and D, and C is
/// slow; B is silent for 4.5 s since it ran, but too young to be judged
/// slow.
#[tokio::test]
async fn the_slow_and_the_silent_restart_share_a_tick() {
    const QUARTER: u64 = TOTAL / 4;
    let base = Instant::now();
    let mut h = Harness::new(TOTAL, &options(4));
    let (a, _ea) = h.spawn_running(0..QUARTER, base);
    let (c, _ec) = h.spawn_running(QUARTER..2 * QUARTER, base);
    let (d, _ed) = h.spawn_running(2 * QUARTER..3 * QUARTER, base);
    let (b, mut eb) = h.spawn(3 * QUARTER..TOTAL, base);
    h.download.on_strategy_tick(base);
    let step = |h: &mut Harness, now: Instant| {
        h.deliver(c, FRAME, now);
        tick(h, &[a, d], now);
    };
    for n in 1..=3 {
        step(&mut h, base + TICK * n);
    }
    eb.start();
    h.go(base + TICK * 4);
    for n in 4..=6 {
        step(&mut h, base + TICK * n);
        assert!(!stopping(&h, b) && !stopping(&h, c), "tick {n}");
    }
    step(&mut h, base + TICK * 7);
    assert_eq!((stopping(&h, c), stopping(&h, b)), (true, false));
    step(&mut h, base + TICK * 8);
    assert!(stopping(&h, b));
}

/// A runner that reads its body slowly, never filling a frame within
/// 4.5 s, next to a fast one, is reading, not silent: it is never stopped,
/// nothing is charged, and its frames come in (design §14.19).
#[tokio::test]
async fn a_slow_runner_that_reads_is_never_silent() {
    let base = Instant::now();
    let (mut h, a, b, _ea, eb) = two_runners(&options(2), 0, base);
    // 5 KiB/s: a frame of 32 KiB about every 6.4 s, every fifth tick.
    for n in 1..=20 {
        let now = base + TICK * n;
        // It read a little just before the tick.
        eb.reads.note_at(now - TICK / 3);
        if n % 5 == 0 {
            h.deliver(b, FRAME, now);
        }
        tick(&mut h, &[a], now);
        assert!(!stopping(&h, b), "tick {n}");
    }
    assert_eq!(h.download.manager.cooldowns(), []);
    let chunk = h.chunk(b);
    assert_eq!(
        (chunk.downloaded.end - chunk.downloaded.start, chunk.charged),
        (4 * FRAME, 0)
    );
}

/// A runner pushing a frame waits for the loop, and is not silent, however
/// long it waits; from when it reads on after its push, its silence starts
/// over (design §14.19).
#[tokio::test]
async fn a_pushing_runner_is_never_silent() {
    let base = Instant::now();
    let (mut h, a, b, _ea, eb) = two_runners(&options(2), 0, base);
    eb.reads.start_emitting();
    for n in 1..=7 {
        tick(&mut h, &[a], base + TICK * n);
        assert!(!stopping(&h, b), "tick {n}");
    }
    let pushed = base + TICK * 7;
    eb.reads.stop_emitting();
    eb.reads.note_at(pushed);
    tick(&mut h, &[a], pushed + SILENT_RESTART_AFTER - NANO);
    assert!(!stopping(&h, b));
    tick(&mut h, &[a], pushed + SILENT_RESTART_AFTER);
    assert!(stopping(&h, b));
}

/// A runner that connects only after the others finished, and long after
/// the samples of their rates are gone, is restarted 4.5 s after the tick
/// it first ran at, if it reads nothing (design §14.19).
#[tokio::test]
async fn a_late_runner_alone_is_restarted() {
    let base = Instant::now();
    let mut h = Harness::new(TOTAL, &options(2));
    let (a, mut ea) = h.spawn_running(0..HALF, base);
    let (b, mut eb) = h.spawn(HALF..TOTAL, base);
    h.download.on_strategy_tick(base);
    // A finishes at once.
    h.deliver(a, HALF, base + TICK);
    ea.end(LifecycleEvent::finished());
    h.go(base + TICK);
    for n in 1..=13 {
        h.download.on_strategy_tick(base + TICK * n);
    }
    // B connects at 21 s, and stalls.
    eb.start();
    h.go(base + TICK * 14);
    for n in 14..=16 {
        h.download.on_strategy_tick(base + TICK * n);
        assert!(!stopping(&h, b), "tick {n}");
    }
    let at = base + TICK * 14 + SILENT_RESTART_AFTER;
    h.download.on_strategy_tick(at - NANO);
    assert!(!stopping(&h, b));
    h.download.on_strategy_tick(at);
    assert!(stopping(&h, b));
}

/// A runner whose frames wait in its full ring, because the loop fell
/// behind, delivers: however late the next tick, and with the writer free,
/// it is neither let off for the growth nor stopped, and once the loop
/// takes its frames they are all accepted, uncharged (design §14.19).
#[tokio::test]
async fn queued_frames_are_no_silence() {
    let base = Instant::now();
    let (mut h, a, b, _ea, mut eb) = two_runners(&options(2), 0, base);
    for _ in 0..DATA_FRAME_CHANNEL_CAPACITY {
        eb.send(FRAME);
    }
    let sample = |h: &Harness| {
        let tick = h.download.rates.tick();
        (tick.all_delivering, tick.silent)
    };
    tick(&mut h, &[a], base + HEDGE_IDLE);
    assert_eq!(sample(&h), (false, false), "not let off");
    // The next tick comes 6 s late.
    let late = base + HEDGE_IDLE + SILENT_RESTART_AFTER + TICK;
    assert!(h.download.writer.can_write());
    tick(&mut h, &[a], late);
    assert_eq!(sample(&h), (false, false), "not let off");
    assert!(!stopping(&h, b) && !eb.stop.is_cancelled());

    // The loop catches up.
    for _ in 0..DATA_FRAME_CHANNEL_CAPACITY {
        h.go(late);
        h.settle_writes(late);
    }
    let chunk = h.chunk(b);
    assert_eq!(
        (chunk.downloaded.end - chunk.downloaded.start, chunk.charged),
        (DATA_FRAME_CHANNEL_CAPACITY as u64 * FRAME, 0)
    );
    assert_eq!(h.download.manager.cooldowns(), []);
    assert!(!stopping(&h, b));
}

/// A fast runner with a frame queued at every tick, because the loop fell
/// behind, still counts in the slow window's median: only the silent
/// restart leaves runners with queued frames alone (design §14.19). So the
/// slow runner next to it is stopped once its window is full.
#[tokio::test]
async fn queued_frames_do_not_hide_a_slow_runner() {
    const QUARTER: u64 = TOTAL / 4;
    let base = Instant::now();
    let mut h = Harness::new(TOTAL, &options(3));
    let (a, mut ea) = h.spawn_running(0..QUARTER, base);
    let (c, _ec) = h.spawn_running(QUARTER..2 * QUARTER, base);
    let (d, _ed) = h.spawn_running(2 * QUARTER..TOTAL, base);
    h.download.on_strategy_tick(base);
    for n in 1..=7 {
        let now = base + TICK * n;
        h.deliver(c, FRAME, now);
        tick_with_a_frame_queued(&mut h, &mut ea, &[a, d], now);
        assert_eq!(stopping(&h, c), n == 7, "tick {n}");
        assert!(!stopping(&h, a));
        // The loop catches up with A's frame.
        h.go(now);
        h.settle_writes(now);
    }
}

/// A tick at `now`, after `fed` deliver a MiB each, with a frame of the
/// runner of `queued` still in its ring.
fn tick_with_a_frame_queued(h: &mut Harness, queued: &mut Ends, fed: &[RunnerId], now: Instant) {
    queued.send(FRAME);
    tick(h, fed, now);
}

/// A runner that stalls, the writer filling and recovering meanwhile, is
/// stopped 4.5 s after its last read, not 4.5 s after the recovery: the
/// writer's recovery restarts the hedge's silence, not the silent
/// restart's (design §14.19).
#[tokio::test]
async fn a_stall_is_measured_from_the_last_read() {
    let base = Instant::now();
    let (mut h, a, b, mut ea, _eb) = two_runners(&options(2), 0, base);
    // B reads, and stalls after.
    let stalled = base + TICK;
    h.deliver(b, FRAME, stalled);
    tick(&mut h, &[a], stalled);
    fill_writer(&mut h, &mut ea, stalled);
    tick(&mut h, &[a], base + 2 * TICK);
    assert!(!stopping(&h, b), "the writer is full");
    let mut writes = h.rt.take_parked();
    assert!((&mut writes[0]).now_or_never().is_some());
    let recovered = base + 2 * TICK + TICK / 2;
    let event = futures::poll!(futures::future::poll_fn(|cx| h.download.poll_writer(cx)));
    let Poll::Ready(event @ LoopEvent::WriterCompleted(_, true)) = event else {
        panic!("expected the completion of a full writer");
    };
    assert!(
        h.download
            .handle_event_at(event, recovered)
            .unwrap()
            .is_continue()
    );
    h.settle_writes(recovered);
    let at = stalled + SILENT_RESTART_AFTER;
    tick(&mut h, &[a], at - NANO);
    assert!(!stopping(&h, b));
    tick(&mut h, &[a], at);
    assert!(stopping(&h, b), "4.5 s after its last read");
    assert!(at < recovered + SILENT_RESTART_AFTER);
    drop(writes);
}

/// A stop for being silent drops the doubling to the limit that waited for
/// its runners (design §14.18, §14.19), the shape of s4c: the others
/// finish, which leaves the doubling waiting; the stalled runner is
/// stopped, and once its range is claimed again, the growth splits it.
/// Without the stop, as after the runners that finished, the doubling
/// would wait on.
#[tokio::test]
async fn a_restart_releases_the_doubling_at_the_limit() {
    let base = Instant::now();
    let mut h = Harness::new(TOTAL, &options(4));
    let (a, _ea, _la) = spawn_splittable(&mut h, 0..HALF, base);
    let (b, mut eb, _lb) = spawn_splittable(&mut h, HALF..TOTAL, base);
    h.download.on_strategy_tick(base);
    tick(&mut h, &[a, b], base + TICK);
    // A doubling to the limit, 2 to 4, to be judged once it has its four.
    assert_eq!(h.download.manager.get_active_runners_count(), 4);
    let grown: Vec<_> = (0..4).filter(|&id| id != a && id != b).collect();
    for &id in &grown {
        h.download.manager.mark_started(id);
    }
    // B stalls; the others deliver, and all finish.
    tick(&mut h, &[a, grown[0], grown[1]], base + 2 * TICK);
    for id in [a, grown[0], grown[1]] {
        let left = h.chunk(id).remaining();
        h.deliver(id, left, base + 2 * TICK);
        h.download.manager.release_runner_full(id, true).unwrap();
    }
    for n in 3..=4 {
        h.download.on_strategy_tick(base + TICK * n);
        assert_eq!(h.download.manager.get_active_runners_count(), 1, "tick {n}");
    }
    let at = base + TICK + SILENT_RESTART_AFTER;
    h.download.on_strategy_tick(at);
    assert!(stopping(&h, b));
    eb.fail(TaskError::Cancelled);
    h.go(at);
    assert_eq!(h.download.manager.get_active_runners_count(), 0);
    // Its range is claimed again after its backoff.
    h.download.on_strategy_tick(at + TICK);
    let pending = |h: &Harness| {
        (0..8).find(|&id| {
            h.download
                .manager
                .get_runner_state(id)
                .is_some_and(|state| state.status == ChunkStatus::Pending)
        })
    };
    let e = pending(&h).expect("a runner on the released range");
    assert_eq!(h.chunk(e).allocated.start, HALF + MIB, "B read a MiB");
    let claimed = h.chunk(e).allocated;
    h.download.manager.mark_started(e);
    h.deliver(e, MIB, at + 2 * TICK);
    h.download.on_strategy_tick(at + 2 * TICK);
    assert_eq!(
        h.download.manager.get_active_runners_count(),
        2,
        "the growth splits the released range"
    );
    assert!(h.chunk(e).allocated.end < claimed.end);
    drop(h.rt.take_parked());
}

/// A stop for being slow drops the doubling to the limit too (design
/// §14.18, §14.19): a doubling to 4 waits, one of its runners having
/// finished; the slow runner is stopped on its window, and the growth
/// splits for the free slot in the same tick, before B's range comes back
/// from its backoff. Without the release, it would wait on.
#[tokio::test]
async fn a_slow_restart_releases_the_doubling_at_the_limit() {
    let base = Instant::now();
    let mut h = Harness::new(TOTAL, &options(4));
    let (a, _ea, _la) = spawn_splittable(&mut h, 0..HALF, base);
    let (b, mut eb, _lb) = spawn_splittable(&mut h, HALF..TOTAL, base);
    h.download.on_strategy_tick(base);
    tick(&mut h, &[a, b], base + TICK);
    assert_eq!(h.download.manager.get_active_runners_count(), 4);
    let grown: Vec<_> = (0..4).filter(|&id| id != a && id != b).collect();
    for &id in &grown {
        h.download.manager.mark_started(id);
    }
    let (c, done) = (grown[0], grown[1]);
    let left = h.chunk(done).remaining();
    h.deliver(done, left, base + TICK);
    h.download.manager.release_runner_full(done, true).unwrap();
    // B delivers a frame a tick, A and C a MiB: B is slow, not silent.
    let mut n = 2;
    let stopped = loop {
        let now = base + TICK * n;
        h.deliver(b, FRAME, now);
        tick(&mut h, &[a, c], now);
        assert!(!stopping(&h, a) && !stopping(&h, c));
        if stopping(&h, b) {
            break now;
        }
        assert_eq!(h.download.manager.get_active_runners_count(), 3, "tick {n}");
        n += 1;
        assert!(n <= 12, "B was never stopped");
    };
    // The tick that stopped B grew at once for its slot, splitting A's or
    // C's chunk, while B's range is still its own.
    eb.fail(TaskError::Cancelled);
    h.go(stopped);
    assert!(
        !h.download.manager.cooldowns().is_empty(),
        "B's range backs off"
    );
    assert_eq!(
        h.download.manager.get_active_runners_count(),
        3,
        "a runner more than A and C"
    );
    drop(h.rt.take_parked());
}

/// A running runner silent for the hedge's idle time no longer holds the
/// growth back (design §9.2, §14.19): the tick lets it off, and says so.
/// One that delivers nothing for less, one still connecting and one that
/// connected less than that ago still hold it back, and so does a full
/// writer, which makes every runner look silent.
#[tokio::test]
async fn a_silent_runner_does_not_hold_the_growth_back() {
    let sample = |h: &Harness| {
        let tick = h.download.rates.tick();
        (tick.all_delivering, tick.silent)
    };
    let base = Instant::now();
    let mut h = Harness::new(TOTAL, &options(4));
    let (a, _ea, _la) = spawn_splittable(&mut h, 0..HALF, base);
    let (b, _eb, _lb) = spawn_splittable(&mut h, HALF..TOTAL, base);
    h.download.on_strategy_tick(base);
    tick(&mut h, &[a], base + TICK);
    assert_eq!(sample(&h), (false, false), "silent for 1.5 s");
    assert_eq!(h.download.manager.get_active_runners_count(), 2);
    tick(&mut h, &[a], base + HEDGE_IDLE);
    assert_eq!(sample(&h), (true, true), "silent for 2 s");
    // The first growth: up to the limit, splitting B's chunk too.
    assert_eq!(h.download.manager.get_active_runners_count(), 4);
    assert!(h.chunk(b).allocated.end < TOTAL);

    // Still connecting, or connected for less than 2 s.
    let mut h = Harness::new(TOTAL, &options(4));
    let (a, _ea) = h.spawn_running(0..HALF, base);
    let (_b, mut eb) = h.spawn(HALF..TOTAL, base);
    h.download.on_strategy_tick(base);
    tick(&mut h, &[a], base + TICK);
    assert_eq!(sample(&h), (false, false), "connecting");
    eb.start();
    h.go(base + 2 * TICK);
    tick(&mut h, &[a], base + 2 * TICK);
    assert_eq!(sample(&h), (false, false), "connected just now");
    tick(&mut h, &[a], base + 3 * TICK);
    assert_eq!(sample(&h), (false, false), "running for 1.5 s");
    assert_eq!(h.download.manager.get_active_runners_count(), 2);
    tick(&mut h, &[a], base + 2 * TICK + HEDGE_IDLE);
    assert_eq!(sample(&h), (true, true), "running for 2 s");

    // Reading, though no frame is full yet: not silent, from when it read.
    let (mut h, a, _b, _ea, eb) = two_runners(&options(4), 0, base);
    tick(&mut h, &[a], base + TICK);
    let read = base + TICK + TICK / 3;
    eb.reads.note_at(read);
    tick(&mut h, &[a], base + 2 * TICK);
    assert_eq!(sample(&h), (false, false), "read in the last tick");
    tick(&mut h, &[a], read + HEDGE_IDLE - NANO);
    assert_eq!(
        sample(&h),
        (false, false),
        "read 2 s ago, less a nanosecond"
    );
    tick(&mut h, &[a], read + HEDGE_IDLE);
    assert_eq!(sample(&h), (true, true), "read nothing for 2 s");

    // Pushing a frame, the loop taking none: not silent, however long.
    let (mut h, a, _b, _ea, eb) = two_runners(&options(4), 0, base);
    eb.reads.start_emitting();
    tick(&mut h, &[a], base + TICK);
    tick(&mut h, &[a], base + 4 * TICK);
    assert_eq!(sample(&h), (false, false), "pushing for 6 s");
    eb.reads.stop_emitting();
    eb.reads.note_at(base + 4 * TICK + NANO);
    tick(&mut h, &[a], base + 4 * TICK + HEDGE_IDLE);
    assert_eq!(
        sample(&h),
        (false, false),
        "pushed 2 s ago, less a nanosecond"
    );
    tick(&mut h, &[a], base + 4 * TICK + NANO + HEDGE_IDLE);
    assert_eq!(sample(&h), (true, true), "nothing for 2 s since its push");

    // A full writer.
    let (mut h, a, _b, mut ea, _eb) = two_runners(&options(4), 0, base);
    fill_writer(&mut h, &mut ea, base);
    tick(&mut h, &[a], base + HEDGE_IDLE);
    assert_eq!(sample(&h), (false, false), "the writer is full");
    assert_eq!(h.download.manager.get_active_runners_count(), 2);
    drop(h.rt.take_parked());
}
