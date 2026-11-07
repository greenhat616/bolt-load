//! The endgame hedge in the loop (design §9.6, §14.16).
//!
//! Runners are spawned through the manager with channels the test holds,
//! so the test decides what each delivers and how it ends; the hedge the
//! trigger spawns is a real runner, parked. Writes are recorded, and every
//! step gets its time passed in.

use std::{num::NonZeroU32, ops::Range};

use async_ringbuf::{
    AsyncHeapRb,
    traits::{Producer, Split},
};
use bytes::Bytes;
use futures::FutureExt;
use pretty_assertions::assert_eq;

use super::{
    super::{
        super::{
            chunk_planner::{ChunkState, ChunkStatus, DEFAULT_MIN_CHUNK_SIZE, Side, SideFailure},
            file_writer::WrittenRanges,
            runner_manager::RunnerChannels,
            strategy::{SLOW_WINDOW, StrategyAction},
        },
        hedge::HEDGE_IDLE,
    },
    *,
};
use crate::{
    adapter::{RetryableError, UnretryableError},
    runner::{
        DATA_FRAME_CHANNEL_CAPACITY, DataFrame, DataFrameSender, LIFECYCLE_CHANNEL_CAPACITY,
        LifecycleEvent, LifecycleSender,
    },
    task::{GrowthStrategy, RunnerId},
};

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
const MIN: u64 = DEFAULT_MIN_CHUNK_SIZE;
const NANO: Duration = Duration::from_nanos(1);
const TICK: Duration = DEFAULT_STRATEGY_TICK_INTERVAL;

/// The ends of a runner the test drives, and its stop.
struct Ends {
    lifecycle: LifecycleSender,
    data: DataFrameSender,
    stop: CancellationToken,
}

impl Ends {
    fn new() -> (Self, RunnerChannels) {
        let (lifecycle, lifecycle_rx) =
            AsyncHeapRb::<LifecycleEvent>::new(LIFECYCLE_CHANNEL_CAPACITY).split();
        let (data, data_rx) = AsyncHeapRb::<DataFrame>::new(DATA_FRAME_CHANNEL_CAPACITY).split();
        let stop = CancellationToken::new();
        let ends = Self {
            lifecycle,
            data,
            stop: stop.clone(),
        };
        let channels = RunnerChannels {
            lifecycle_rx,
            data_rx,
            stop,
        };
        (ends, channels)
    }

    fn start(&mut self) {
        self.lifecycle.try_push(LifecycleEvent::Started).unwrap();
    }

    /// Queue a frame of `len` bytes.
    fn send(&mut self, len: u64) {
        self.data
            .try_push(DataFrame {
                data: Bytes::from(vec![0; len as usize]),
            })
            .unwrap();
    }

    fn end(&mut self, event: LifecycleEvent) {
        self.lifecycle.try_push(event).unwrap();
    }

    fn fail(&mut self, error: TaskError) {
        self.end(LifecycleEvent::failed(error));
    }
}

fn io() -> TaskError {
    TaskError::StreamError {
        source: AdapterError::Retryable {
            source: RetryableError::Io {
                source: Arc::new(std::io::Error::other("reset")),
            },
        },
    }
}

/// A 503 with `Retry-After: 3`.
fn congested() -> TaskError {
    TaskError::StreamError {
        source: AdapterError::Unretryable {
            source: UnretryableError::Congestion {
                status: 503,
                retry_after: Some(Duration::from_secs(3)),
            },
        },
    }
}

fn range_ignored() -> TaskError {
    TaskError::StreamError {
        source: UnretryableError::RangeIgnored.into(),
    }
}

fn refused() -> TaskError {
    TaskError::StreamError {
        source: UnretryableError::Io {
            source: Arc::new(std::io::Error::other("refused")),
        }
        .into(),
    }
}

fn is_congestion(error: &TaskError) -> bool {
    matches!(
        error,
        TaskError::StreamError {
            source: AdapterError::Unretryable {
                source: UnretryableError::Congestion { .. }
            }
        }
    )
}

fn options(max_connections: usize) -> TaskOptions {
    TaskOptions::default()
        .max_connections(NonZeroUsize::new(max_connections).unwrap())
        .growth_strategy(GrowthStrategy::Doubling)
}

/// A loop over a file of `total` bytes whose writes are recorded.
struct Harness {
    download: DownloadLoop,
    rt: ScriptedRuntime,
    written: WrittenRanges,
}

impl Harness {
    fn new(total: u64, options: &TaskOptions) -> Self {
        Self::on(ScriptedRuntime::new(TimerKind::Scripted), total, options)
    }

    fn on(rt: ScriptedRuntime, total: u64, options: &TaskOptions) -> Self {
        let (file_writer, written) = FileWriter::recording();
        let spawner = RunnerSpawner::new(
            ThreadedRuntimeImpl::new_other_rt(rt.clone()),
            // Never connected: the hedge the trigger spawns stays parked.
            DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(1).with_range_support(true)),
            WaitGroup::new(),
            CancellationToken::new(),
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        );
        let (reporter, _status) = ProgressReporter::for_test();
        let download = DownloadLoop::new(
            total,
            spawner,
            &file_writer,
            reporter,
            options,
            CancellationToken::new(),
        );
        Self {
            download,
            rt,
            written,
        }
    }

    /// Spawn a runner for `range` at `now`.
    fn spawn(&mut self, range: Range<u64>, now: Instant) -> (RunnerId, Ends) {
        let slot = self.download.manager.try_reserve_slot().unwrap();
        let runner_id = slot.runner_id;
        let (ends, channels) = Ends::new();
        assert!(
            self.download
                .manager
                .spawn_in(slot, range, now, |_, _| Ok(channels))
        );
        (runner_id, ends)
    }

    /// Spawn a running runner for `range` at `now`.
    fn spawn_running(&mut self, range: Range<u64>, now: Instant) -> (RunnerId, Ends) {
        let (runner_id, mut ends) = self.spawn(range, now);
        ends.start();
        self.go(now);
        assert_eq!(self.chunk(runner_id).status, ChunkStatus::Running);
        (runner_id, ends)
    }

    /// Hedge the chunk of `owner` at `now`.
    fn hedge(&mut self, owner: RunnerId, now: Instant) -> (RunnerId, Ends) {
        let slot = self.download.manager.try_reserve_slot().unwrap();
        let runner_id = slot.runner_id;
        let (ends, channels) = Ends::new();
        assert!(
            self.download
                .manager
                .spawn_hedge(slot, owner, now, |_, _, _| Ok(channels))
        );
        (runner_id, ends)
    }

    /// Handle the next runner event at `now`, which must be ready.
    fn step(&mut self, now: Instant) -> Result<ControlFlow<()>> {
        let tick = self
            .download
            .manager
            .tick()
            .now_or_never()
            .expect("a runner event is ready");
        self.download.on_runner(tick, now)
    }

    /// Handle every ready runner event at `now`; none may end the task.
    fn drain(&mut self, now: Instant) {
        while let Some(tick) = self.download.manager.tick().now_or_never() {
            assert!(self.download.on_runner(tick, now).unwrap().is_continue());
        }
    }

    /// Handle the next runner event at `now`, which must be ready and
    /// must not end the task.
    fn go(&mut self, now: Instant) {
        assert!(self.step(now).unwrap().is_continue());
    }

    /// Handle a frame of `len` bytes from `runner_id` at `now`, as if the
    /// manager took it from its channel now.
    fn frame(&mut self, runner_id: RunnerId, len: u64, now: Instant) {
        let tick = self
            .download
            .manager
            .on_data_frame(runner_id, Bytes::from(vec![0; len as usize]));
        assert!(self.download.on_runner(tick, now).unwrap().is_continue());
    }

    /// `runner_id` delivers `bytes` at `now`, without a write.
    fn deliver(&mut self, runner_id: RunnerId, bytes: u64, now: Instant) {
        self.download
            .manager
            .update_progress(runner_id, bytes)
            .unwrap();
        self.download.manager.note_delivery(runner_id, now);
    }

    fn chunk(&self, runner_id: RunnerId) -> ChunkState {
        self.download.manager.chunk_of(runner_id).unwrap().clone()
    }

    /// Whether `runner_id` has no chunk any more.
    fn chunk_of_none(&self, runner_id: RunnerId) -> bool {
        self.download.manager.chunk_of(runner_id).is_none()
    }

    fn undecided(&self, runner_id: RunnerId) -> bool {
        self.download.manager.is_in_undecided_pair(runner_id)
    }

    fn slots(&self) -> usize {
        self.download.manager.allocated_runner_count()
    }

    /// Complete the writes, and account for them at `now`.
    fn settle_writes(&mut self, now: Instant) {
        for mut write in self.rt.take_parked() {
            assert!((&mut write).now_or_never().is_some(), "a write completes");
        }
        if let Some(Some(completions)) = self.download.writer.tick().now_or_never() {
            self.download
                .record_completions(completions, false, now)
                .unwrap();
        }
    }

    /// The ranges written, once the writes completed: no byte was written
    /// twice.
    fn written(&mut self, now: Instant) -> Vec<Range<u64>> {
        self.settle_writes(now);
        let mut ranges = self.written.lock().unwrap().clone();
        ranges.retain(|range| !range.is_empty());
        ranges.sort_by_key(|range| range.start);
        for pair in ranges.windows(2) {
            assert!(pair[0].end <= pair[1].start, "written twice: {ranges:?}");
        }
        ranges
    }

    /// What the admission evidence holds of the ended attempts: the
    /// corroborated refusals in a row and a pending congestion.
    fn evidence(&self) -> (usize, bool) {
        let admission = self.download.manager.admission();
        (admission.streak_count(), admission.congestion_pending())
    }
}

/// The task error a step ended with.
fn transfer_error(result: Result<ControlFlow<()>>) -> TaskError {
    match result {
        Err(DownloadError::Transfer { source }) => (*source).clone(),
        other => panic!("expected a transfer error, got {other:?}"),
    }
}

// ── Trigger ──────────────────────────────────────────────────────────

/// An endgame of two running runners on 3 MiB: A on the first 1.5 MiB
/// with 512 KiB left, delivering; B on the rest, 1.5 MiB left and silent
/// since `base`.
fn endgame(options: &TaskOptions, base: Instant) -> (Harness, RunnerId, RunnerId, Ends, Ends) {
    let mut h = Harness::new(3 * MIB, options);
    let (a, ea) = h.spawn_running(0..3 * MIB / 2, base);
    let (b, eb) = h.spawn_running(3 * MIB / 2..3 * MIB, base);
    h.deliver(a, MIB, base);
    (h, a, b, ea, eb)
}

/// A runner silent for 2 s in the endgame is hedged, a nanosecond earlier
/// it is not. The hedge runner is spawned through the spawner on the rest
/// of the chunk, and both sides count as active.
#[tokio::test]
async fn a_runner_silent_for_two_seconds_is_hedged() {
    let base = Instant::now();
    let (mut h, a, b, _ea, _eb) = endgame(&options(4), base);
    h.deliver(a, 64 * KIB, base + HEDGE_IDLE - NANO);
    h.download.on_strategy_tick(base + HEDGE_IDLE - NANO);
    assert!(!h.undecided(b));
    assert!(h.rt.take_parked().is_empty());

    h.deliver(a, 64 * KIB, base + HEDGE_IDLE);
    let deadline = h.download.policy.deadline();
    h.download.hedge_stalled_runner(base + HEDGE_IDLE);
    assert!(h.undecided(b));
    assert_eq!(h.rt.take_parked().len(), 1, "the hedge runner");
    assert_eq!(h.download.policy.deadline(), deadline, "no progress");
    let hedge = h.chunk(b).hedge.unwrap();
    assert_eq!(
        (hedge.fork, hedge.owner_side, hedge.first),
        (3 * MIB / 2, Side::Original, None)
    );
    assert_eq!(h.download.manager.get_active_runners_count(), 3);
    assert_eq!(h.slots(), 3);
}

/// `Legacy` samples no rates, and never hedges.
#[tokio::test]
async fn legacy_never_hedges() {
    let base = Instant::now();
    let legacy = options(4).growth_strategy(GrowthStrategy::Legacy);
    let (mut h, a, b, _ea, _eb) = endgame(&legacy, base);
    assert!(h.download.rates.is_none());
    for n in 1..20 {
        let now = base + TICK * n;
        h.download.manager.note_delivery(a, now);
        h.download.hedge_stalled_runner(now);
    }
    assert!(!h.undecided(b));
    assert_eq!(h.download.manager.get_active_runners_count(), 2);
}

/// A connecting runner is silent from its spawn.
#[tokio::test]
async fn a_connecting_runner_is_silent_from_its_spawn() {
    let base = Instant::now();
    let mut h = Harness::new(3 * MIB, &options(4));
    let (a, _ea) = h.spawn_running(0..3 * MIB / 2, base);
    h.deliver(a, MIB, base);
    let (b, _eb) = h.spawn(3 * MIB / 2..3 * MIB, base + TICK);
    h.deliver(a, KIB, base + TICK + HEDGE_IDLE);
    h.download
        .hedge_stalled_runner(base + TICK + HEDGE_IDLE - NANO);
    assert!(!h.undecided(b));
    h.download.hedge_stalled_runner(base + TICK + HEDGE_IDLE);
    assert!(h.undecided(b));
    assert_eq!(h.chunk(b).status, ChunkStatus::Pending);
}

/// Of two silent runners, the one silent longer is hedged; one per call.
#[tokio::test]
async fn the_runner_silent_longest_is_hedged_first() {
    let base = Instant::now();
    let mut h = Harness::new(4 * MIB, &options(8));
    let (a, _ea) = h.spawn_running(0..MIB, base);
    let (b, _eb) = h.spawn_running(MIB..2 * MIB, base + TICK);
    let (c, _ec) = h.spawn_running(2 * MIB..3 * MIB, base);
    let (d, _ed) = h.spawn_running(3 * MIB..4 * MIB, base);
    // Less than the smallest chunk left: nothing to split.
    h.deliver(b, KIB, base + TICK);
    h.deliver(c, KIB, base);
    let now = base + TICK + HEDGE_IDLE;
    h.deliver(a, KIB, now);
    h.deliver(d, KIB, now);
    h.download.hedge_stalled_runner(now);
    assert_eq!((h.undecided(b), h.undecided(c)), (false, true));
    h.download.hedge_stalled_runner(now);
    assert_eq!((h.undecided(b), h.undecided(c)), (true, true));
}

/// Every condition holds for the runner hedged: the one silent longest
/// may not be hedged while another chunk may still be split, but that one,
/// silent a little less, may be when nothing else may be split.
#[tokio::test]
async fn the_longest_silence_among_runners_that_qualify() {
    let base = Instant::now();
    let mut h = Harness::new(MIB / 2 + 3 * MIB / 2, &options(4));
    // A: 512 KiB left, silent longer; B: 1.5 MiB left, more than the
    // smallest chunk, so splittable while A is the candidate.
    let (a, _ea) = h.spawn_running(0..MIB / 2, base);
    let (b, _eb) = h.spawn_running(MIB / 2..2 * MIB, base + TICK);
    let now = base + TICK + HEDGE_IDLE;
    h.download.hedge_stalled_runner(now);
    assert!(!h.undecided(a));
    assert!(h.undecided(b), "B qualifies: A may not be split");
}

/// Nothing is hedged while a range may be claimed, while no slot is free,
/// at the concurrency limit, or while congested.
#[tokio::test]
async fn no_hedge_without_a_free_connection_or_with_a_range_left() {
    let base = Instant::now();
    let now = base + HEDGE_IDLE;

    // An unassigned range.
    let mut h = Harness::new(4 * MIB, &options(4));
    let (a, _ea) = h.spawn_running(0..3 * MIB / 2, base);
    let (b, _eb) = h.spawn_running(3 * MIB / 2..3 * MIB, base);
    h.deliver(a, MIB, now);
    h.download.hedge_stalled_runner(now);
    assert!(!h.undecided(b));

    // No free slot: reserved meanwhile.
    let (mut h, _, b, _ea, _eb) = endgame(&options(4), base);
    h.download.manager.note_delivery(0, now);
    let held = [
        h.download.manager.try_reserve_slot().unwrap(),
        h.download.manager.try_reserve_slot().unwrap(),
    ];
    h.download.hedge_stalled_runner(now);
    assert!(!h.undecided(b));
    let [first, _second] = held;
    h.download.manager.release(first);
    h.download.hedge_stalled_runner(now);
    assert!(h.undecided(b));

    // The concurrency limit.
    let (mut h, _, b, _ea, _eb) = endgame(&options(2), base);
    h.download.manager.note_delivery(0, now);
    h.download.hedge_stalled_runner(now);
    assert!(!h.undecided(b));

    // Congested until 3 s after `base`.
    let (mut h, _, b, _ea, _eb) = endgame(&options(4), base);
    h.download.manager.note_delivery(0, now);
    assert!(h.download.backoff.hold_congestion(&congested(), 1, base));
    h.download.hedge_stalled_runner(now);
    assert!(!h.undecided(b));
    h.download
        .manager
        .note_delivery(0, base + Duration::from_secs(3));
    h.download
        .hedge_stalled_runner(base + Duration::from_secs(3));
    assert!(h.undecided(b));
}

/// The candidate must have less than twice the smallest chunk left: the
/// slow restart takes the rest (design §9.5).
#[tokio::test]
async fn the_candidate_has_less_than_two_minimum_chunks_left() {
    for (left, hedged) in [(2 * MIN, false), (2 * MIN - 1, true)] {
        let base = Instant::now();
        let now = base + HEDGE_IDLE;
        let mut h = Harness::new(MIB + 2 * MIN, &options(4));
        let (a, _ea) = h.spawn_running(0..MIB, base);
        let (b, _eb) = h.spawn_running(MIB..MIB + 2 * MIN, base);
        h.deliver(a, MIB / 2, now);
        h.deliver(b, 2 * MIN - left, base);
        h.download.hedge_stalled_runner(now);
        assert_eq!(h.undecided(b), hedged, "{left} bytes left");
    }
}

/// Another running chunk may be split when it has at least the planned
/// size of its runner left (design §9.3): its rate × 2 s, here 1.5 MiB.
#[tokio::test]
async fn no_hedge_while_another_chunk_may_be_split() {
    // 1.125 MiB over a tick of 1.5 s: 0.75 MiB/s.
    const OVER_A_TICK: u64 = 9 * MIB / 8;
    for (left, hedged) in [(3 * MIB / 2, false), (3 * MIB / 2 - 1, true)] {
        let base = Instant::now();
        let a_end = OVER_A_TICK + left;
        let mut h = Harness::new(a_end + MIB, &options(4));
        let (a, _ea) = h.spawn_running(0..a_end, base);
        let (b, _eb) = h.spawn_running(a_end..a_end + MIB, base);
        h.download
            .rates
            .as_mut()
            .unwrap()
            .sample(&h.download.manager, base + TICK);
        h.deliver(a, OVER_A_TICK, base + TICK * 2);
        h.download
            .rates
            .as_mut()
            .unwrap()
            .sample(&h.download.manager, base + TICK * 2);
        assert_eq!(
            h.download.planned_split_size(a),
            3 * MIB / 2,
            "the planned size of A"
        );
        h.download.hedge_stalled_runner(base + TICK * 2);
        assert_eq!(h.undecided(b), hedged, "{left} bytes left");
    }
}

/// A connecting runner has no rate yet: its chunk may be split when it has
/// twice the per-runner speed left, even once sampled twice, when its rate
/// is 0 (design §9.6, trigger 1).
#[tokio::test]
async fn a_connecting_chunk_is_judged_by_the_per_runner_speed() {
    for (per_runner_speed, hedged) in [(10.0 * MIB as f64, true), (MIB as f64, false)] {
        let base = Instant::now();
        let now = base + HEDGE_IDLE;
        let mut h = Harness::new(MIB + 5 * MIB + MIB, &options(4));
        let (a, _ea) = h.spawn_running(0..MIB, base);
        let (c, _ec) = h.spawn(MIB..6 * MIB, base);
        let (b, _eb) = h.spawn_running(6 * MIB..7 * MIB, base);
        h.deliver(a, MIB / 2, now);
        h.download.manager.note_delivery(c, now);
        for t in [base, base + TICK] {
            h.download
                .rates
                .as_mut()
                .unwrap()
                .sample(&h.download.manager, t);
        }
        assert_eq!(h.download.rates.as_ref().unwrap().rate(c), Some(0.0));
        h.download.throughput.per_runner_speed = per_runner_speed;
        h.download.hedge_stalled_runner(now);
        assert_eq!(h.undecided(b), hedged, "{per_runner_speed} B/s");
    }
}

/// A runner the slow restart is stopping is no candidate: its range is
/// about to be released.
#[tokio::test]
async fn a_stopping_runner_is_not_hedged() {
    let base = Instant::now();
    let (mut h, a, b, _ea, eb) = endgame(&options(4), base);
    assert!(h.download.manager.stop_runner(b));
    assert!(eb.stop.is_cancelled());
    h.deliver(a, KIB, base + HEDGE_IDLE);
    h.download.hedge_stalled_runner(base + HEDGE_IDLE);
    assert!(!h.undecided(b));
    assert_eq!(h.download.manager.get_active_runners_count(), 2);
}

/// While the writer is full, no runner's data is taken, so none is silent.
/// Once a write completes, every silence starts over: the writer is full
/// before the completion is polled, and can take data again by the time it
/// is handled.
#[tokio::test]
async fn a_full_writer_restarts_every_silence() {
    let base = Instant::now();
    // The writer takes 4 writes in flight and 1 pending.
    let (mut h, a, b, mut ea, _eb) = endgame(&options(4), base);
    for _ in 0..5 {
        ea.send(16 * KIB);
        h.go(base);
    }
    assert!(!h.download.writer.can_write());
    h.download.manager.note_delivery(a, base + HEDGE_IDLE);
    h.download.hedge_stalled_runner(base + HEDGE_IDLE);
    assert!(!h.undecided(b), "the writer is full");

    // One write completes.
    let mut writes = h.rt.take_parked();
    assert_eq!(writes.len(), 4);
    assert!((&mut writes[0]).now_or_never().is_some());
    let recovered = base + HEDGE_IDLE + TICK;
    let event = futures::poll!(futures::future::poll_fn(|cx| h.download.poll_writer(cx)));
    let Poll::Ready(event @ LoopEvent::WriterCompleted(_, true)) = event else {
        panic!("expected the completion of a full writer");
    };
    assert!(h.download.writer.can_write());
    assert!(
        h.download
            .handle_event_at(event, recovered)
            .unwrap()
            .is_continue()
    );
    h.download.manager.note_delivery(a, recovered + TICK);
    h.download
        .hedge_stalled_runner(recovered + HEDGE_IDLE - NANO);
    assert!(!h.undecided(b), "silent since the writer recovered");
    h.download.hedge_stalled_runner(recovered + HEDGE_IDLE);
    assert!(h.undecided(b));
    drop(writes);
}

/// A rejected spawn of the hedge runner is an unretryable connect failure,
/// as for any runner (design §9.6, M4): its slot is reclaimed once, the
/// original keeps its range, and the task ends.
#[tokio::test]
async fn a_rejected_hedge_ends_the_task() {
    let base = Instant::now();
    let (mut h, a, b, _ea, _eb) = endgame(&options(4), base);
    h.rt.set_mode(SpawnMode::Fail);
    h.deliver(a, KIB, base + HEDGE_IDLE);
    h.download.hedge_stalled_runner(base + HEDGE_IDLE);
    assert!(h.undecided(b));
    assert_eq!(h.slots(), 3);
    let error = transfer_error(h.step(base + HEDGE_IDLE));
    assert!(matches!(error, TaskError::Other { .. }), "{error:?}");
    assert_eq!(h.slots(), 2);
    assert!(h.chunk(b).partner().is_none());
    assert_eq!(h.chunk(b).allocated, 3 * MIB / 2..3 * MIB);
    assert_eq!(h.download.manager.get_available_ranges(), []);
}

// ── Arbitration ──────────────────────────────────────────────────────

/// A running runner O on the whole 1.5 MiB file, which delivered `pre`
/// bytes, hedged by H at `now`, running when `started`.
fn pair(pre: u64, started: bool, now: Instant) -> (Harness, RunnerId, Ends, RunnerId, Ends) {
    let mut h = Harness::new(3 * MIB / 2, &options(8));
    let (o, mut eo) = h.spawn_running(0..3 * MIB / 2, now);
    if pre > 0 {
        eo.send(pre);
        h.go(now);
    }
    let (hd, mut eh) = h.hedge(o, now);
    if started {
        eh.start();
        h.go(now);
    }
    assert!(h.undecided(o) && h.undecided(hd));
    (h, o, eo, hd, eh)
}

/// The first non-empty frame decides: the original's, or the hedge's. The
/// other side is stopped, its frames are dropped, and its late events are
/// ignored; its slot is reclaimed once it ends. No byte is written twice.
#[tokio::test]
async fn the_first_frame_decides_the_pair() {
    for hedge_first in [false, true] {
        let now = Instant::now();
        let (mut h, o, eo, hd, eh) = pair(64 * KIB, true, now);
        let (winner, loser, mut ew, mut el) = if hedge_first {
            (hd, o, eh, eo)
        } else {
            (o, hd, eo, eh)
        };
        ew.send(32 * KIB);
        h.go(now);
        assert!(!h.undecided(winner), "hedge first: {hedge_first}");
        assert!(el.stop.is_cancelled() && !ew.stop.is_cancelled());
        assert_eq!(h.chunk(winner).runner_id, Some(winner));
        assert_eq!(h.chunk(winner).downloaded, 0..96 * KIB);
        let side = if hedge_first {
            Side::Hedge
        } else {
            Side::Original
        };
        assert_eq!(h.chunk(winner).hedge.unwrap().owner_side, side);
        assert_eq!(h.download.manager.get_active_runners_count(), 1);
        assert_eq!(h.slots(), 2, "the loser holds its slot");

        // The loser's late frames are dropped, and so is its end.
        el.send(64 * KIB);
        el.end(LifecycleEvent::finished());
        h.drain(now);
        assert_eq!(h.slots(), 1);
        assert!(!h.download.manager.is_stopping(loser));
        assert_eq!(h.chunk(winner).downloaded, 0..96 * KIB);
        assert_eq!(h.download.manager.get_available_ranges(), []);

        // The winner goes on to the end, which ends the hedge (state F).
        ew.send(3 * MIB / 2 - 96 * KIB);
        h.go(now);
        let chunk = h.chunk(winner);
        assert_eq!(
            (chunk.status, chunk.hedge),
            (ChunkStatus::Finished, None),
            "hedge first: {hedge_first}"
        );
        ew.end(LifecycleEvent::finished());
        assert!(h.step(now).unwrap().is_break());
        assert_eq!(h.written(now).len(), 3);
        let covered: u64 = h.written(now).iter().map(|r| r.end - r.start).sum();
        assert_eq!(covered, 3 * MIB / 2);
        assert_eq!(h.slots(), 0);
    }
}

/// When the hedge's first frame is taken while the original still has a
/// non-empty frame queued, which it read before the fork or in the same
/// poll, the original wins: its queued frame is taken and written at once,
/// in order with the rest, and the hedge's is dropped. Empty frames queued
/// before it are skipped.
#[tokio::test]
async fn the_original_wins_a_tie() {
    let now = Instant::now();
    let (mut h, o, mut eo, hd, eh) = pair(0, true, now);
    eo.send(0);
    eo.send(32 * KIB);
    eo.send(16 * KIB);
    h.frame(hd, 32 * KIB, now);
    assert!(!h.undecided(o));
    assert!(eh.stop.is_cancelled());
    assert_eq!(h.chunk(o).downloaded, 0..32 * KIB, "the original's frame");
    h.go(now);
    assert_eq!(h.chunk(o).downloaded, 0..48 * KIB);
    assert_eq!(h.written(now), [0..32 * KIB, 32 * KIB..48 * KIB]);
}

/// Empty frames queued by the original decide nothing: the hedge's
/// non-empty frame wins over them, and is written.
#[tokio::test]
#[expect(clippy::single_range_in_vec_init, reason = "a list holding one range")]
async fn queued_empty_frames_do_not_win_a_tie() {
    let now = Instant::now();
    let (mut h, o, mut eo, hd, eh) = pair(0, true, now);
    eo.send(0);
    eo.send(0);
    h.frame(hd, 32 * KIB, now);
    assert!(eo.stop.is_cancelled() && !eh.stop.is_cancelled());
    assert_eq!(h.chunk(hd).runner_id, Some(hd));
    assert_eq!(h.chunk(hd).downloaded, 0..32 * KIB);
    assert!(h.chunk_of_none(o));
    assert_eq!(h.written(now), [0..32 * KIB]);
}

/// While the original still connects, its frames cannot be taken: the
/// hedge's first frame wins.
#[tokio::test]
async fn a_connecting_original_loses_to_the_first_frame() {
    let now = Instant::now();
    let mut h = Harness::new(MIB, &options(8));
    let (o, mut eo) = h.spawn(0..MIB, now);
    let (hd, mut eh) = h.hedge(o, now);
    eh.start();
    h.go(now);
    eo.send(KIB);
    h.frame(hd, 2 * KIB, now);
    assert_eq!(h.chunk(hd).runner_id, Some(hd));
    assert_eq!(h.chunk(hd).status, ChunkStatus::Running);
    assert!(eo.stop.is_cancelled());
    // Its late `Started` is ignored, and so is its data.
    eo.start();
    h.drain(now);
    assert_eq!(h.chunk(hd).downloaded, 0..2 * KIB);
    assert_eq!(h.slots(), 2);
}

/// An empty frame decides nothing.
#[tokio::test]
async fn an_empty_frame_decides_nothing() {
    let now = Instant::now();
    let (mut h, o, mut eo, hd, mut eh) = pair(64 * KIB, true, now);
    eh.send(0);
    h.go(now);
    eo.send(0);
    h.go(now);
    assert!(h.undecided(o) && h.undecided(hd));
    assert!(!eo.stop.is_cancelled() && !eh.stop.is_cancelled());
}

/// A beaten side's late end is ignored whatever it is: a fallback error,
/// an unretryable error, a congestion or a closed lifecycle neither falls
/// back, ends the task, degrades nor holds connections back. Only its slot
/// is reclaimed, once; its late `Started` reclaims nothing.
#[tokio::test]
async fn a_loser_ends_without_a_trace() {
    let ends: [fn(&mut Ends); 7] = [
        |e| e.end(LifecycleEvent::finished()),
        |e| e.fail(TaskError::Cancelled),
        |e| e.fail(io()),
        |e| e.fail(congested()),
        |e| e.fail(range_ignored()),
        |e| e.fail(refused()),
        |e| {
            let (lifecycle, _) = AsyncHeapRb::<LifecycleEvent>::new(1).split();
            drop(std::mem::replace(&mut e.lifecycle, lifecycle));
        },
    ];
    for (n, end) in ends.into_iter().enumerate() {
        let now = Instant::now();
        // The hedge connects while it loses.
        let (mut h, o, mut eo, hd, mut eh) = pair(64 * KIB, false, now);
        eo.send(KIB);
        h.go(now);
        let sampled = h.evidence();
        eh.start();
        h.drain(now);
        assert_eq!(h.slots(), 2, "case {n}: `Started` reclaims nothing");
        end(&mut eh);
        h.drain(now);
        assert_eq!(h.slots(), 1, "case {n}");
        assert_eq!(h.evidence(), sampled, "case {n}");
        assert_eq!(h.download.backoff.congestion_until(), None, "case {n}");
        assert_eq!(h.download.policy.consecutive(), 0, "case {n}");
        assert_eq!(h.chunk(o).downloaded, 0..65 * KIB, "case {n}");
        assert_eq!(h.download.manager.cooldowns(), [], "case {n}");
        assert!(!h.download.manager.is_stopping(hd), "case {n}");
    }
}

/// The loser ends whenever it does, whatever became of the chunk: its slot
/// is reclaimed once, and the chunk is left alone.
#[tokio::test]
async fn a_loser_ends_whatever_became_of_the_chunk() {
    for case in ["downloading", "failed", "claimed again", "complete"] {
        let now = Instant::now();
        let mut h = Harness::new(3 * MIB, &options(8));
        let (o, mut eo) = h.spawn_running(0..MIB, now);
        let (_other, _eother) = h.spawn_running(MIB..3 * MIB, now);
        let (hd, mut eh) = h.hedge(o, now);
        eo.send(KIB);
        h.go(now);
        assert!(eh.stop.is_cancelled());
        let mut claimed = None;
        match case {
            "downloading" => {}
            "failed" | "claimed again" => {
                eo.fail(io());
                h.go(now);
                assert_eq!(h.download.manager.cooldowns().len(), 1);
                if case == "claimed again" {
                    claimed = Some(h.spawn(KIB..MIB, now));
                }
            }
            "complete" => {
                eo.send(MIB - KIB);
                h.go(now);
                eo.end(LifecycleEvent::finished());
                assert!(h.step(now).unwrap().is_continue());
            }
            _ => unreachable!(),
        }
        let before = (
            h.slots(),
            h.download.manager.chunk_ranges(),
            h.download.manager.cooldowns(),
        );
        eh.fail(TaskError::Cancelled);
        h.go(now);
        assert_eq!(h.slots(), before.0 - 1, "{case}");
        assert_eq!(
            (
                h.download.manager.chunk_ranges(),
                h.download.manager.cooldowns()
            ),
            (before.1, before.2),
            "{case}"
        );
        assert!(!h.download.manager.is_stopping(hd), "{case}");
        drop(claimed);
    }
}

/// A reclaimed ID is a new runner's: the events of a new runner holding
/// the ID of a hedge that failed, or of an original that lost, are its
/// own, and leave the pair alone.
#[tokio::test]
async fn a_reused_id_is_a_new_runner() {
    // The hedge fails first: the pair is left to the original (state C).
    let now = Instant::now();
    let mut h = Harness::new(3 * MIB, &options(8));
    let (o, _eo) = h.spawn_running(0..MIB, now);
    let (hd, mut eh) = h.hedge(o, now);
    eh.fail(io());
    h.go(now);
    let (reused, mut er) = h.spawn(MIB..3 * MIB, now);
    assert_eq!(reused, hd);
    er.start();
    h.go(now);
    er.send(KIB);
    h.go(now);
    assert_eq!(h.chunk(reused).downloaded, MIB..MIB + KIB);
    er.fail(io());
    h.go(now);
    assert!(h.undecided(o));
    assert!(h.chunk(o).hedge.unwrap().first.is_some());
    assert_eq!(h.chunk(o).runner_id, Some(o));

    // The original loses; its ID goes to a new runner.
    let now = Instant::now();
    let mut h = Harness::new(3 * MIB, &options(8));
    let (o, mut eo) = h.spawn_running(0..MIB, now);
    let (hd, mut eh) = h.hedge(o, now);
    eh.start();
    h.go(now);
    eh.send(KIB);
    h.go(now);
    eo.fail(TaskError::Cancelled);
    h.go(now);
    let (reused, mut er) = h.spawn(MIB..3 * MIB, now);
    assert_eq!(reused, o);
    er.start();
    h.go(now);
    er.send(KIB);
    h.go(now);
    assert_eq!(h.chunk(reused).downloaded, MIB..MIB + KIB);
    assert_eq!(h.chunk(hd).downloaded, 0..KIB);
    assert_eq!(h.chunk(hd).runner_id, Some(hd));
}

/// The sequence the split property test shrank to (design §14.18), run to
/// the end: 4 connections, the first runner split twice, then left with
/// a byte under `2 × min_chunk_size`, silent since its spawn 2 s ago,
/// while neither tail is worth splitting, so the tick hedges it. The
/// original then delivers first and wins; the hedge, a real runner
/// parked at its spawn, is stopped and confirms; the original finishes.
/// Every slot is held by a runner and reclaimed once, and the IDs are
/// reused.
#[tokio::test]
async fn a_hedge_after_two_splits_runs_to_the_end() {
    const TOTAL: u64 = 64 * MIB;
    let t0 = Instant::now();
    let mut h = Harness::new(TOTAL, &options(4));
    // The first runner keeps its limits, so that splits can shrink it.
    let slot = h.download.manager.try_reserve_slot().unwrap();
    let o = slot.runner_id;
    let (mut eo, channels) = Ends::new();
    let mut limits = None;
    assert!(
        h.download
            .manager
            .spawn_in(slot, 0..TOTAL, t0, |_, receiver| {
                limits = Some(receiver);
                Ok(channels)
            })
    );
    eo.start();
    h.go(t0);
    let now = t0 + Duration::from_secs(2);
    h.download.apply(StrategyAction::SplitAllTask(0), now);
    for bytes in [5_701_224, 16_462_322, 7_196_583] {
        h.download.manager.update_progress(o, bytes).unwrap();
    }
    h.download.apply(StrategyAction::SplitGivenTask(o), now);
    // The two tails, real runners parked at their spawn; kept alive.
    let tails = h.rt.take_parked();
    assert_eq!(tails.len(), 2);
    assert_eq!(h.slots(), 3);
    assert_eq!(h.chunk(o).remaining(), 2 * MIN - 1);

    let active = h.download.manager.get_active_runners_count();
    h.download.throughput.speed = 50_331_650.0;
    h.download.throughput.per_runner_speed = 50_331_650.0 / active as f64;
    h.download.on_strategy_tick(now);
    assert_eq!(h.slots(), 4, "the hedge holds a slot");
    let hd = (0..4)
        .find(|&id| id != o && h.undecided(id) && h.download.manager.get_runner_state(id).is_none())
        .expect("a hedge without a chunk of its own");
    assert!(h.undecided(o));
    assert_eq!(h.download.manager.get_active_runners_count(), 4);

    // The original delivers first: it wins, the hedge is stopped.
    eo.send(32 * KIB);
    h.go(now);
    assert!(!h.undecided(o) && !h.undecided(hd));
    assert!(h.download.manager.is_stopping(hd));
    assert_eq!(h.slots(), 4, "the loser holds its slot until it confirms");
    assert_eq!(h.download.manager.get_active_runners_count(), 3);

    // The hedge, polled at last, sees its stop and confirms.
    for parked in h.rt.take_parked() {
        tokio::time::timeout(Duration::from_secs(10), parked)
            .await
            .expect("a parked future completes");
    }
    h.drain(now);
    assert_eq!(h.slots(), 3);
    assert!(!h.download.manager.is_stopping(hd));
    assert!(h.chunk_of_none(hd));

    // The original finishes its chunk, which ends the hedge record, and
    // stops; the tails still hold the rest of the file.
    eo.send(2 * MIN - 1 - 32 * KIB);
    h.go(now);
    let chunk = h.chunk(o);
    assert_eq!((chunk.status, chunk.hedge), (ChunkStatus::Finished, None));
    eo.end(LifecycleEvent::finished());
    assert!(h.step(now).unwrap().is_continue());
    assert_eq!(h.slots(), 2);
    assert!(!h.undecided(o) && !h.download.manager.is_stopping(o));

    // Both IDs are free again, for new runners.
    let first = h.download.manager.try_reserve_slot().unwrap();
    let second = h.download.manager.try_reserve_slot().unwrap();
    let mut reused = [first.runner_id, second.runner_id];
    reused.sort_unstable();
    let mut expected = [o, hd];
    expected.sort_unstable();
    assert_eq!(reused, expected);
    assert!(h.download.manager.try_reserve_slot().is_none());
    h.download.manager.release(first);
    h.download.manager.release(second);
    assert_eq!(h.slots(), 2);
    drop(h);
    drop((tails, limits));
}

// ── Errors and budget ────────────────────────────────────────────────

/// One side fails while the other runs on: the range stays with the
/// other, nothing backs off, and the budget waits. A restart is no
/// consecutive failure; a failure without data is one.
#[tokio::test]
async fn one_side_fails_and_the_other_runs_on() {
    // The original delivered before the fork, and times out: a restart.
    let now = Instant::now();
    let (mut h, o, mut eo, hd, _eh) = pair(64 * KIB, true, now);
    eo.fail(TaskError::Timeout);
    assert!(h.step(now).unwrap().is_continue());
    assert_eq!(h.download.policy.consecutive(), 0);
    let chunk = h.chunk(hd);
    assert_eq!(
        (
            chunk.runner_id,
            chunk.downloaded.clone(),
            chunk.attempts,
            chunk.charged
        ),
        (Some(hd), 0..64 * KIB, 0, 0)
    );
    assert_eq!(
        chunk.hedge.unwrap().first,
        Some(SideFailure {
            restart: true,
            charged: false
        })
    );
    assert_eq!(h.download.manager.cooldowns(), []);
    assert_eq!(h.download.manager.get_available_ranges(), []);
    assert!(h.undecided(hd));
    assert_eq!(h.slots(), 1);
    assert!(!h.download.manager.is_stopping(o));

    // The hedge fails without data, after the original delivered.
    let (mut h, o, _eo, _hd, mut eh) = pair(64 * KIB, true, now);
    eh.fail(TaskError::Timeout);
    assert!(h.step(now).unwrap().is_continue());
    assert_eq!(h.download.policy.consecutive(), 1);
    assert_eq!(
        h.chunk(o).hedge.unwrap().first,
        Some(SideFailure {
            restart: false,
            charged: true
        })
    );
    assert_eq!(h.download.manager.cooldowns(), []);
    // The attempt is not hedged again.
    h.download.hedge_stalled_runner(now + HEDGE_IDLE * 10);
    assert_eq!(h.download.manager.get_active_runners_count(), 1);
}

/// A congestion of one side holds every new connection back at once, and
/// feeds the concurrency strategy, without a consecutive failure.
#[tokio::test]
async fn a_congested_side_holds_connections_back() {
    let now = Instant::now();
    let (mut h, o, _eo, _hd, mut eh) = pair(0, false, now);
    eh.fail(congested());
    assert!(h.step(now).unwrap().is_continue());
    assert_eq!(
        h.download.backoff.congestion_until(),
        Some(now + Duration::from_secs(3))
    );
    // Left to the next strategy step, which lowers the limit.
    assert!(h.download.manager.admission().congestion_pending());
    assert_eq!(h.download.policy.consecutive(), 0);
    assert_eq!(h.download.manager.cooldowns(), []);
    assert_eq!(h.chunk(o).runner_id, Some(o));
}

/// A side's fallback error falls back, and any other unretryable error
/// ends the task, as for any runner.
#[tokio::test]
async fn an_unretryable_side_ends_the_task() {
    let now = Instant::now();
    let (mut h, _o, mut eo, _hd, _eh) = pair(0, true, now);
    eo.fail(range_ignored());
    assert!(matches!(
        h.step(now),
        Err(DownloadError::RangeUnsupported { .. })
    ));
    let (mut h, _o, _eo, _hd, mut eh) = pair(0, true, now);
    eh.fail(refused());
    assert!(transfer_error(h.step(now)).is_stream_error());
}

/// A range that failed before, `attempts` and `charged` times, claimed by
/// a runner on the whole 1 MiB file, which is hedged; 8 connections.
fn retried_pair(
    attempts: u32,
    charged: u32,
    options: &TaskOptions,
    now: Instant,
) -> (Harness, RunnerId, Ends, RunnerId, Ends) {
    let mut h = Harness::new(MIB, options);
    h.download
        .manager
        .cool_down(0..MIB, attempts, charged, now - NANO);
    let (o, eo) = h.spawn_running(0..MIB, now);
    assert_eq!(
        (h.chunk(o).attempts, h.chunk(o).charged),
        (attempts, charged)
    );
    let (hd, mut eh) = h.hedge(o, now);
    eh.start();
    h.go(now);
    (h, o, eo, hd, eh)
}

/// The winner inherits the counts of the range, whichever side wins.
#[tokio::test]
async fn the_winner_inherits_the_counts() {
    for hedge_wins in [false, true] {
        let now = Instant::now();
        let (mut h, o, mut eo, hd, mut eh) = retried_pair(3, 2, &options(8), now);
        let winner = if hedge_wins {
            eh.send(KIB);
            hd
        } else {
            eo.send(KIB);
            o
        };
        h.go(now);
        let chunk = h.chunk(winner);
        assert_eq!(
            (chunk.runner_id, chunk.attempts, chunk.charged),
            (Some(winner), 3, 2)
        );
    }
}

/// How a side of a hedged pair fails.
#[derive(Clone, Copy, Debug)]
enum Fail {
    /// A timeout, a restart after the side's own data.
    Timeout,
    Congested,
    Lost,
    /// Deliver 1 KiB first, deciding the pair, then time out.
    TimeoutAfterData,
    /// Deliver 1 KiB first, deciding the pair, then get lost.
    LostAfterData,
}

/// Fail `ends` as `fail` at `now`, and handle it.
fn fail(h: &mut Harness, ends: &mut Ends, fail: Fail, now: Instant) -> Result<ControlFlow<()>> {
    match fail {
        Fail::Timeout => ends.fail(TaskError::Timeout),
        Fail::Congested => ends.fail(congested()),
        Fail::Lost => {
            let (lifecycle, _) = AsyncHeapRb::<LifecycleEvent>::new(1).split();
            drop(std::mem::replace(&mut ends.lifecycle, lifecycle));
        }
        Fail::TimeoutAfterData | Fail::LostAfterData => {
            ends.send(KIB);
            h.go(now);
            let then = match fail {
                Fail::TimeoutAfterData => Fail::Timeout,
                _ => Fail::Lost,
            };
            return self::fail(h, ends, then, now);
        }
    }
    h.step(now)
}

/// The original, which delivered `pre` bytes, fails as `first`, then the
/// hedge as `last`, or the other way round when `hedge_first`; the range
/// had failed `charged` times. Another runner holds a slot when `other`.
/// Returns the harness and how the last failure ended.
fn both_fail(
    pre: u64,
    charged: u32,
    first: Fail,
    last: Fail,
    hedge_first: bool,
    other: bool,
) -> (Harness, Result<ControlFlow<()>>) {
    let now = Instant::now();
    let options = options(8)
        .max_attempts_per_range(NonZeroU32::new(5).unwrap())
        .max_consecutive_failures(100);
    let mut h = Harness::new(3 * MIB, &options);
    h.download
        .manager
        .cool_down(0..MIB, charged + 1, charged, now - NANO);
    let (o, mut eo) = h.spawn_running(0..MIB, now);
    if pre > 0 {
        eo.send(pre);
        h.go(now);
    }
    let _other = other.then(|| h.spawn_running(MIB..3 * MIB, now));
    let (_hd, mut eh) = h.hedge(o, now);
    eh.start();
    h.go(now);
    let (ef, el) = if hedge_first {
        (&mut eh, &mut eo)
    } else {
        (&mut eo, &mut eh)
    };
    assert!(fail(&mut h, ef, first, now).unwrap().is_continue());
    assert_eq!(h.download.manager.cooldowns(), [], "the first failure");
    let result = fail(&mut h, el, last, now);
    (h, result)
}

/// The single cooldown: its range, attempts and charged failures.
fn cooled(h: &Harness) -> (Range<u64>, u32, u32) {
    let cooldowns = h.download.manager.cooldowns();
    let [(range, cooldown)] = cooldowns.as_slice() else {
        panic!("expected one cooldown, got {cooldowns:?}");
    };
    (range.clone(), cooldown.attempts, cooldown.charged)
}

/// Both sides fail: the range backs off once, and the pair is charged at
/// most once, unless either side restarted (design §9.6, budget).
#[tokio::test]
async fn a_pair_is_charged_at_most_once() {
    use Fail::*;
    // (pre, first, last, hedge first, charged, released from)
    let cases = [
        // The original times out without data, then the hedge is
        // congested.
        (0, Timeout, Congested, false, 1, 0),
        // The hedge fails without data, then the original restarts.
        (64 * KIB, Timeout, Timeout, true, 0, 64 * KIB),
        // The original restarts, then the hedge is charged.
        (64 * KIB, Timeout, Timeout, false, 0, 64 * KIB),
        // Both congested.
        (0, Congested, Congested, false, 0, 0),
        // The original is lost after data, then the hedge is congested.
        (64 * KIB, Lost, Congested, false, 1, 64 * KIB),
        // The hedge's own bytes are none: the original delivered, the
        // hedge times out, the original is lost, in both orders.
        (64 * KIB, Timeout, Lost, true, 1, 64 * KIB),
        (64 * KIB, Lost, Timeout, false, 1, 64 * KIB),
        // Both charged.
        (0, Timeout, Lost, false, 1, 0),
    ];
    for (n, (pre, first, last, hedge_first, charged, from)) in cases.into_iter().enumerate() {
        let (h, result) = both_fail(pre, 1, first, last, hedge_first, true);
        assert!(result.unwrap().is_continue(), "case {n}");
        assert_eq!(cooled(&h), (from..MIB, 3, 1 + charged), "case {n}");
    }
}

/// The class of the side that failed first lasts until the attempt ends:
/// the original restarts, then the hedge delivers and is lost. The pair is
/// not charged, even at the edge of the budget; a lost runner alone would
/// be.
#[tokio::test]
async fn the_first_failure_outlives_the_survivors_frame() {
    let (h, result) = both_fail(
        64 * KIB,
        4,
        Fail::Timeout,
        Fail::LostAfterData,
        false,
        false,
    );
    assert!(result.unwrap().is_continue());
    assert_eq!(cooled(&h), (65 * KIB..MIB, 6, 4));
    assert_eq!(h.download.policy.consecutive(), 1);
}

/// The survivor delivered, then fails (state C′): its own bytes count
/// from the fork. A timeout is a restart, which charges nothing; a lost
/// runner is charged, and counts as a consecutive failure.
#[tokio::test]
async fn the_survivor_fails_after_its_own_data() {
    for (last, charged, consecutive) in
        [(Fail::TimeoutAfterData, 0, 1), (Fail::LostAfterData, 1, 2)]
    {
        // The original fails without data, charged.
        let (h, result) = both_fail(0, 1, Fail::Timeout, last, false, false);
        assert!(result.unwrap().is_continue(), "{last:?}");
        assert_eq!(cooled(&h), (KIB..MIB, 3, 1 + charged), "{last:?}");
        assert_eq!(h.download.policy.consecutive(), consecutive, "{last:?}");
    }
}

/// The survivor receives everything (state F, the hedge ended), then
/// reports a lost connection before `Stopped`: nothing is left to retry,
/// so the budget, one failure short, is not exhausted, and the failure only
/// counts as a consecutive one.
#[tokio::test]
async fn a_failure_after_the_last_byte_exhausts_nothing() {
    let now = Instant::now();
    let options = options(8)
        .max_attempts_per_range(NonZeroU32::new(5).unwrap())
        .max_consecutive_failures(100);
    let mut h = Harness::new(3 * MIB, &options);
    h.download.manager.cool_down(0..MIB, 5, 4, now - NANO);
    let (o, mut eo) = h.spawn_running(0..MIB, now);
    let _other = h.spawn_running(MIB..3 * MIB, now);
    let (hd, mut eh) = h.hedge(o, now);
    eh.start();
    h.go(now);
    eo.fail(TaskError::Timeout);
    h.go(now);
    eh.send(MIB);
    h.go(now);
    assert_eq!(h.chunk(hd).hedge, None);
    eh.fail(TaskError::ChannelClosed);
    h.go(now);
    assert_eq!(h.download.manager.cooldowns(), []);
    assert_eq!(h.download.policy.consecutive(), 2);
    assert_eq!(h.download.manager.get_available_ranges(), []);
}

/// At the edge of the budget, the last side's congestion ends the task as
/// the range's exhaustion, whether or not another runner is in flight;
/// one failure short of it, the congestion is left to the strategy. The
/// other way round, a restart and a charged failure charge nothing.
#[tokio::test]
async fn the_budget_of_a_pair_comes_first() {
    for other in [true, false] {
        let (_h, result) = both_fail(0, 4, Fail::Timeout, Fail::Congested, false, other);
        match transfer_error(result) {
            TaskError::RangeExhausted {
                range,
                attempts: 5,
                source,
            } => {
                assert_eq!(range, 0..MIB);
                assert!(is_congestion(&source), "{source:?}");
            }
            other => panic!("expected RangeExhausted, got {other:?}"),
        }
    }
    let (h, result) = both_fail(0, 3, Fail::Timeout, Fail::Congested, false, true);
    assert!(result.unwrap().is_continue());
    assert_eq!(cooled(&h).2, 4);
    assert!(h.download.backoff.congestion_until().is_some());

    let (h, result) = both_fail(64 * KIB, 4, Fail::Timeout, Fail::Timeout, false, false);
    assert!(result.unwrap().is_continue());
    assert_eq!(cooled(&h), (64 * KIB..MIB, 6, 4));
}

/// The last side does not count itself in flight: both sides congested,
/// with the budget left and no other runner, end the task with the
/// congestion.
#[tokio::test]
async fn the_last_side_is_not_in_flight() {
    let (_h, result) = both_fail(0, 0, Fail::Congested, Fail::Congested, false, false);
    assert!(is_congestion(&transfer_error(result)));
}

/// Rounds of pairs that fail without data are charged once a round, until
/// the budget ends the task; pairs that are always congested, while
/// another runner is in flight, are never charged.
#[tokio::test]
async fn rounds_of_failed_pairs() {
    for (fails, exhausted) in [(Fail::Timeout, true), (Fail::Congested, false)] {
        let mut now = Instant::now();
        let options = options(8)
            .max_attempts_per_range(NonZeroU32::new(3).unwrap())
            .max_consecutive_failures(100);
        let mut h = Harness::new(3 * MIB, &options);
        let _other = h.spawn_running(MIB..3 * MIB, now);
        let mut range = 0..MIB;
        for round in 1..=6 {
            let (o, mut eo) = h.spawn_running(range.clone(), now);
            let (_hd, mut eh) = h.hedge(o, now);
            eh.start();
            h.go(now);
            assert!(fail(&mut h, &mut eo, fails, now).unwrap().is_continue());
            let result = fail(&mut h, &mut eh, fails, now);
            if exhausted && round == 3 {
                assert!(matches!(
                    transfer_error(result),
                    TaskError::RangeExhausted { attempts: 3, .. }
                ));
                break;
            }
            assert!(result.unwrap().is_continue(), "{fails:?}, round {round}");
            let (cooled_range, attempts, charged) = cooled(&h);
            let expected = if exhausted { round } else { 0 };
            assert_eq!((attempts, charged), (round, expected), "{fails:?}");
            range = cooled_range;
            now = h.download.manager.next_cooldown_end(now).unwrap();
        }
    }
}

/// Runners the loop stopped are no attempts in flight (design §9.6, M3):
/// a congestion while only a beaten side, or a runner stopped for being
/// slow, holds another slot ends the task.
#[tokio::test]
async fn stopping_runners_are_not_in_flight() {
    for slow in [false, true] {
        let now = Instant::now();
        let mut h = Harness::new(4 * MIB, &options(8));
        let (o, eo) = h.spawn_running(0..MIB, now);
        if slow {
            assert!(h.download.manager.stop_runner(o));
        } else {
            // The hedge wins with the whole chunk, and finishes.
            let (_hd, mut eh) = h.hedge(o, now);
            eh.start();
            h.go(now);
            eh.send(MIB);
            h.go(now);
            eh.end(LifecycleEvent::finished());
            h.go(now);
        }
        assert!(eo.stop.is_cancelled() && h.download.manager.is_stopping(o));
        let (_c, mut ec) = h.spawn_running(3 * MIB..4 * MIB, now);
        assert_eq!(h.slots(), 2, "the stopping runner and the congested one");
        ec.fail(congested());
        assert!(is_congestion(&transfer_error(h.step(now))), "slow {slow}");
    }
}

// ── Exclusions ───────────────────────────────────────────────────────

/// Neither side of an undecided pair is judged slow or split, not even
/// once the original runs; nor is a decided chunk split, whichever side
/// won, and another candidate does not count it as work left.
#[tokio::test]
async fn a_hedged_chunk_is_neither_restarted_nor_split() {
    let now = Instant::now();
    let mut h = Harness::new(3 * MIB, &options(8));
    let (o, mut eo) = h.spawn(0..3 * MIB, now);
    let (hd, mut eh) = h.hedge(o, now);
    eh.start();
    h.go(now);
    eo.start();
    h.go(now);
    assert_eq!(h.chunk(o).status, ChunkStatus::Running);
    assert!(h.download.manager.largest_running_chunk().is_none());
    assert!(!h.download.manager.stop_runner(o));
    assert!(!h.download.split(o, 0..3 * MIB, now));
    assert_eq!(h.chunk(o).allocated, 0..3 * MIB);
    assert!(h.undecided(o) && h.undecided(hd));

    for hedge_wins in [false, true] {
        let now = Instant::now();
        let mut h = Harness::new(5 * MIB, &options(8));
        let (o, mut eo) = h.spawn_running(0..3 * MIB, now);
        let (hd, mut eh) = h.hedge(o, now);
        eh.start();
        h.go(now);
        let winner = if hedge_wins {
            eh.send(KIB);
            hd
        } else {
            eo.send(KIB);
            o
        };
        h.go(now);
        // 3 MiB left, far more than the planned size.
        assert!(h.download.manager.largest_running_chunk().is_none());
        assert!(
            !h.download
                .split(winner, h.chunk(winner).incomplete_range(), now)
        );
        // Another runner, silent on a small chunk, is hedged all the same.
        let (b, _eb) = h.spawn_running(3 * MIB..4 * MIB, now);
        let (c, _ec) = h.spawn_running(4 * MIB..5 * MIB, now + HEDGE_IDLE);
        h.download.manager.note_delivery(winner, now + HEDGE_IDLE);
        h.deliver(c, MIB / 2, now + HEDGE_IDLE);
        h.download.hedge_stalled_runner(now + HEDGE_IDLE);
        assert!(h.undecided(b), "hedge wins: {hedge_wins}");
    }
}

/// While the hedge failed and the original did not deliver yet (state C),
/// the original is not judged slow, nor counted in the median: with it, a
/// slow runner would not be slow enough.
#[tokio::test]
async fn the_survivor_of_an_undecided_pair_is_not_judged() {
    let base = Instant::now();
    let mut h = Harness::new(20 * MIB, &options(8));
    let fast: Vec<_> = (0..3)
        .map(|i| h.spawn_running(i * 6 * MIB..(i + 1) * 6 * MIB, base))
        .collect();
    let (o, _eo) = h.spawn_running(18 * MIB..19 * MIB, base);
    let (_hd, mut eh) = h.hedge(o, base);
    eh.fail(io());
    h.go(base);
    assert!(h.undecided(o));
    // Over the window: 4.67, 4.67 and 0.875 MiB; the original none. The
    // third is below a quarter of the median of the three, but not of the
    // four.
    let per_tick = [MIB * 2 / 3, MIB * 2 / 3, MIB / 8];
    for n in 0..=7 {
        let now = base + TICK * n;
        if n > 0 {
            for ((runner_id, _), bytes) in fast.iter().zip(per_tick) {
                h.download
                    .manager
                    .update_progress(*runner_id, bytes)
                    .unwrap();
            }
        }
        h.download
            .rates
            .as_mut()
            .unwrap()
            .sample(&h.download.manager, now);
        h.download.restart_slow_runner(now);
    }
    assert!(SLOW_WINDOW <= TICK * 7);
    assert!(h.download.manager.is_stopping(fast[2].0));
    assert!(!h.download.manager.is_stopping(o));
}

// ── Timing ───────────────────────────────────────────────────────────

/// Neither the hedge nor the frames of a beaten side move the no-progress
/// deadline; only the winner's accepted bytes do. Undecided, the task ends
/// without progress when the deadline is due.
#[tokio::test]
async fn the_hedge_never_moves_the_no_progress_deadline() {
    let base = Instant::now();
    let mut h = Harness::new(3 * MIB / 2, &options(8));
    let (o, mut eo) = h.spawn_running(0..3 * MIB / 2, base);
    eo.send(64 * KIB);
    h.go(base);
    let deadline = h.download.policy.deadline();
    let (hd, mut eh) = h.hedge(o, base + TICK);
    eh.start();
    h.go(base + TICK);
    assert_eq!(h.download.policy.deadline(), deadline, "the hedge");

    eh.send(KIB);
    h.go(base + TICK * 2);
    let deadline = h.download.policy.deadline();
    assert_eq!(h.chunk(hd).downloaded, 0..65 * KIB);
    // A frame of the beaten original, taken late.
    h.frame(o, 32 * KIB, base + TICK * 3);
    assert_eq!(h.download.policy.deadline(), deadline, "the loser's frame");
    assert_eq!(h.chunk(hd).downloaded, 0..65 * KIB);
    assert_eq!(
        h.written(base + TICK * 3),
        [0..64 * KIB, 64 * KIB..65 * KIB]
    );

    let (mut h, _o, _eo, _hd, _eh) = pair(64 * KIB, true, base);
    let deadline = h.download.policy.deadline();
    assert!(matches!(
        transfer_error(h.download.on_deadline(deadline).map(ControlFlow::Continue)),
        TaskError::NoProgress { .. }
    ));
}

/// A range whose backoff ends while the hedge holds the last slot, and
/// neither side delivers, waits without spinning until a side ends: here
/// the original, which connects late and then times out idle; the
/// no-progress timeout is long, and the hedge lives on without data.
#[tokio::test]
async fn a_backed_off_range_waits_for_a_side_to_end() {
    let base = Instant::now();
    let options = options(3).no_progress_timeout(Duration::from_secs(600));
    let mut h = Harness::new(5 * MIB / 2, &options);
    let (q, _eq) = h.spawn_running(0..MIB / 2, base);
    let (_r, mut er) = h.spawn_running(MIB / 2..MIB, base);
    // The original connects slowly.
    let (o, mut eo) = h.spawn(MIB..5 * MIB / 2, base);
    h.deliver(q, MIB / 4, base);
    // The first step of the strategies, with nothing to create.
    h.download.on_strategy_tick(base);
    assert_eq!(h.slots(), 3);

    let failed = base + HEDGE_IDLE;
    er.fail(io());
    h.go(failed);
    let (cooling, _, _) = cooled(&h);
    let end = h.download.manager.next_cooldown_end(failed).unwrap();
    h.download.manager.note_delivery(q, failed);
    h.download.hedge_stalled_runner(failed);
    assert!(h.undecided(o), "M5: a backed-off range is no work left");
    assert_eq!(h.slots(), 3);

    // The backoff ends: no slot, nothing created, no spinning.
    h.download.on_deadline(end).unwrap();
    assert_eq!(h.slots(), 3);
    assert!(h.download.deadline.at() > end);
    // The original connects just before its connect timeout; still no
    // slot.
    let connected = base + crate::runner::DEFAULT_CONNECT_TIMEOUT - NANO;
    eo.start();
    h.go(connected);
    h.download.retry_backed_off(connected);
    assert_eq!(h.slots(), 3);
    assert_eq!(cooled(&h).0, cooling);
    // A whole idle timeout later, it times out; its slot goes to the range.
    let idle = connected + crate::runner::DEFAULT_IDLE_TIMEOUT;
    h.download.retry_backed_off(idle - NANO);
    assert_eq!(h.slots(), 3);
    eo.fail(TaskError::Timeout);
    assert!(h.step(idle).unwrap().is_continue());
    assert_eq!(h.slots(), 2);
    h.download.retry_backed_off(idle);
    assert_eq!(h.slots(), 3, "the backed-off range is claimed");
    assert_eq!(h.download.manager.cooldowns(), []);
}

// ── Integration ──────────────────────────────────────────────────────

/// Aborts the server task when dropped, also when the test panics.
#[cfg(all(feature = "reqwest", feature = "tokio"))]
struct AbortOnDrop(tokio::task::JoinHandle<()>);

#[cfg(all(feature = "reqwest", feature = "tokio"))]
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Fails the test if `fut` deadlocks; far longer than the test needs.
#[cfg(all(feature = "reqwest", feature = "tokio"))]
async fn with_deadlock_watchdog<T>(fut: impl Future<Output = T>) -> T {
    match futures::future::select(async_io::Timer::after(Duration::from_secs(120)), pin!(fut)).await
    {
        Either::Left(_) => panic!("the event loop never returned"),
        Either::Right((value, _)) => value,
    }
}

/// The fault server stalls the connection of the last chunk halfway,
/// without closing it. The loop, with its real timers, the reqwest adapter
/// and `Doubling`, hedges the stalled runner once it was silent for 2 s:
/// the hedge requests the rest of the chunk, wins, and the file is
/// complete and correct.
///
/// The order is set by the test, not raced. The strategy ticks are held
/// back while the stalled response holds its body at a gate until the first
/// chunk completed, and while the stalled runner's whole buffers are
/// accepted; so the stall is the endgame, and nothing is hedged early. That
/// the hedge did it shows in the loop's state: the stalled runner is beaten
/// while still in flight, which a runner ended by its idle timeout is not;
/// and in the requests: the rest is asked for from where the accepted bytes
/// end.
#[cfg(all(feature = "reqwest", feature = "tokio"))]
#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_connection_is_hedged_in_the_endgame() {
    use bolt_load_adapter::reqwest::IntoReqwestAdapter;
    use bolt_load_tests::adapter::{
        fault::{Gate, RangeFault},
        http_server::create_range_fault_server_with_log,
        simple::{calculate_blake3, create_deterministic_content},
    };

    use super::super::super::runner_manager::StopReason;
    use crate::task::Progress;

    const SIZE: u64 = 3 * MIB / 2;
    const SPLIT: u64 = MIB / 2;
    let dir = tempfile::TempDir::new().unwrap();
    let content = Bytes::from(create_deterministic_content(SIZE as usize));
    let gate = Gate::new();
    let (port, server, log) =
        create_range_fault_server_with_log(content.clone(), Some(gate.clone()))
            .await
            .unwrap();
    let server = AbortOnDrop(server);
    let url = reqwest::Url::parse(&format!(
        "http://127.0.0.1:{port}{}",
        RangeFault::StalledOnce.path()
    ))
    .unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let adapter =
        DynBoltLoadAdapter::new_arc(client.into_reqwest_adapter((reqwest::Method::GET, url)));
    let path = dir.path().join("out.bin");
    let file_writer = FileWriter::new(&path, SIZE).await.unwrap();
    let spawner = RunnerSpawner::new(
        ThreadedRuntimeImpl::new_tokio_rt(),
        adapter,
        WaitGroup::new(),
        CancellationToken::new(),
        crate::runner::DEFAULT_IDLE_TIMEOUT,
    );
    let (reporter, _status) = ProgressReporter::for_test();
    let mut download = DownloadLoop::new(
        SIZE,
        spawner,
        &file_writer,
        reporter,
        &options(4),
        CancellationToken::new(),
    );

    // The runners deliver whole buffers only: the stalled one's last
    // bytes stay in its buffer.
    let accepted = (SIZE - SPLIT) / 2 - crate::runner::BUFFER_SIZE as u64;
    let tail = 1;
    download.spawn_runner(0..SPLIT, Instant::now());
    download.spawn_runner(SPLIT..SIZE, Instant::now());
    let result = with_deadlock_watchdog(async {
        // Without the strategy, the first chunk completes while the last
        // one's body waits at the gate; then the stalled runner's whole
        // buffers are accepted. Nothing may be hedged before.
        drive(&mut download, false, |download| {
            download.manager.get_runner_state(0).is_none()
        })
        .await?;
        gate.release();
        drive(&mut download, false, |download| {
            download
                .manager
                .get_runner_state(tail)
                .is_some_and(|state| state.downloaded == (SPLIT..SPLIT + accepted))
        })
        .await?;
        // With the strategy, the stalled runner is hedged, and beaten while
        // still in flight: not after its idle timeout, which would have
        // ended it as a failure.
        drive(&mut download, true, |download| {
            download.manager.stop_reason(tail) == Some(StopReason::Loser)
        })
        .await?;
        download.run().await
    })
    .await;
    let mut progress = Progress::default();
    let finished = with_deadlock_watchdog(download.finish(result, &mut progress)).await;
    let finalized = file_writer.finalize().await;
    drop(server);

    finished.expect("the download completes");
    finalized.unwrap();
    assert_eq!(progress.downloaded, SIZE);
    assert_eq!(
        calculate_blake3(&std::fs::read(&path).unwrap()),
        calculate_blake3(&content)
    );
    // The last chunk, then its rest from where the accepted bytes end.
    let ranges: Vec<_> = log
        .requests()
        .into_iter()
        .filter_map(|request| request.range)
        .filter(|&(first, _)| first >= SPLIT)
        .collect();
    assert_eq!(
        ranges,
        [(SPLIT, SIZE - 1), (SPLIT + accepted, SIZE - 1)],
        "{ranges:?}"
    );
}

/// Handle the loop's events until `done` holds, without the strategy
/// ticks unless `strategy`; the loop must not end before.
#[cfg(all(feature = "reqwest", feature = "tokio"))]
async fn drive(
    download: &mut DownloadLoop,
    strategy: bool,
    mut done: impl FnMut(&DownloadLoop) -> bool,
) -> Result<()> {
    while !done(download) {
        let event = std::future::poll_fn(|cx| download.poll_event(cx)).await;
        if !strategy && matches!(event, LoopEvent::StrategyTimer) {
            continue;
        }
        if download.handle_event(event)?.is_break() {
            return Err(DownloadError::other(
                "the loop ended before the condition held",
            ));
        }
    }
    Ok(())
}

// ── The connection limit (design §8.3, §14.13a) ──────────────────────

/// Each side of a pair is classified when it fails, with the state of
/// that moment: a refusal above the proven window, beside two proven
/// runners X and Y, is not charged, and a lost side is. The pair's
/// formula is unchanged: charged once when either side is, and not when
/// both are exempt.
#[tokio::test]
async fn each_side_of_a_pair_is_classified_when_it_fails() {
    // (original connecting, hedge fails first, charged)
    for (connecting, hedge_first, charged) in [
        // An exempt refusal of the hedge, then the original lost.
        (false, true, 1),
        // The original lost, then an exempt refusal of the hedge.
        (false, false, 1),
        // Both refused while connecting, both exempt.
        (true, true, 0),
    ] {
        let now = Instant::now();
        let options = options(8).max_consecutive_failures(100);
        let mut h = Harness::new(4 * MIB, &options);
        let (o, mut eo) = if connecting {
            h.spawn(0..MIB, now)
        } else {
            h.spawn_running(0..MIB, now)
        };
        // X and Y prove that the server admits two.
        let (_x, mut ex) = h.spawn_running(MIB..2 * MIB, now);
        let (_y, mut ey) = h.spawn_running(2 * MIB..4 * MIB, now);
        for ends in [&mut ex, &mut ey] {
            ends.send(KIB);
            h.go(now);
        }
        let (_hd, mut eh) = h.hedge(o, now);
        let case = format!("connecting {connecting}, hedge first {hedge_first}");
        let mut end_original = |h: &mut Harness| {
            if connecting {
                eo.fail(io());
            } else {
                let (lifecycle, _) = AsyncHeapRb::<LifecycleEvent>::new(1).split();
                drop(std::mem::replace(&mut eo.lifecycle, lifecycle));
            }
            h.step(now)
        };
        let first = if hedge_first {
            eh.fail(io());
            h.step(now)
        } else {
            end_original(&mut h)
        };
        assert!(first.unwrap().is_continue(), "{case}");
        assert_eq!(
            h.download.manager.cooldowns(),
            [],
            "{case}: the first failure"
        );
        let last = if hedge_first {
            end_original(&mut h)
        } else {
            eh.fail(io());
            h.step(now)
        };
        assert!(last.unwrap().is_continue(), "{case}");
        // One attempt for the pair.
        assert_eq!(cooled(&h), (0..MIB, 1, charged), "{case}");
    }
}

/// The hedge opens a connection only below the limit controller's cap: at
/// a limit lowered to the running runners, none; once a recovery step
/// allows one more, the trial's slot; but not once a refusal marked the
/// step, before its tick decides it.
#[tokio::test]
async fn a_hedge_opens_no_connection_above_the_limit() {
    use super::super::super::strategy::AdmissionClass;

    let base = Instant::now();
    let (mut h, a, b, _ea, _eb) = endgame(&options(4), base);
    // A 503 beside the two lowers the limit to them at the next tick.
    h.download
        .manager
        .admission_mut()
        .on_attempt_ended(AdmissionClass::Congestion, 2, 2);
    let mut now = base;
    let tick = |h: &mut Harness, now: Instant| {
        // Both deliver, so that neither is silent.
        h.download.manager.note_delivery(a, now);
        h.download.manager.note_delivery(b, now);
        h.download.on_strategy_tick(now);
    };
    // The first step only creates runners; the next takes the 503.
    tick(&mut h, now);
    now += TICK;
    tick(&mut h, now);
    assert_eq!(h.download.max_concurrency, 2);
    // Silent B, at the lowered limit: no hedge.
    h.download.hedge_stalled_runner(now + HEDGE_IDLE);
    assert!(!h.undecided(b));
    // Up to the recovery step, 30 s after the lowering.
    while h.download.strategy.limit_state().1.is_none() {
        now += TICK;
        tick(&mut h, now);
        assert!(now < base + Duration::from_secs(40), "no step began");
    }
    assert_eq!(h.download.max_concurrency, 3);
    // An uncorroborated refusal marks the step: no hedge before its tick.
    h.download
        .manager
        .admission_mut()
        .on_attempt_ended(AdmissionClass::Refused, 0, 2);
    h.download.manager.note_delivery(a, now + HEDGE_IDLE);
    h.download.hedge_stalled_runner(now + HEDGE_IDLE);
    assert!(!h.undecided(b));
    assert_eq!(h.download.manager.get_active_runners_count(), 2);
}

/// A hedge handed the chunk after the original failed is not proven by
/// the original's bytes: proof is the owner's own accepted bytes on this
/// attempt (design §8.3).
#[tokio::test]
async fn a_hedge_handed_the_chunk_is_not_proven_by_the_originals_bytes() {
    let now = Instant::now();
    let (mut h, _o, mut eo, hd, _eh) = pair(64 * KIB, true, now);
    assert_eq!(h.download.manager.get_proven_runners_count(), 1);
    // The original times out after its bytes: the hedge runs on alone.
    eo.fail(TaskError::Timeout);
    assert!(h.step(now).unwrap().is_continue());
    assert_eq!(h.chunk(hd).runner_id, Some(hd));
    assert_eq!(h.chunk(hd).downloaded, 0..64 * KIB);
    assert_eq!(h.download.manager.get_proven_runners_count(), 0);
    // Its own first bytes prove it.
    h.frame(hd, KIB, now);
    assert_eq!(h.download.manager.get_proven_runners_count(), 1);
}

/// A beaten side that reports `Started` late holds a connection until it
/// ends: it is connected for the admission evidence, then released.
#[tokio::test]
async fn a_beaten_side_started_late_is_connected_until_it_ends() {
    let now = Instant::now();
    let mut h = Harness::new(MIB, &options(8));
    let (o, mut eo) = h.spawn(0..MIB, now);
    let (hd, mut eh) = h.hedge(o, now);
    eh.start();
    h.go(now);
    h.frame(hd, 2 * KIB, now);
    assert!(eo.stop.is_cancelled(), "the connecting original lost");
    assert!(!h.download.manager.admission().is_connected(o));
    eo.start();
    h.drain(now);
    assert!(h.download.manager.admission().is_connected(o));
    assert_eq!(h.download.manager.admission().connected(), 2);
    eo.fail(TaskError::Cancelled);
    h.drain(now);
    assert!(!h.download.manager.admission().is_connected(o));
    assert_eq!(h.slots(), 1);
}
