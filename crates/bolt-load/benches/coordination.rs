//! Coordination-strategy benchmark for the concurrent download hot path.
//!
//! Compares the single-owner "event loop" design used by `ConcurrentTaskInner::download_to`
//! against shared-state alternatives. The model is self-contained: it reimplements the
//! per-frame bookkeeping of `RunnerManager::handle_data_frame` / `ChunkPlanner` and the
//! periodic sampler (`throughout_meter_tick`) instead of depending on private internals.
//!
//! Variants (all producers run on a tokio multi-thread runtime, one task per runner):
//! - `event_loop_inline`: per-runner `async-ringbuf` SPSC (cap 32) data + lifecycle
//!   channels, merged with `futures_concurrency::StreamGroup`, consumed by one loop
//!   (`select_biased!` over lifecycle > data, plus a sampler timer). The sink is called
//!   inline on the consumer.
//! - `event_loop_dispatch`: same, but writes go through a `PendingWriter`-like
//!   dispatcher: one spawned task per write, in-flight cap = available parallelism,
//!   completions over a bounded `async_channel`; the loop stops reading data when full.
//! - `std_mutex`: `Arc<std::sync::Mutex<State>>`, each runner locks per frame, updates
//!   the same state, unlocks and writes itself.
//! - `async_mutex`: same with `async_lock::Mutex`.
//! - `sharded_atomics`: per-runner atomics (downloaded end + meter); no shared lock on the
//!   hot path; the sampler aggregates.
//!
//! Every variant runs the same sampler every `SAMPLE_INTERVAL`: drain meters, sum them,
//! collect + sort + merge downloaded ranges.
//!
//! Run with: `cargo bench -p bolt-load --bench coordination`
//!
//! CPU/RSS profiling mode (bypasses criterion), e.g.:
//! `COORD_PROFILE=std_mutex:16:32768:8 /usr/bin/time -l target/release/deps/coordination-<hash>`
//! (`variant:runners:frame_size:iterations[:write_cost_ns]`).

use std::{
    collections::{BTreeMap, HashMap},
    future::{IntoFuture, poll_fn},
    hint::black_box,
    ops::{ControlFlow, Range},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    task::{Context, Poll, ready},
    time::{Duration, Instant},
};

use async_ringbuf::{AsyncHeapCons, AsyncHeapProd, AsyncHeapRb, traits::*};
use bolt_load::runtime::{DynTimer, Timer, TimerImpl, TokioDelayedTimer};
use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use futures::{FutureExt, Stream, StreamExt, future::Fuse};
use futures_concurrency::{future::FutureGroup, stream::StreamGroup};
use tokio::runtime::Runtime;

type RunnerId = u64;

/// Total bytes per download iteration.
const TOTAL: u64 = 256 * 1024 * 1024;
/// Mirrors `runner::DATA_FRAME_CHANNEL_CAPACITY`.
const DATA_FRAME_CHANNEL_CAPACITY: usize = 32;
/// Mirrors `runner::LIFECYCLE_CHANNEL_CAPACITY`.
const LIFECYCLE_CHANNEL_CAPACITY: usize = 2;
/// Sampler / strategy cadence (real code: 250 ms sampler, 1.5 s strategy; use the
/// more aggressive 100 ms here).
const SAMPLE_INTERVAL: Duration = Duration::from_millis(100);

// ───────────────────────────── shared model ─────────────────────────────

/// Mirrors `ChunkState` (the fields touched on the hot path).
struct ChunkState {
    allocated: Range<u64>,
    downloaded: Range<u64>,
    finished: bool,
}

/// Mirrors `ChunkPlanner` + the `meters` map owned by `download_to`.
struct State {
    chunks: HashMap<(u64, u64), ChunkState>,
    runner_to_chunk: HashMap<RunnerId, (u64, u64)>,
    meters: HashMap<RunnerId, usize>,
    last_accepted: u64,
}

impl State {
    fn new(ranges: &[Range<u64>]) -> Self {
        let mut chunks = HashMap::with_capacity(ranges.len());
        let mut runner_to_chunk = HashMap::with_capacity(ranges.len());
        for (id, r) in ranges.iter().enumerate() {
            let key = (r.start, r.end);
            chunks.insert(
                key,
                ChunkState {
                    allocated: r.clone(),
                    downloaded: r.start..r.start,
                    finished: false,
                },
            );
            runner_to_chunk.insert(id as RunnerId, key);
        }
        Self {
            chunks,
            runner_to_chunk,
            meters: HashMap::with_capacity(ranges.len()),
            last_accepted: 0,
        }
    }

    /// Mirrors `RunnerManager::handle_data_frame` + `failure_policy.on_bytes_accepted(
    /// runner_manager.get_total_downloaded(), ..)` (an O(chunks) sum per frame).
    fn on_frame(&mut self, runner_id: RunnerId, len: usize) -> Range<u64> {
        *self.meters.entry(runner_id).or_insert(0) += len;
        let key = self.runner_to_chunk[&runner_id];
        let chunk = self.chunks.get_mut(&key).expect("chunk");
        let new_end = (chunk.downloaded.end + len as u64).min(chunk.allocated.end);
        let prev = std::mem::replace(&mut chunk.downloaded.end, new_end);
        if chunk.downloaded.end >= chunk.allocated.end {
            chunk.finished = true;
        }
        let total: u64 = self
            .chunks
            .values()
            .map(|c| c.downloaded.end - c.downloaded.start)
            .sum();
        if total > self.last_accepted {
            self.last_accepted = total;
        }
        prev..new_end
    }

    /// Mirrors `throughout_meter_tick`: drain meters, then `get_downloaded_ranges`.
    fn sample(&mut self) -> (usize, u64) {
        let speed = self.meters.values_mut().map(std::mem::take).sum::<usize>();
        let ranges = self
            .chunks
            .values()
            .filter(|c| c.downloaded.start < c.downloaded.end)
            .map(|c| c.downloaded.clone())
            .collect();
        (speed, merged_len(ranges))
    }
}

fn merged_len(mut ranges: Vec<Range<u64>>) -> u64 {
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<Range<u64>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match merged.last_mut() {
            Some(last) if last.end >= r.start => last.end = last.end.max(r.end),
            _ => merged.push(r),
        }
    }
    merged.iter().map(|r| r.end - r.start).sum()
}

fn split_ranges(total: u64, n: usize) -> Vec<Range<u64>> {
    let step = total / n as u64;
    (0..n as u64)
        .map(|i| {
            let start = i * step;
            let end = if i == n as u64 - 1 {
                total
            } else {
                start + step
            };
            start..end
        })
        .collect()
}

/// The "write" sink: touches the bytes cheaply, optionally burns `cost_ns` of CPU to
/// simulate a write syscall / memcpy into an mmap.
#[inline]
fn sink(range: &Range<u64>, data: &Bytes, cost_ns: u64) -> u64 {
    let mut acc = range.start ^ data.len() as u64;
    if let (Some(a), Some(b)) = (data.first(), data.last()) {
        acc = acc.wrapping_add(*a as u64 + *b as u64);
    }
    if cost_ns > 0 {
        let start = Instant::now();
        while start.elapsed().as_nanos() < cost_ns as u128 {
            std::hint::spin_loop();
        }
    }
    black_box(acc)
}

/// Producer frame source: pre-allocated `Bytes`, cloned per frame (no network, no copy).
/// Yields at each frame boundary to stand in for the network `stream.next()` await.
struct FrameSource {
    frame: Bytes,
    remaining: u64,
}

impl FrameSource {
    fn new(frame: &Bytes, range: &Range<u64>) -> Self {
        Self {
            frame: frame.clone(),
            remaining: range.end - range.start,
        }
    }

    async fn next(&mut self) -> Option<Bytes> {
        if self.remaining == 0 {
            return None;
        }
        tokio::task::yield_now().await;
        let len = (self.frame.len() as u64).min(self.remaining);
        self.remaining -= len;
        Some(self.frame.slice(..len as usize))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Variant {
    EventLoopInline,
    EventLoopDispatch,
    StdMutex,
    AsyncMutex,
    ShardedAtomics,
}

impl Variant {
    const ALL: [Variant; 5] = [
        Variant::EventLoopInline,
        Variant::EventLoopDispatch,
        Variant::StdMutex,
        Variant::AsyncMutex,
        Variant::ShardedAtomics,
    ];

    fn name(self) -> &'static str {
        match self {
            Variant::EventLoopInline => "event_loop_inline",
            Variant::EventLoopDispatch => "event_loop_dispatch",
            Variant::StdMutex => "std_mutex",
            Variant::AsyncMutex => "async_mutex",
            Variant::ShardedAtomics => "sharded_atomics",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.name() == s)
    }
}

#[derive(Clone, Copy)]
struct Config {
    runners: usize,
    frame_size: usize,
    write_cost_ns: u64,
}

async fn run(variant: Variant, cfg: Config) -> u64 {
    let ranges = split_ranges(TOTAL, cfg.runners);
    let frame = Bytes::from(vec![0xABu8; cfg.frame_size]);
    let downloaded = match variant {
        Variant::EventLoopInline => event_loop(&ranges, &frame, cfg, false).await,
        Variant::EventLoopDispatch => event_loop(&ranges, &frame, cfg, true).await,
        Variant::StdMutex => std_mutex(&ranges, &frame, cfg).await,
        Variant::AsyncMutex => async_mutex(&ranges, &frame, cfg).await,
        Variant::ShardedAtomics => sharded(&ranges, &frame, cfg).await,
    };
    assert_eq!(downloaded, TOTAL, "{variant:?} lost bytes");
    downloaded
}

// ───────────────────────────── A: event loop ─────────────────────────────

enum Lifecycle {
    Started,
    Finished,
}

/// Mirrors `RunnerTaggedStream`: tag each item with its runner id.
fn tag<S: futures::Stream>(id: RunnerId, s: S) -> impl futures::Stream<Item = (RunnerId, S::Item)> {
    s.map(move |item| (id, item))
}

async fn event_loop(ranges: &[Range<u64>], frame: &Bytes, cfg: Config, dispatch: bool) -> u64 {
    let mut state = State::new(ranges);
    let mut lifecycle_group = StreamGroup::new();
    let mut data_group = StreamGroup::new();
    let mut producers = Vec::with_capacity(ranges.len());

    for (id, range) in ranges.iter().enumerate() {
        let (mut lc_tx, lc_rx): (AsyncHeapProd<Lifecycle>, AsyncHeapCons<Lifecycle>) =
            AsyncHeapRb::new(LIFECYCLE_CHANNEL_CAPACITY).split();
        let (mut data_tx, data_rx): (AsyncHeapProd<Bytes>, AsyncHeapCons<Bytes>) =
            AsyncHeapRb::new(DATA_FRAME_CHANNEL_CAPACITY).split();
        lifecycle_group.insert(tag(id as RunnerId, lc_rx));
        data_group.insert(tag(id as RunnerId, data_rx));
        let mut src = FrameSource::new(frame, range);
        producers.push(tokio::spawn(async move {
            let _ = lc_tx.push(Lifecycle::Started).await;
            while let Some(f) = src.next().await {
                if data_tx.push(f).await.is_err() {
                    return;
                }
            }
            // The real runner waits for the data channel to drain before `Finished`.
            data_tx.wait_vacant(DATA_FRAME_CHANNEL_CAPACITY).await;
            let _ = lc_tx.push(Lifecycle::Finished).await;
        }));
    }

    // PendingWriter-like dispatcher.
    let capacity = std::thread::available_parallelism().map_or(4, |n| n.get());
    let (done_tx, done_rx) = async_channel::bounded::<Range<u64>>(capacity + 1);
    let mut in_flight = 0usize;

    let mut sampler = tokio::time::interval(SAMPLE_INTERVAL);
    sampler.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Probe knobs: COORD_BATCH=k drains up to k extra ready frames per wake-up;
    // COORD_NO_TIMER=1 polls the sampler deadline with `Instant` instead of a timer future.
    let batch: usize = std::env::var("COORD_BATCH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let no_timer = std::env::var("COORD_NO_TIMER").is_ok();
    let mut next_sample = Instant::now() + SAMPLE_INTERVAL;
    let mut finished = 0usize;
    let mut data_done = false;
    let mut checksum = 0u64;

    enum Ev {
        Written,
        Sample,
        Lifecycle(Option<(RunnerId, Lifecycle)>),
        Data(Option<(RunnerId, Bytes)>),
    }

    loop {
        while let Ok(_r) = done_rx.try_recv() {
            in_flight -= 1;
        }
        let can_write = in_flight < capacity;
        let ev = {
            let written = if in_flight == 0 {
                futures::future::Either::Left(futures::future::pending())
            } else {
                futures::future::Either::Right(done_rx.recv())
            }
            .fuse();
            let tick = if no_timer {
                futures::future::Either::Left(futures::future::pending::<tokio::time::Instant>())
            } else {
                futures::future::Either::Right(sampler.tick())
            }
            .fuse();
            let lc = lifecycle_group.next().fuse();
            let data = if can_write && !data_done {
                futures::future::Either::Left(data_group.next())
            } else {
                futures::future::Either::Right(futures::future::pending())
            }
            .fuse();
            futures::pin_mut!(written, tick, lc, data);
            futures::select_biased! {
                r = written => {
                    r.expect("completion channel");
                    Ev::Written
                }
                _ = tick => Ev::Sample,
                l = lc => Ev::Lifecycle(l),
                d = data => Ev::Data(d),
            }
        };
        if no_timer && Instant::now() >= next_sample {
            next_sample += SAMPLE_INTERVAL;
            black_box(state.sample());
        }
        let mut pending_frames = Vec::new();
        if let Ev::Data(Some(_)) = &ev {
            for _ in 0..batch {
                match data_group.next().now_or_never() {
                    Some(Some(f)) => pending_frames.push(f),
                    _ => break,
                }
            }
        }
        for (id, mut bytes) in pending_frames {
            let range = state.on_frame(id, bytes.len());
            bytes.truncate((range.end - range.start) as usize);
            checksum = checksum.wrapping_add(sink(&range, &bytes, cfg.write_cost_ns));
        }
        match ev {
            Ev::Written => in_flight -= 1,
            Ev::Sample => {
                black_box(state.sample());
            }
            Ev::Lifecycle(Some((_, Lifecycle::Started))) => {}
            Ev::Lifecycle(Some((_, Lifecycle::Finished))) => {
                finished += 1;
                if finished == ranges.len() {
                    break;
                }
            }
            Ev::Lifecycle(None) => {}
            // All data streams closed (the real `DataAggregator` parks instead).
            Ev::Data(None) => data_done = true,
            Ev::Data(Some((id, mut bytes))) => {
                let range = state.on_frame(id, bytes.len());
                bytes.truncate((range.end - range.start) as usize);
                if dispatch {
                    in_flight += 1;
                    let tx = done_tx.clone();
                    let cost = cfg.write_cost_ns;
                    tokio::spawn(async move {
                        black_box(sink(&range, &bytes, cost));
                        let _ = tx.send(range).await;
                    });
                } else {
                    checksum = checksum.wrapping_add(sink(&range, &bytes, cfg.write_cost_ns));
                }
            }
        }
    }
    while in_flight > 0 {
        done_rx.recv().await.expect("completion channel");
        in_flight -= 1;
    }
    for p in producers {
        p.await.expect("producer");
    }
    black_box(checksum);
    state.sample().1
}

// ───────────────────────────── B/C: shared mutex ─────────────────────────────

async fn std_mutex(ranges: &[Range<u64>], frame: &Bytes, cfg: Config) -> u64 {
    let state = Arc::new(std::sync::Mutex::new(State::new(ranges)));
    let reader = {
        let state = state.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(SAMPLE_INTERVAL);
            loop {
                iv.tick().await;
                black_box(state.lock().unwrap().sample());
            }
        })
    };
    let producers: Vec<_> = ranges
        .iter()
        .enumerate()
        .map(|(id, range)| {
            let state = state.clone();
            let mut src = FrameSource::new(frame, range);
            tokio::spawn(async move {
                let mut acc = 0u64;
                while let Some(mut f) = src.next().await {
                    let r = state.lock().unwrap().on_frame(id as RunnerId, f.len());
                    f.truncate((r.end - r.start) as usize);
                    acc = acc.wrapping_add(sink(&r, &f, cfg.write_cost_ns));
                }
                acc
            })
        })
        .collect();
    for p in producers {
        black_box(p.await.expect("producer"));
    }
    reader.abort();
    state.lock().unwrap().sample().1
}

async fn async_mutex(ranges: &[Range<u64>], frame: &Bytes, cfg: Config) -> u64 {
    let state = Arc::new(async_lock::Mutex::new(State::new(ranges)));
    let reader = {
        let state = state.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(SAMPLE_INTERVAL);
            loop {
                iv.tick().await;
                black_box(state.lock().await.sample());
            }
        })
    };
    let producers: Vec<_> = ranges
        .iter()
        .enumerate()
        .map(|(id, range)| {
            let state = state.clone();
            let mut src = FrameSource::new(frame, range);
            tokio::spawn(async move {
                let mut acc = 0u64;
                while let Some(mut f) = src.next().await {
                    let r = state.lock().await.on_frame(id as RunnerId, f.len());
                    f.truncate((r.end - r.start) as usize);
                    acc = acc.wrapping_add(sink(&r, &f, cfg.write_cost_ns));
                }
                acc
            })
        })
        .collect();
    for p in producers {
        black_box(p.await.expect("producer"));
    }
    reader.abort();
    state.lock().await.sample().1
}

// ───────────────────────────── D: sharded atomics ─────────────────────────────

struct Shard {
    allocated: Range<u64>,
    downloaded_end: AtomicU64,
    meter: AtomicUsize,
}

struct Sharded {
    shards: Vec<Shard>,
    total: AtomicU64,
}

impl Sharded {
    fn sample(&self) -> (usize, u64) {
        let speed = self
            .shards
            .iter()
            .map(|s| s.meter.swap(0, Ordering::Relaxed))
            .sum::<usize>();
        let ranges = self
            .shards
            .iter()
            .map(|s| s.allocated.start..s.downloaded_end.load(Ordering::Acquire))
            .filter(|r| r.start < r.end)
            .collect();
        (speed, merged_len(ranges))
    }
}

async fn sharded(ranges: &[Range<u64>], frame: &Bytes, cfg: Config) -> u64 {
    let state = Arc::new(Sharded {
        shards: ranges
            .iter()
            .map(|r| Shard {
                allocated: r.clone(),
                downloaded_end: AtomicU64::new(r.start),
                meter: AtomicUsize::new(0),
            })
            .collect(),
        total: AtomicU64::new(0),
    });
    let reader = {
        let state = state.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(SAMPLE_INTERVAL);
            loop {
                iv.tick().await;
                black_box(state.sample());
            }
        })
    };
    let producers: Vec<_> = ranges
        .iter()
        .enumerate()
        .map(|(id, range)| {
            let state = state.clone();
            let mut src = FrameSource::new(frame, range);
            tokio::spawn(async move {
                let shard = &state.shards[id];
                let mut acc = 0u64;
                while let Some(mut f) = src.next().await {
                    // Single writer per shard: load + store is enough.
                    let prev = shard.downloaded_end.load(Ordering::Relaxed);
                    let new_end = (prev + f.len() as u64).min(shard.allocated.end);
                    shard.downloaded_end.store(new_end, Ordering::Release);
                    shard.meter.fetch_add(f.len(), Ordering::Relaxed);
                    // Global "progress made" counter replaces the O(chunks) sum.
                    state.total.fetch_add(new_end - prev, Ordering::Relaxed);
                    f.truncate((new_end - prev) as usize);
                    acc = acc.wrapping_add(sink(&(prev..new_end), &f, cfg.write_cost_ns));
                }
                acc
            })
        })
        .collect();
    for p in producers {
        black_box(p.await.expect("producer"));
    }
    reader.abort();
    assert_eq!(state.total.load(Ordering::Relaxed), TOTAL);
    state.sample().1
}

// ───────────────────────────── E: download loop shape ─────────────────────────────
//
// Mirrors one `DownloadLoop` attempt: cancel token, no-progress deadline
// (`async_io::Timer`), writer completions, strategy + throughput timers, and the
// runner lifecycle / pending / data streams, selected with the same priority
// (cancel > deadline > writer > strategy > throughput > runner; lifecycle > pending >
// data). Shapes:
// - `rebuilt_select`: today's loop. Every iteration drains completions first, then
//   rebuilds the cancel listener, both timer `tick()` futures, the writer `recv()`
//   and the runner `select_biased!`.
// - `persistent_select`: every source lives across iterations and is polled through
//   `poll_*` fns, still selected with `select_biased!`.
// - `direct_poll`: the same `poll_*` fns called in priority order from one `poll_fn`.
// Writers: `spawn_oneshot` (today: a task per write, a command with a oneshot reply,
// then a completion send) vs `direct_completion` (the worker sends the completion).

/// Bytes per loop iteration.
const LOOP_TOTAL: u64 = 64 * 1024 * 1024;
/// Shortened cadences (real: 250 ms / 1.5 s), so every iteration sees many ticks.
const LOOP_THROUGHPUT_INTERVAL: Duration = Duration::from_millis(1);
const LOOP_STRATEGY_INTERVAL: Duration = Duration::from_millis(2);
/// Mirrors `DEFAULT_NO_PROGRESS_TIMEOUT`.
const LOOP_NO_PROGRESS: Duration = Duration::from_secs(60);
/// Mirrors `FILE_WRITER_QUEUE_SIZE`.
const WRITER_QUEUE: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoopShape {
    RebuiltSelect,
    PersistentSelect,
    DirectPoll,
}

impl LoopShape {
    const ALL: [LoopShape; 3] = [
        LoopShape::RebuiltSelect,
        LoopShape::PersistentSelect,
        LoopShape::DirectPoll,
    ];

    fn name(self) -> &'static str {
        match self {
            LoopShape::RebuiltSelect => "rebuilt_select",
            LoopShape::PersistentSelect => "persistent_select",
            LoopShape::DirectPoll => "direct_poll",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TimerKind {
    Tokio,
    /// `SmolDelayedTimer`: an `async_io::Timer` re-armed after each tick.
    Smol,
    /// `TimerImpl::Custom`: a boxed future per `tick()`.
    Custom,
}

impl TimerKind {
    const ALL: [TimerKind; 3] = [TimerKind::Tokio, TimerKind::Smol, TimerKind::Custom];

    fn name(self) -> &'static str {
        match self {
            TimerKind::Tokio => "tokio",
            TimerKind::Smol => "smol",
            TimerKind::Custom => "custom",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriteMode {
    SpawnOneshot,
    DirectCompletion,
}

impl WriteMode {
    fn name(self) -> &'static str {
        match self {
            WriteMode::SpawnOneshot => "spawn_oneshot",
            WriteMode::DirectCompletion => "direct_completion",
        }
    }
}

/// When the loop stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    /// After every runner finished.
    Complete,
    /// On cancellation, `after` the first frame.
    CancelAfter(Duration),
    /// On the no-progress deadline, `after` the first frame (progress is ignored).
    DeadlineAfter(Duration),
}

#[derive(Clone, Copy, Debug)]
struct LoopConfig {
    shape: LoopShape,
    timer: TimerKind,
    write: WriteMode,
    runners: usize,
    frame_size: usize,
    /// Bytes per loop run.
    total: u64,
    /// In-flight write limit (`PendingWriter::capacity`).
    capacity: usize,
    /// Backend command queue capacity.
    queue: usize,
    write_cost_ns: u64,
    /// Odd runners connect 1 ms late, so connections complete under load.
    stagger_connect: bool,
    stop: Stop,
}

impl LoopConfig {
    fn new(shape: LoopShape, timer: TimerKind, runners: usize, frame_size: usize) -> Self {
        Self {
            shape,
            timer,
            write: WriteMode::SpawnOneshot,
            runners,
            frame_size,
            total: LOOP_TOTAL,
            capacity: std::thread::available_parallelism().map_or(4, |n| n.get()),
            queue: WRITER_QUEUE,
            write_cost_ns: 0,
            stagger_connect: false,
            stop: Stop::Complete,
        }
    }

    /// A one-slot backend queue below the in-flight limit with slow writes, so the
    /// direct path must fall back.
    fn queue_full(self) -> Self {
        Self {
            capacity: 8,
            queue: 1,
            write_cost_ns: 5_000,
            stagger_connect: true,
            ..self
        }
    }
}

/// A user timer passed as `TimerImpl::Custom`: an `async_io::Timer` re-armed per tick.
struct CustomTimer {
    timer: async_io::Timer,
    period: Duration,
}

impl Timer for CustomTimer {
    async fn tick(&mut self) {
        (&mut self.timer).await;
        self.timer.set_after(self.period);
    }
}

/// `rebuilt_select` ticks the crate's own `TimerImpl`; the persistent shapes poll the
/// planned `EventTimer`: native `poll_tick` for tokio / smol, one long-lived stream
/// over the `TimerImpl` for custom timers.
enum LoopTimer {
    Rebuilt(TimerImpl),
    /// Without the `smol` feature `SmolDelayedTimer` is unavailable; this mirrors its
    /// `SmolDelayedTick` (poll the timer, then `set_after`).
    #[cfg(not(feature = "smol"))]
    RebuiltSmol {
        timer: async_io::Timer,
        period: Duration,
    },
    Tokio(tokio::time::Interval),
    Smol {
        timer: async_io::Timer,
        period: Duration,
    },
    CustomStream(Pin<Box<dyn Stream<Item = ()> + Send>>),
}

impl LoopTimer {
    fn new(kind: TimerKind, period: Duration, persistent: bool) -> Self {
        let custom = || {
            TimerImpl::Custom(DynTimer::new_box(CustomTimer {
                timer: async_io::Timer::at(Instant::now()),
                period,
            }))
        };
        match (kind, persistent) {
            (TimerKind::Tokio, false) => {
                LoopTimer::Rebuilt(TimerImpl::Tokio(TokioDelayedTimer::new(period)))
            }
            #[cfg(feature = "smol")]
            (TimerKind::Smol, false) => LoopTimer::Rebuilt(TimerImpl::Smol(
                bolt_load::runtime::SmolDelayedTimer::new(period),
            )),
            #[cfg(not(feature = "smol"))]
            (TimerKind::Smol, false) => LoopTimer::RebuiltSmol {
                timer: async_io::Timer::at(Instant::now()),
                period,
            },
            (TimerKind::Custom, false) => LoopTimer::Rebuilt(custom()),
            (TimerKind::Tokio, true) => {
                let mut timer = tokio::time::interval(period);
                timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                LoopTimer::Tokio(timer)
            }
            (TimerKind::Smol, true) => LoopTimer::Smol {
                timer: async_io::Timer::at(Instant::now()),
                period,
            },
            (TimerKind::Custom, true) => LoopTimer::CustomStream(Box::pin(
                futures::stream::unfold(custom(), |mut timer| async move {
                    timer.tick().await;
                    Some(((), timer))
                }),
            )),
        }
    }

    /// Today's `TimerImpl::tick`, rebuilt on every loop iteration.
    async fn tick(&mut self) {
        match self {
            LoopTimer::Rebuilt(timer) => timer.tick().await,
            #[cfg(not(feature = "smol"))]
            LoopTimer::RebuiltSmol { timer, period } => {
                (&mut *timer).await;
                timer.set_after(*period);
            }
            _ => unreachable!("persistent timers are polled"),
        }
    }

    /// The planned `EventTimer::poll_tick`.
    #[inline]
    fn poll_tick(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        match self {
            LoopTimer::Tokio(timer) => timer.poll_tick(cx).map(drop),
            LoopTimer::Smol { timer, period } => {
                ready!(Pin::new(&mut *timer).poll(cx));
                timer.set_after(*period);
                Poll::Ready(())
            }
            LoopTimer::CustomStream(stream) => stream.as_mut().poll_next(cx).map(drop),
            _ => unreachable!("rebuilt timers are ticked"),
        }
    }
}

enum WriteReply {
    Awaited(oneshot::Sender<()>),
    Completion(async_channel::Sender<Range<u64>>),
}

struct WriteCommand {
    range: Range<u64>,
    data: Bytes,
    reply: WriteReply,
}

/// Mirrors `PendingWriter<FileWriter>` over a single-worker backend (like mmap):
/// `capacity` writes in flight plus one buffered pending write, flushed once a
/// completion batch has been settled.
struct LoopWriter {
    mode: WriteMode,
    commands: async_channel::Sender<WriteCommand>,
    done_tx: async_channel::Sender<Range<u64>>,
    done_rx: Pin<Box<async_channel::Receiver<Range<u64>>>>,
    in_flight: usize,
    capacity: usize,
    pending_write: Option<(Range<u64>, Bytes)>,
    /// Every dispatched write must complete exactly once.
    dispatched: u64,
    completed: u64,
    /// Direct submissions that found the backend queue full.
    fallbacks: u64,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl LoopWriter {
    fn new(cfg: &LoopConfig) -> Self {
        let capacity = cfg.capacity;
        let write_cost_ns = cfg.write_cost_ns;
        let (commands, command_rx) = async_channel::bounded::<WriteCommand>(cfg.queue);
        let (done_tx, done_rx) = async_channel::bounded(capacity + 1);
        let worker = std::thread::spawn(move || {
            while let Ok(command) = command_rx.recv_blocking() {
                black_box(sink(&command.range, &command.data, write_cost_ns));
                match command.reply {
                    WriteReply::Awaited(tx) => drop(tx.send(())),
                    WriteReply::Completion(tx) => tx
                        .try_send(command.range)
                        .expect("completions are bounded by the in-flight limit"),
                }
            }
        });
        Self {
            mode: cfg.write,
            commands,
            done_tx,
            done_rx: Box::pin(done_rx),
            in_flight: 0,
            capacity,
            pending_write: None,
            dispatched: 0,
            completed: 0,
            fallbacks: 0,
            worker: Some(worker),
        }
    }

    /// `PendingWriter::can_write`.
    #[inline]
    fn can_write(&self) -> bool {
        self.in_flight < self.capacity || self.pending_write.is_none()
    }

    /// `PendingWriter::write_range`.
    fn write_range(&mut self, range: Range<u64>, data: Bytes) {
        if self.in_flight < self.capacity {
            self.dispatch(range, data);
        } else {
            assert!(self.pending_write.is_none(), "writer full");
            self.pending_write = Some((range, data));
        }
    }

    fn dispatch(&mut self, range: Range<u64>, data: Bytes) {
        self.in_flight += 1;
        self.dispatched += 1;
        let commands = self.commands.clone();
        let done_tx = self.done_tx.clone();
        match self.mode {
            WriteMode::SpawnOneshot => {
                tokio::spawn(async move {
                    let (tx, rx) = oneshot::channel();
                    let command = WriteCommand {
                        range: range.clone(),
                        data,
                        reply: WriteReply::Awaited(tx),
                    };
                    if commands.send(command).await.is_ok() {
                        let _ = rx.await;
                    }
                    let _ = done_tx.send(range).await;
                });
            }
            WriteMode::DirectCompletion => {
                let command = WriteCommand {
                    range,
                    data,
                    reply: WriteReply::Completion(done_tx),
                };
                match commands.try_send(command) {
                    Ok(()) => {}
                    Err(async_channel::TrySendError::Full(command)) => {
                        self.fallbacks += 1;
                        tokio::spawn(async move {
                            let _ = commands.send(command).await;
                        });
                    }
                    Err(async_channel::TrySendError::Closed(_)) => unreachable!("worker alive"),
                }
            }
        }
    }

    /// Ack a batch, then flush the pending write into the freed slot.
    fn settle(&mut self, mut batch: Vec<Range<u64>>) -> Vec<Range<u64>> {
        while let Ok(range) = self.done_rx.try_recv() {
            batch.push(range);
        }
        self.in_flight = self
            .in_flight
            .checked_sub(batch.len())
            .expect("duplicate completion");
        self.completed += batch.len() as u64;
        if self.in_flight < self.capacity
            && let Some((range, data)) = self.pending_write.take()
        {
            self.dispatch(range, data);
        }
        batch
    }

    /// `PendingWriter::try_tick`.
    fn try_tick(&mut self) -> Vec<Range<u64>> {
        self.settle(Vec::new())
    }

    /// `PendingWriter::tick`: rebuilds a `recv()` listener per call.
    async fn tick(&mut self) -> Vec<Range<u64>> {
        let first = self.done_rx.recv().await.expect("completion channel");
        self.settle(vec![first])
    }

    /// The planned `PendingWriter::poll_completions`.
    #[inline]
    fn poll_completions(&mut self, cx: &mut Context<'_>) -> Poll<Vec<Range<u64>>> {
        if self.in_flight == 0 {
            return Poll::Pending;
        }
        let first = ready!(self.done_rx.as_mut().poll_next(cx)).expect("completion channel");
        Poll::Ready(self.settle(vec![first]))
    }

    /// Wait for every write; returns the bytes completed meanwhile and the number of
    /// full-queue fallbacks.
    async fn finish(mut self) -> (u64, u64) {
        let mut written = 0;
        while self.in_flight > 0 || self.pending_write.is_some() {
            written += self
                .tick()
                .await
                .iter()
                .map(|r| r.end - r.start)
                .sum::<u64>();
        }
        assert_eq!(
            self.completed, self.dispatched,
            "lost or duplicated completions"
        );
        assert!(self.done_rx.try_recv().is_err(), "duplicate completion");
        let Self {
            commands,
            worker,
            fallbacks,
            ..
        } = self;
        drop(commands);
        worker.expect("worker").join().expect("worker panicked");
        (written, fallbacks)
    }
}

/// Mirrors `ChunkPlanner` (chunks in a `BTreeMap` keyed by start) and the
/// per-runner `meters` of `RunnerManager`.
struct Planner {
    chunks: BTreeMap<u64, ChunkState>,
    runner_to_chunk: HashMap<RunnerId, u64>,
    meters: HashMap<RunnerId, usize>,
}

impl Planner {
    fn new(ranges: &[Range<u64>]) -> Self {
        let mut planner = Self {
            chunks: BTreeMap::new(),
            runner_to_chunk: HashMap::with_capacity(ranges.len()),
            meters: HashMap::with_capacity(ranges.len()),
        };
        for (id, range) in ranges.iter().enumerate() {
            planner.chunks.insert(
                range.start,
                ChunkState {
                    allocated: range.clone(),
                    downloaded: range.start..range.start,
                    finished: false,
                },
            );
            planner.runner_to_chunk.insert(id as RunnerId, range.start);
        }
        planner
    }

    /// `RunnerManager::handle_data_frame`: meter, then `ChunkPlanner::update_progress`.
    #[inline]
    fn update_progress(&mut self, runner_id: RunnerId, len: usize) -> Range<u64> {
        *self.meters.entry(runner_id).or_insert(0) += len;
        let start = self.runner_to_chunk[&runner_id];
        let chunk = self.chunks.get_mut(&start).expect("chunk");
        let remaining = chunk.allocated.end - chunk.downloaded.end;
        let new_end = chunk.downloaded.end + (len as u64).min(remaining);
        let previous_end = std::mem::replace(&mut chunk.downloaded.end, new_end);
        if chunk.downloaded.end >= chunk.allocated.end {
            chunk.finished = true;
        }
        previous_end..new_end
    }

    /// `ChunkPlanner::get_total_downloaded`: O(chunks).
    fn total_downloaded(&self) -> u64 {
        self.chunks
            .values()
            .map(|c| c.downloaded.end - c.downloaded.start)
            .sum()
    }

    /// `take_meters_snapshot` + `get_downloaded_ranges` (collect, sort, merge).
    fn sample(&mut self) -> (usize, u64) {
        let speed = self.meters.values_mut().map(std::mem::take).sum::<usize>();
        let ranges = self
            .chunks
            .values()
            .filter(|c| c.downloaded.start < c.downloaded.end)
            .map(|c| c.downloaded.clone())
            .collect();
        (speed, merged_len(ranges))
    }
}

/// Mirrors the crate's aggregators: an empty group stays pending instead of ending.
struct Group<S>(StreamGroup<S>);

impl<S: Stream + Unpin> Stream for Group<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<S::Item>> {
        if self.0.is_empty() {
            return Poll::Pending;
        }
        self.0.poll_next_unpin(cx)
    }
}

type LifecycleGroup = Group<futures::stream::BoxStream<'static, (RunnerId, Lifecycle)>>;
type DataGroup = Group<futures::stream::BoxStream<'static, (RunnerId, Bytes)>>;

/// A connected runner's channels, sent from its task like `RunnerBuilderOutput`.
type Connected = (RunnerId, AsyncHeapCons<Lifecycle>, AsyncHeapCons<Bytes>);

/// Mirrors `PendingRunnerGroup`: registers the waker on every poll and stays
/// pending while empty.
struct PendingGroup {
    waker: futures::task::AtomicWaker,
    group: Pin<Box<FutureGroup<oneshot::AsyncReceiver<Connected>>>>,
}

impl Stream for PendingGroup {
    type Item = Result<Connected, oneshot::RecvError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.waker.register(cx.waker());
        if self.group.is_empty() {
            return Poll::Pending;
        }
        self.group.as_mut().poll_next(cx)
    }
}

enum RunnerEvent {
    Lifecycle(Option<(RunnerId, Lifecycle)>),
    Pending(Option<Result<Connected, oneshot::RecvError>>),
    Data(Option<(RunnerId, Bytes)>),
}

enum LoopEvent {
    Cancelled,
    Deadline,
    Written(Vec<Range<u64>>),
    Strategy,
    Throughput,
    Runner(RunnerEvent),
}

/// The planned `RunnerManager::poll_tick`.
#[inline]
fn poll_runner(
    lifecycle: &mut LifecycleGroup,
    pending: &mut PendingGroup,
    data: &mut DataGroup,
    data_enabled: bool,
    cx: &mut Context<'_>,
) -> Poll<RunnerEvent> {
    if let Poll::Ready(event) = lifecycle.poll_next_unpin(cx) {
        return Poll::Ready(RunnerEvent::Lifecycle(event));
    }
    if let Poll::Ready(connected) = pending.poll_next_unpin(cx) {
        return Poll::Ready(RunnerEvent::Pending(connected));
    }
    if data_enabled {
        data.poll_next_unpin(cx).map(RunnerEvent::Data)
    } else {
        Poll::Pending
    }
}

/// Today's `RunnerManager::tick` / `tick_lifecycle_only`.
async fn runner_tick_rebuilt(
    lifecycle: &mut LifecycleGroup,
    pending: &mut PendingGroup,
    data: &mut DataGroup,
    data_enabled: bool,
) -> RunnerEvent {
    let next_lifecycle = lifecycle.next().fuse();
    let next_pending = pending.next().fuse();
    futures::pin_mut!(next_lifecycle, next_pending);
    if data_enabled {
        let next_data = data.next().fuse();
        futures::pin_mut!(next_data);
        futures::select_biased! {
            event = next_lifecycle => RunnerEvent::Lifecycle(event),
            connected = next_pending => RunnerEvent::Pending(connected),
            frame = next_data => RunnerEvent::Data(frame),
        }
    } else {
        futures::select_biased! {
            event = next_lifecycle => RunnerEvent::Lifecycle(event),
            connected = next_pending => RunnerEvent::Pending(connected),
        }
    }
}

/// Result of one loop run.
struct LoopOutcome {
    downloaded: u64,
    written: u64,
    frames: u64,
    strategy_ticks: u64,
    throughput_ticks: u64,
    fallbacks: u64,
    /// Time from the stop signal to the loop handling it.
    stop_latency: Option<Duration>,
}

struct LoopModel {
    cfg: LoopConfig,
    planner: Planner,
    writer: LoopWriter,
    lifecycle: LifecycleGroup,
    pending: PendingGroup,
    data: DataGroup,
    cancel_token: smol_cancellation_token::CancellationToken,
    /// The persistent cancel listener; `rebuilt_select` re-creates one per iteration.
    cancel: Option<Fuse<Pin<Box<smol_cancellation_token::WaitForCancellationFutureOwned>>>>,
    cancelled_at: Arc<std::sync::Mutex<Option<Instant>>>,
    deadline: async_io::Timer,
    no_progress: Duration,
    last_accepted: u64,
    last_progress: Instant,
    strategy: LoopTimer,
    throughput: LoopTimer,
    producers: Vec<tokio::task::JoinHandle<()>>,
    finished: usize,
    frames: u64,
    written: u64,
    strategy_ticks: u64,
    throughput_ticks: u64,
}

impl LoopModel {
    fn new(cfg: LoopConfig) -> Self {
        let ranges = split_ranges(cfg.total, cfg.runners);
        let frame = Bytes::from(vec![0xABu8; cfg.frame_size]);
        let mut connecting = Box::pin(FutureGroup::new());
        let mut producers = Vec::with_capacity(ranges.len());
        for (id, range) in ranges.iter().enumerate() {
            let (connected_tx, connected_rx) = oneshot::channel();
            connecting.as_mut().insert(connected_rx.into_future());
            let mut src = FrameSource::new(&frame, range);
            if !matches!(cfg.stop, Stop::Complete) {
                // Keep the loop busy until the stop signal; extra frames are clipped.
                src.remaining = u64::MAX;
            }
            let late = cfg.stagger_connect && id % 2 == 1;
            producers.push(tokio::spawn(async move {
                // "Connect", then hand the channels to the loop.
                if late {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                } else {
                    tokio::task::yield_now().await;
                }
                let (mut lc_tx, lc_rx) = AsyncHeapRb::new(LIFECYCLE_CHANNEL_CAPACITY).split();
                let (mut data_tx, data_rx) = AsyncHeapRb::new(DATA_FRAME_CHANNEL_CAPACITY).split();
                if connected_tx.send((id as RunnerId, lc_rx, data_rx)).is_err() {
                    return;
                }
                let _ = lc_tx.push(Lifecycle::Started).await;
                while let Some(f) = src.next().await {
                    if data_tx.push(f).await.is_err() {
                        return;
                    }
                }
                data_tx.wait_vacant(DATA_FRAME_CHANNEL_CAPACITY).await;
                let _ = lc_tx.push(Lifecycle::Finished).await;
            }));
        }

        let cancel_token = smol_cancellation_token::CancellationToken::new();
        let persistent = cfg.shape != LoopShape::RebuiltSelect;
        let now = Instant::now();
        Self {
            cfg,
            planner: Planner::new(&ranges),
            writer: LoopWriter::new(&cfg),
            lifecycle: Group(StreamGroup::new()),
            pending: PendingGroup {
                waker: futures::task::AtomicWaker::new(),
                group: connecting,
            },
            data: Group(StreamGroup::new()),
            cancel: persistent.then(|| Box::pin(cancel_token.clone().cancelled_owned()).fuse()),
            cancel_token,
            cancelled_at: Arc::default(),
            deadline: async_io::Timer::at(now + LOOP_NO_PROGRESS),
            no_progress: LOOP_NO_PROGRESS,
            last_accepted: 0,
            last_progress: now,
            strategy: LoopTimer::new(cfg.timer, LOOP_STRATEGY_INTERVAL, persistent),
            throughput: LoopTimer::new(cfg.timer, LOOP_THROUGHPUT_INTERVAL, persistent),
            producers,
            finished: 0,
            frames: 0,
            written: 0,
            strategy_ticks: 0,
            throughput_ticks: 0,
        }
    }

    /// Arm the stop signal once setup is done, right before the loop starts.
    fn arm_stop(&mut self) {
        match self.cfg.stop {
            Stop::Complete => {}
            Stop::CancelAfter(after) => {
                let token = self.cancel_token.clone();
                let cancelled_at = self.cancelled_at.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(after);
                    *cancelled_at.lock().unwrap() = Some(Instant::now());
                    token.cancel();
                });
            }
            Stop::DeadlineAfter(after) => {
                self.no_progress = after;
                self.last_progress = Instant::now();
                self.deadline.set_at(self.last_progress + after);
            }
        }
    }

    #[inline]
    fn data_enabled(&self) -> bool {
        self.writer.can_write()
    }

    async fn next_rebuilt(&mut self) -> LoopEvent {
        let data_enabled = self.data_enabled();
        let Self {
            writer,
            lifecycle,
            pending,
            data,
            cancel_token,
            deadline,
            strategy,
            throughput,
            ..
        } = self;
        let writer_fut = if writer.in_flight == 0 && writer.pending_write.is_none() {
            futures::future::Either::Left(futures::future::pending())
        } else {
            futures::future::Either::Right(writer.tick())
        }
        .fuse();
        let runner_fut = runner_tick_rebuilt(lifecycle, pending, data, data_enabled).fuse();
        let cancelled = cancel_token.cancelled().fuse();
        let deadline = FutureExt::fuse(deadline);
        let strategy_tick = strategy.tick().fuse();
        let throughput_tick = throughput.tick().fuse();
        futures::pin_mut!(
            cancelled,
            deadline,
            writer_fut,
            runner_fut,
            strategy_tick,
            throughput_tick
        );
        futures::select_biased! {
            _ = cancelled => LoopEvent::Cancelled,
            _ = deadline => LoopEvent::Deadline,
            batch = writer_fut => LoopEvent::Written(batch),
            _ = strategy_tick => LoopEvent::Strategy,
            _ = throughput_tick => LoopEvent::Throughput,
            event = runner_fut => LoopEvent::Runner(event),
        }
    }

    async fn next_persistent(&mut self) -> LoopEvent {
        let data_enabled = self.data_enabled();
        let Self {
            writer,
            lifecycle,
            pending,
            data,
            cancel,
            deadline,
            strategy,
            throughput,
            ..
        } = self;
        let mut cancel = cancel.as_mut().expect("persistent cancel listener");
        let deadline = FutureExt::fuse(deadline);
        let writer_fut = poll_fn(|cx| writer.poll_completions(cx)).fuse();
        let strategy_tick = poll_fn(|cx| strategy.poll_tick(cx)).fuse();
        let throughput_tick = poll_fn(|cx| throughput.poll_tick(cx)).fuse();
        let runner_fut =
            poll_fn(|cx| poll_runner(lifecycle, pending, data, data_enabled, cx)).fuse();
        futures::pin_mut!(
            deadline,
            writer_fut,
            strategy_tick,
            throughput_tick,
            runner_fut
        );
        futures::select_biased! {
            _ = cancel => LoopEvent::Cancelled,
            _ = deadline => LoopEvent::Deadline,
            batch = writer_fut => LoopEvent::Written(batch),
            _ = strategy_tick => LoopEvent::Strategy,
            _ = throughput_tick => LoopEvent::Throughput,
            event = runner_fut => LoopEvent::Runner(event),
        }
    }

    /// Priority: cancel > deadline > writer > strategy > throughput > runner.
    fn poll_event(&mut self, cx: &mut Context<'_>) -> Poll<LoopEvent> {
        let cancel = self.cancel.as_mut().expect("persistent cancel listener");
        if let Poll::Ready(()) = cancel.poll_unpin(cx) {
            return Poll::Ready(LoopEvent::Cancelled);
        }
        if Pin::new(&mut self.deadline).poll(cx).is_ready() {
            return Poll::Ready(LoopEvent::Deadline);
        }
        if let Poll::Ready(batch) = self.writer.poll_completions(cx) {
            return Poll::Ready(LoopEvent::Written(batch));
        }
        if let Poll::Ready(()) = self.strategy.poll_tick(cx) {
            return Poll::Ready(LoopEvent::Strategy);
        }
        if let Poll::Ready(()) = self.throughput.poll_tick(cx) {
            return Poll::Ready(LoopEvent::Throughput);
        }
        let data_enabled = self.data_enabled();
        poll_runner(
            &mut self.lifecycle,
            &mut self.pending,
            &mut self.data,
            data_enabled,
            cx,
        )
        .map(LoopEvent::Runner)
    }

    /// `record_completions` + `check_completions`.
    fn record_completions(&mut self, batch: Vec<Range<u64>>) {
        self.written += batch.iter().map(|r| r.end - r.start).sum::<u64>();
    }

    /// Returns the stop latency once the loop must end.
    fn handle(&mut self, event: LoopEvent) -> ControlFlow<Option<Duration>> {
        match event {
            LoopEvent::Cancelled => {
                let at = self.cancelled_at.lock().unwrap().expect("cancel time");
                return ControlFlow::Break(Some(at.elapsed()));
            }
            LoopEvent::Deadline => {
                let due = self.last_progress + self.no_progress;
                let now = Instant::now();
                if now >= due {
                    return ControlFlow::Break(Some(now - due));
                }
                self.deadline.set_at(due);
            }
            LoopEvent::Written(batch) => self.record_completions(batch),
            LoopEvent::Strategy => {
                self.strategy_ticks += 1;
                black_box(self.planner.runner_to_chunk.len());
            }
            LoopEvent::Throughput => {
                self.throughput_ticks += 1;
                black_box(self.planner.sample());
            }
            LoopEvent::Runner(RunnerEvent::Pending(connected)) => {
                let (id, lc_rx, data_rx) = connected.expect("pending group").expect("connected");
                self.lifecycle.0.insert(tag(id, lc_rx).boxed());
                self.data.0.insert(tag(id, data_rx).boxed());
            }
            LoopEvent::Runner(RunnerEvent::Lifecycle(Some((_, Lifecycle::Finished)))) => {
                self.finished += 1;
                if self.finished == self.cfg.runners {
                    return ControlFlow::Break(None);
                }
            }
            LoopEvent::Runner(RunnerEvent::Lifecycle(_)) => {}
            // The last data stream ended; the now empty group stays pending.
            LoopEvent::Runner(RunnerEvent::Data(None)) => {}
            LoopEvent::Runner(RunnerEvent::Data(Some((id, mut bytes)))) => {
                self.frames += 1;
                // Stop a busy loop: runners may take a while to deliver on a loaded machine.
                if self.frames == 1 {
                    self.arm_stop();
                }
                let range = self.planner.update_progress(id, bytes.len());
                bytes.truncate((range.end - range.start) as usize);
                self.writer.write_range(range, bytes);
                // `policy.on_bytes_accepted(manager.get_total_downloaded(), Instant::now())`.
                let total = self.planner.total_downloaded();
                let now = Instant::now();
                if total > self.last_accepted && !matches!(self.cfg.stop, Stop::DeadlineAfter(_)) {
                    self.last_accepted = total;
                    self.last_progress = now;
                }
            }
        }
        ControlFlow::Continue(())
    }

    async fn finish(self, stop_latency: Option<Duration>) -> LoopOutcome {
        let Self {
            mut planner,
            writer,
            lifecycle,
            pending,
            data,
            producers,
            frames,
            mut written,
            strategy_ticks,
            throughput_ticks,
            ..
        } = self;
        // Dropping the consumers releases producers blocked on a full channel.
        drop((lifecycle, pending, data));
        let (tail, fallbacks) = writer.finish().await;
        written += tail;
        for producer in producers {
            producer.await.expect("producer");
        }
        LoopOutcome {
            downloaded: planner.sample().1,
            written,
            frames,
            strategy_ticks,
            throughput_ticks,
            fallbacks,
            stop_latency,
        }
    }
}

async fn download_loop(cfg: LoopConfig) -> LoopOutcome {
    let mut model = LoopModel::new(cfg);
    let stop_latency = loop {
        let event = match cfg.shape {
            LoopShape::RebuiltSelect => {
                let batch = model.writer.try_tick();
                model.record_completions(batch);
                model.next_rebuilt().await
            }
            LoopShape::PersistentSelect => model.next_persistent().await,
            LoopShape::DirectPoll => poll_fn(|cx| model.poll_event(cx)).await,
        };
        if let ControlFlow::Break(latency) = model.handle(event) {
            break latency;
        }
    };
    let outcome = model.finish(stop_latency).await;
    // A stop is armed by the first frame, so a stopped loop has done some work.
    if cfg.stop == Stop::Complete {
        assert_eq!(outcome.downloaded, cfg.total, "{cfg:?} lost bytes");
        assert_eq!(outcome.written, cfg.total, "{cfg:?} lost writes");
    }
    // Whether a queue of one fills depends on scheduling; the profile mode
    // reports how often it did.
    outcome
}

// ───────────────────────────── F: per-frame bookkeeping ─────────────────────────────

/// `meters: HashMap<RunnerId, usize>` vs the planned `Vec<Option<usize>>`.
fn meters_hashmap(runners: usize, frames: usize, sample_every: usize) -> usize {
    let mut meters: HashMap<RunnerId, usize> = HashMap::with_capacity(runners);
    let mut acc = 0;
    for i in 0..frames {
        *meters.entry((i % runners) as RunnerId).or_insert(0) += 32 * 1024;
        if i % sample_every == 0 {
            let active = meters.len();
            acc += active + meters.values_mut().map(std::mem::take).sum::<usize>();
        }
    }
    acc
}

fn meters_vec(runners: usize, frames: usize, sample_every: usize) -> usize {
    let mut meters: Vec<Option<usize>> = vec![None; runners];
    let mut acc = 0;
    for i in 0..frames {
        *meters[i % runners].get_or_insert(0) += 32 * 1024;
        if i % sample_every == 0 {
            let active = meters.iter().filter(|m| m.is_some()).count();
            acc += active
                + meters
                    .iter_mut()
                    .flatten()
                    .map(std::mem::take)
                    .sum::<usize>();
        }
    }
    acc
}

/// `active` running chunks after `retained` finished ones that no runner owns.
fn retained_chunks_planner(retained: usize, active: usize) -> Planner {
    let ranges = split_ranges(LOOP_TOTAL, retained + active);
    // Runner ids 0..active own the last chunks; the earlier ones are finished.
    let mut planner = Planner::new(&ranges[retained..]);
    for range in &ranges[..retained] {
        planner.chunks.insert(
            range.start,
            ChunkState {
                allocated: range.clone(),
                downloaded: range.clone(),
                finished: true,
            },
        );
    }
    planner
}

/// Today: every frame recomputes `get_total_downloaded()`.
fn frames_with_sum(planner: &mut Planner, active: usize, frames: usize) -> u64 {
    let mut last_accepted = 0;
    for i in 0..frames {
        planner.update_progress((i % active) as RunnerId, 1);
        last_accepted = last_accepted.max(planner.total_downloaded());
    }
    last_accepted
}

/// Planned: `accepted_bytes` grows by each accepted range.
fn frames_with_counter(planner: &mut Planner, active: usize, frames: usize) -> u64 {
    let mut accepted = planner.total_downloaded();
    for i in 0..frames {
        let range = planner.update_progress((i % active) as RunnerId, 1);
        accepted += range.end - range.start;
    }
    accepted
}

// ───────────────────────────── allocation counter ─────────────────────────────

/// Counts allocations process-wide while `COUNT_ALLOCS` is set. Built only with
/// `RUSTFLAGS="--cfg coord_alloc"`, so ordinary timing runs use the plain allocator.
#[cfg(coord_alloc)]
mod alloc_counter {
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        sync::atomic::{AtomicBool, AtomicU64, Ordering},
    };

    pub static COUNT_ALLOCS: AtomicBool = AtomicBool::new(false);
    pub static ALLOCS: AtomicU64 = AtomicU64::new(0);

    struct CountingAlloc;

    #[inline]
    fn count() {
        if COUNT_ALLOCS.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
    }

    // SAFETY: forwards every call to `System` unchanged.
    unsafe impl GlobalAlloc for CountingAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            count();
            // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            count();
            // SAFETY: the caller upholds `GlobalAlloc::alloc_zeroed`'s contract.
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            count();
            // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static GLOBAL: CountingAlloc = CountingAlloc;
}

// ───────────────────────────── criterion ─────────────────────────────

fn bench_coordination(c: &mut Criterion) {
    let rt = Runtime::new().expect("Failed to create runtime");
    let mut group = c.benchmark_group("coordination");
    group.throughput(Throughput::Bytes(TOTAL));
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(3));

    for frame_size in [16 * 1024, 32 * 1024, 64 * 1024] {
        for runners in [4usize, 16, 64] {
            for variant in Variant::ALL {
                let cfg = Config {
                    runners,
                    frame_size,
                    write_cost_ns: 0,
                };
                group.bench_with_input(
                    BenchmarkId::new(
                        variant.name(),
                        format!("n{runners}/c{}k", frame_size / 1024),
                    ),
                    &cfg,
                    |b, cfg| b.to_async(&rt).iter(|| run(variant, *cfg)),
                );
            }
        }
    }
    group.finish();
}

/// Simulated per-write cost (~ a 32 KiB memcpy into an mmap / pwrite syscall).
/// `event_loop_inline` is omitted: it would serialize writes on the coordinator, which
/// the real code never does.
fn bench_coordination_costly_write(c: &mut Criterion) {
    let rt = Runtime::new().expect("Failed to create runtime");
    let mut group = c.benchmark_group("coordination_write_cost_5us");
    group.throughput(Throughput::Bytes(TOTAL));
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(3));

    for runners in [4usize, 16, 64] {
        for variant in Variant::ALL
            .into_iter()
            .filter(|v| *v != Variant::EventLoopInline)
        {
            let cfg = Config {
                runners,
                frame_size: 32 * 1024,
                write_cost_ns: 5_000,
            };
            group.bench_with_input(
                BenchmarkId::new(variant.name(), format!("n{runners}/c32k")),
                &cfg,
                |b, cfg| b.to_async(&rt).iter(|| run(variant, *cfg)),
            );
        }
    }
    group.finish();
}

fn loop_group<'a>(
    c: &'a mut Criterion,
    name: &str,
) -> criterion::BenchmarkGroup<'a, criterion::measurement::WallTime> {
    let mut group = c.benchmark_group(name);
    group.throughput(Throughput::Bytes(LOOP_TOTAL));
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(3));
    group
}

/// Event selection: shapes × timers × runners at 32 KiB, plus a frame-size sweep.
fn bench_download_loop_shape(c: &mut Criterion) {
    let rt = Runtime::new().expect("Failed to create runtime");
    let mut group = loop_group(c, "download_loop_shape");
    let mut bench = |cfg: LoopConfig| {
        group.bench_with_input(
            BenchmarkId::new(
                cfg.shape.name(),
                format!(
                    "{}/n{}/c{}k",
                    cfg.timer.name(),
                    cfg.runners,
                    cfg.frame_size / 1024
                ),
            ),
            &cfg,
            |b, cfg| b.to_async(&rt).iter(|| download_loop(*cfg)),
        );
    };
    for timer in TimerKind::ALL {
        for runners in [1usize, 4, 16, 64] {
            for shape in LoopShape::ALL {
                bench(LoopConfig::new(shape, timer, runners, 32 * 1024));
            }
        }
    }
    for frame_size in [16 * 1024, 64 * 1024] {
        for shape in LoopShape::ALL {
            bench(LoopConfig::new(shape, TimerKind::Tokio, 16, frame_size));
        }
    }
    group.finish();
}

/// Write path at equal backend work; `q1` forces the full-queue fallback (asserted).
fn bench_download_loop_writer(c: &mut Criterion) {
    let rt = Runtime::new().expect("Failed to create runtime");
    let mut group = loop_group(c, "download_loop_writer");
    for (label, queue_full) in [("q2048", false), ("q1/cost5us", true)] {
        for runners in [4usize, 16] {
            for write in [WriteMode::SpawnOneshot, WriteMode::DirectCompletion] {
                let cfg = LoopConfig {
                    write,
                    ..LoopConfig::new(LoopShape::DirectPoll, TimerKind::Tokio, runners, 32 * 1024)
                };
                let cfg = if queue_full { cfg.queue_full() } else { cfg };
                group.bench_with_input(
                    BenchmarkId::new(write.name(), format!("{label}/n{runners}/c32k")),
                    &cfg,
                    |b, cfg| b.to_async(&rt).iter(|| download_loop(*cfg)),
                );
            }
        }
    }
    group.finish();
}

/// Longer runs with late connections: every shape must re-arm both timers repeatedly.
fn bench_download_loop_sustained(c: &mut Criterion) {
    const TOTAL: u64 = 8 * LOOP_TOTAL;
    let rt = Runtime::new().expect("Failed to create runtime");
    let mut group = loop_group(c, "download_loop_sustained");
    group.throughput(Throughput::Bytes(TOTAL));
    for timer in [TimerKind::Tokio, TimerKind::Custom] {
        for shape in LoopShape::ALL {
            let cfg = LoopConfig {
                total: TOTAL,
                stagger_connect: true,
                ..LoopConfig::new(shape, timer, 16, 32 * 1024)
            };
            group.bench_with_input(
                BenchmarkId::new(shape.name(), format!("{}/n16/c32k", timer.name())),
                &cfg,
                |b, cfg| {
                    b.to_async(&rt).iter(|| async {
                        let outcome = download_loop(*cfg).await;
                        // The first tick of each timer is immediate.
                        assert!(
                            outcome.strategy_ticks >= 3,
                            "{cfg:?}: strategy re-armed too rarely"
                        );
                        assert!(
                            outcome.throughput_ticks >= 5,
                            "{cfg:?}: throughput re-armed too rarely"
                        );
                        outcome
                    })
                },
            );
        }
    }
    group.finish();
}

/// Time from the cancel signal or the due deadline to the loop handling it, under load.
fn bench_download_loop_latency(c: &mut Criterion) {
    let rt = Runtime::new().expect("Failed to create runtime");
    let mut group = c.benchmark_group("download_loop_stop_latency");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(3));
    let after = Duration::from_millis(2);
    for (label, stop) in [
        ("cancel", Stop::CancelAfter(after)),
        ("deadline", Stop::DeadlineAfter(after)),
    ] {
        for shape in LoopShape::ALL {
            let cfg = LoopConfig {
                stop,
                ..LoopConfig::new(shape, TimerKind::Tokio, 16, 32 * 1024)
            };
            group.bench_with_input(
                BenchmarkId::new(shape.name(), format!("{label}/n16/c32k")),
                &cfg,
                |b, cfg| {
                    b.iter_custom(|iters| {
                        (0..iters)
                            .map(|_| {
                                rt.block_on(download_loop(*cfg))
                                    .stop_latency
                                    .expect("the loop stopped early")
                            })
                            .sum()
                    })
                },
            );
        }
    }
    group.finish();
}

fn bench_frame_bookkeeping(c: &mut Criterion) {
    const FRAMES: usize = 1 << 16;
    let mut group = c.benchmark_group("frame_bookkeeping");
    group.throughput(Throughput::Elements(FRAMES as u64));
    for runners in [4usize, 16, 64] {
        group.bench_with_input(
            BenchmarkId::new("meters_hashmap", format!("n{runners}")),
            &runners,
            |b, &n| b.iter(|| meters_hashmap(n, FRAMES, 256)),
        );
        group.bench_with_input(
            BenchmarkId::new("meters_vec", format!("n{runners}")),
            &runners,
            |b, &n| b.iter(|| meters_vec(n, FRAMES, 256)),
        );
    }
    const ACTIVE: usize = 4;
    for retained in [16usize, 256, 4096] {
        assert_eq!(
            frames_with_sum(
                &mut retained_chunks_planner(retained, ACTIVE),
                ACTIVE,
                FRAMES
            ),
            frames_with_counter(
                &mut retained_chunks_planner(retained, ACTIVE),
                ACTIVE,
                FRAMES
            ),
            "the counter must match the recomputed total"
        );
        group.bench_with_input(
            BenchmarkId::new("progress_sum", format!("a{ACTIVE}/r{retained}")),
            &retained,
            |b, &r| {
                b.iter_batched_ref(
                    || retained_chunks_planner(r, ACTIVE),
                    |planner| frames_with_sum(planner, ACTIVE, FRAMES),
                    criterion::BatchSize::LargeInput,
                )
            },
        );
        group.bench_with_input(
            BenchmarkId::new("progress_counter", format!("a{ACTIVE}/r{retained}")),
            &retained,
            |b, &r| {
                b.iter_batched_ref(
                    || retained_chunks_planner(r, ACTIVE),
                    |planner| frames_with_counter(planner, ACTIVE, FRAMES),
                    criterion::BatchSize::LargeInput,
                )
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_coordination,
    bench_coordination_costly_write,
    bench_download_loop_shape,
    bench_download_loop_writer,
    bench_download_loop_sustained,
    bench_download_loop_latency,
    bench_frame_bookkeeping
);

/// `COORD_PROFILE=loop:shape:timer:write:runners:frame_size:iterations[:queue_full]`
/// runs one download loop config outside criterion, reporting ns/frame, timer ticks
/// and full-queue fallbacks. Built with `RUSTFLAGS="--cfg coord_alloc"` it also reports
/// process-wide allocations per frame (from a separate, untimed pass).
fn profile_loop(parts: &[&str]) {
    let parse = |name: &str, s: &str| -> usize { s.parse().unwrap_or_else(|_| panic!("{name}")) };
    let shape = *LoopShape::ALL
        .iter()
        .find(|s| s.name() == parts[0])
        .expect("unknown shape");
    let timer = *TimerKind::ALL
        .iter()
        .find(|t| t.name() == parts[1])
        .expect("unknown timer");
    let write = *[WriteMode::SpawnOneshot, WriteMode::DirectCompletion]
        .iter()
        .find(|w| w.name() == parts[2])
        .expect("unknown write mode");
    let cfg = LoopConfig {
        write,
        ..LoopConfig::new(
            shape,
            timer,
            parse("runners", parts[3]),
            parse("frame_size", parts[4]),
        )
    };
    let cfg = if parts.get(6) == Some(&"queue_full") {
        cfg.queue_full()
    } else {
        cfg
    };
    let iterations = parse("iterations", parts[5]);
    let rt = Runtime::new().expect("Failed to create runtime");
    let start = Instant::now();
    let (mut frames, mut strategy_ticks, mut throughput_ticks, mut fallbacks) = (0, 0, 0, 0);
    for _ in 0..iterations {
        let outcome = rt.block_on(download_loop(cfg));
        frames += outcome.frames;
        strategy_ticks += outcome.strategy_ticks;
        throughput_ticks += outcome.throughput_ticks;
        fallbacks += outcome.fallbacks;
    }
    let elapsed = start.elapsed();
    let gib = (cfg.total * iterations as u64) as f64 / (1u64 << 30) as f64;
    println!(
        "loop:{}: {gib:.2} GiB in {elapsed:?} -> {:.2} GiB/s wall, {:.1} ns/frame, {fallbacks} \
         fallbacks, {strategy_ticks} strategy + {throughput_ticks} throughput ticks",
        parts.join(":"),
        gib / elapsed.as_secs_f64(),
        elapsed.as_nanos() as f64 / frames as f64,
    );
    #[cfg(coord_alloc)]
    {
        use alloc_counter::{ALLOCS, COUNT_ALLOCS};
        let mut frames = 0;
        COUNT_ALLOCS.store(true, Ordering::Relaxed);
        for _ in 0..iterations {
            frames += rt.block_on(download_loop(cfg)).frames;
        }
        COUNT_ALLOCS.store(false, Ordering::Relaxed);
        println!(
            "  {:.2} allocs/frame (process-wide, amortized)",
            ALLOCS.load(Ordering::Relaxed) as f64 / frames as f64
        );
    }
}

/// `COORD_PROFILE=variant:runners:frame_size:iterations[:write_cost_ns]` runs one config
/// outside criterion so it can be wrapped in `/usr/bin/time -l` for CPU time / max RSS.
fn profile(spec: &str) {
    let parts: Vec<&str> = spec.split(':').collect();
    if parts[0] == "loop" {
        return profile_loop(&parts[1..]);
    }
    let variant = Variant::parse(parts[0]).expect("unknown variant");
    let cfg = Config {
        runners: parts[1].parse().expect("runners"),
        frame_size: parts[2].parse().expect("frame_size"),
        write_cost_ns: parts
            .get(4)
            .map_or(0, |s| s.parse().expect("write_cost_ns")),
    };
    let iterations: u64 = parts[3].parse().expect("iterations");
    let rt = Runtime::new().expect("Failed to create runtime");
    {
        let ranges = split_ranges(TOTAL, cfg.runners);
        let frame = Bytes::from(vec![0u8; 8]);
        println!(
            "future sizes: run={}B event_loop={}B std_mutex={}B",
            std::mem::size_of_val(&run(variant, cfg)),
            std::mem::size_of_val(&event_loop(&ranges, &frame, cfg, false)),
            std::mem::size_of_val(&std_mutex(&ranges, &frame, cfg)),
        );
    }
    let start = Instant::now();
    for _ in 0..iterations {
        rt.block_on(run(variant, cfg));
    }
    let elapsed = start.elapsed();
    let gib = (TOTAL * iterations) as f64 / (1u64 << 30) as f64;
    println!(
        "{spec}: {gib:.2} GiB in {elapsed:?} -> {:.2} GiB/s wall",
        gib / elapsed.as_secs_f64()
    );
}

fn main() {
    if let Ok(spec) = std::env::var("COORD_PROFILE") {
        profile(&spec);
        return;
    }
    benches();
    Criterion::default().configure_from_args().final_summary();
}
