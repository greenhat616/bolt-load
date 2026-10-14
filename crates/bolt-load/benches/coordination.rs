//! Coordination-strategy benchmark for the concurrent download hot path.
//!
//! Compares the single-owner "event loop" design used by `ConcurrentTask::download_to`
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
    alloc::{GlobalAlloc, Layout, System},
    collections::HashMap,
    future::poll_fn,
    hint::black_box,
    ops::{ControlFlow, Range},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    task::{Context, Poll, ready},
    time::{Duration, Instant},
};

use async_ringbuf::{AsyncHeapCons, AsyncHeapProd, AsyncHeapRb, traits::*};
use bolt_load::runtime::{DynTimer, Timer, TimerImpl, TokioDelayedTimer};
use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use futures::{FutureExt, Stream, StreamExt, future::Fuse};
use futures_concurrency::stream::StreamGroup;
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
#[derive(Clone, Copy, Debug)]
enum Stop {
    /// After every runner finished.
    Complete,
    /// On cancellation, `after` the loop started.
    CancelAfter(Duration),
    /// On the no-progress deadline, `after` the loop started (progress is ignored).
    DeadlineAfter(Duration),
}

#[derive(Clone, Copy, Debug)]
struct LoopConfig {
    shape: LoopShape,
    timer: TimerKind,
    write: WriteMode,
    runners: usize,
    frame_size: usize,
    /// Backend command queue capacity.
    queue: usize,
    write_cost_ns: u64,
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
            queue: WRITER_QUEUE,
            write_cost_ns: 0,
            stop: Stop::Complete,
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

/// Fallbacks to the spawned `send().await` when the backend queue was full.
static WRITE_FALLBACKS: AtomicU64 = AtomicU64::new(0);

/// Mirrors `PendingWriter<FileWriter>` over a single-worker backend (like mmap).
struct LoopWriter {
    mode: WriteMode,
    commands: async_channel::Sender<WriteCommand>,
    done_tx: async_channel::Sender<Range<u64>>,
    done_rx: Pin<Box<async_channel::Receiver<Range<u64>>>>,
    in_flight: usize,
    capacity: usize,
    /// Every dispatched write must complete exactly once.
    dispatched: u64,
    completed: u64,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl LoopWriter {
    fn new(mode: WriteMode, queue: usize, write_cost_ns: u64) -> Self {
        let capacity = std::thread::available_parallelism().map_or(4, |n| n.get());
        let (commands, command_rx) = async_channel::bounded::<WriteCommand>(queue);
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
            mode,
            commands,
            done_tx,
            done_rx: Box::pin(done_rx),
            in_flight: 0,
            capacity,
            dispatched: 0,
            completed: 0,
            worker: Some(worker),
        }
    }

    #[inline]
    fn can_write(&self) -> bool {
        self.in_flight < self.capacity
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
                        WRITE_FALLBACKS.fetch_add(1, Ordering::Relaxed);
                        tokio::spawn(async move {
                            let _ = commands.send(command).await;
                        });
                    }
                    Err(async_channel::TrySendError::Closed(_)) => unreachable!("worker alive"),
                }
            }
        }
    }

    fn ack(&mut self, n: usize) {
        self.in_flight = self.in_flight.checked_sub(n).expect("duplicate completion");
        self.completed += n as u64;
    }

    /// `PendingWriter::try_tick`.
    fn try_drain(&mut self) -> usize {
        let mut n = 0;
        while self.done_rx.try_recv().is_ok() {
            n += 1;
        }
        self.ack(n);
        n
    }

    /// `PendingWriter::tick`: rebuilds a `recv()` listener per call.
    async fn tick(&mut self) -> usize {
        self.done_rx.recv().await.expect("completion channel");
        self.ack(1);
        1 + self.try_drain()
    }

    /// The planned `PendingWriter::poll_completions`.
    #[inline]
    fn poll_done(&mut self, cx: &mut Context<'_>) -> Poll<usize> {
        if self.in_flight == 0 {
            return Poll::Pending;
        }
        ready!(self.done_rx.as_mut().poll_next(cx)).expect("completion channel");
        self.ack(1);
        Poll::Ready(1 + self.try_drain())
    }

    async fn finish(mut self) {
        while self.in_flight > 0 {
            self.tick().await;
        }
        assert_eq!(
            self.completed, self.dispatched,
            "lost or duplicated completions"
        );
        assert!(self.done_rx.try_recv().is_err(), "duplicate completion");
        let Self {
            commands, worker, ..
        } = self;
        drop(commands);
        worker.expect("worker").join().expect("worker panicked");
    }
}

type LifecycleGroup = StreamGroup<futures::stream::BoxStream<'static, (RunnerId, Lifecycle)>>;
type DataGroup = StreamGroup<futures::stream::BoxStream<'static, (RunnerId, Bytes)>>;

enum RunnerEvent {
    Lifecycle(Option<(RunnerId, Lifecycle)>),
    /// The pending group never yields here: every runner starts connected.
    Pending,
    Data(Option<(RunnerId, Bytes)>),
}

enum LoopEvent {
    Cancelled,
    Deadline,
    Written(usize),
    Strategy,
    Throughput,
    Runner(RunnerEvent),
}

/// The planned `RunnerManager::poll_tick`.
#[inline]
fn poll_runner(
    lifecycle: &mut LifecycleGroup,
    pending: &mut futures::stream::Pending<()>,
    data: &mut DataGroup,
    data_enabled: bool,
    cx: &mut Context<'_>,
) -> Poll<RunnerEvent> {
    if let Poll::Ready(event) = lifecycle.poll_next_unpin(cx) {
        return Poll::Ready(RunnerEvent::Lifecycle(event));
    }
    if pending.poll_next_unpin(cx).is_ready() {
        return Poll::Ready(RunnerEvent::Pending);
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
    pending: &mut futures::stream::Pending<()>,
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
            _ = next_pending => RunnerEvent::Pending,
            frame = next_data => RunnerEvent::Data(frame),
        }
    } else {
        futures::select_biased! {
            event = next_lifecycle => RunnerEvent::Lifecycle(event),
            _ = next_pending => RunnerEvent::Pending,
        }
    }
}

/// Result of one loop run.
struct LoopOutcome {
    downloaded: u64,
    frames: u64,
    strategy_ticks: u64,
    throughput_ticks: u64,
    /// Time from the stop signal to the loop handling it.
    stop_latency: Option<Duration>,
}

struct LoopModel {
    cfg: LoopConfig,
    state: State,
    writer: LoopWriter,
    lifecycle: LifecycleGroup,
    pending: futures::stream::Pending<()>,
    data: DataGroup,
    data_done: bool,
    cancel_token: smol_cancellation_token::CancellationToken,
    cancel: Fuse<Pin<Box<smol_cancellation_token::WaitForCancellationFutureOwned>>>,
    cancelled_at: Arc<std::sync::Mutex<Option<Instant>>>,
    deadline: async_io::Timer,
    last_progress: Instant,
    strategy: LoopTimer,
    throughput: LoopTimer,
    producers: Vec<tokio::task::JoinHandle<()>>,
    finished: usize,
    frames: u64,
    strategy_ticks: u64,
    throughput_ticks: u64,
    checksum: u64,
}

impl LoopModel {
    fn new(cfg: LoopConfig) -> Self {
        let ranges = split_ranges(LOOP_TOTAL, cfg.runners);
        let frame = Bytes::from(vec![0xABu8; cfg.frame_size]);
        let mut lifecycle = StreamGroup::new();
        let mut data = StreamGroup::new();
        let mut producers = Vec::with_capacity(ranges.len());
        for (id, range) in ranges.iter().enumerate() {
            let (mut lc_tx, lc_rx): (AsyncHeapProd<Lifecycle>, AsyncHeapCons<Lifecycle>) =
                AsyncHeapRb::new(LIFECYCLE_CHANNEL_CAPACITY).split();
            let (mut data_tx, data_rx): (AsyncHeapProd<Bytes>, AsyncHeapCons<Bytes>) =
                AsyncHeapRb::new(DATA_FRAME_CHANNEL_CAPACITY).split();
            lifecycle.insert(tag(id as RunnerId, lc_rx).boxed());
            data.insert(tag(id as RunnerId, data_rx).boxed());
            let mut src = FrameSource::new(&frame, range);
            if !matches!(cfg.stop, Stop::Complete) {
                // Keep the loop busy until the stop signal; extra frames are clipped.
                src.remaining = u64::MAX;
            }
            producers.push(tokio::spawn(async move {
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

        let now = Instant::now();
        let no_progress = match cfg.stop {
            Stop::DeadlineAfter(after) => after,
            _ => LOOP_NO_PROGRESS,
        };
        let cancel_token = smol_cancellation_token::CancellationToken::new();
        let persistent = cfg.shape != LoopShape::RebuiltSelect;
        Self {
            cfg,
            state: State::new(&ranges),
            writer: LoopWriter::new(cfg.write, cfg.queue, cfg.write_cost_ns),
            lifecycle,
            pending: futures::stream::pending(),
            data,
            data_done: false,
            cancel: Box::pin(cancel_token.clone().cancelled_owned()).fuse(),
            cancel_token,
            cancelled_at: Arc::default(),
            deadline: async_io::Timer::at(now + no_progress),
            last_progress: now,
            strategy: LoopTimer::new(cfg.timer, LOOP_STRATEGY_INTERVAL, persistent),
            throughput: LoopTimer::new(cfg.timer, LOOP_THROUGHPUT_INTERVAL, persistent),
            producers,
            finished: 0,
            frames: 0,
            strategy_ticks: 0,
            throughput_ticks: 0,
            checksum: 0,
        }
        .with_cancel_timer()
    }

    /// Arm the cancel signal for `Stop::CancelAfter`.
    fn with_cancel_timer(self) -> Self {
        if let Stop::CancelAfter(after) = self.cfg.stop {
            let token = self.cancel_token.clone();
            let cancelled_at = self.cancelled_at.clone();
            std::thread::spawn(move || {
                std::thread::sleep(after);
                *cancelled_at.lock().unwrap() = Some(Instant::now());
                token.cancel();
            });
        }
        self
    }

    #[inline]
    fn data_enabled(&self) -> bool {
        self.writer.can_write() && !self.data_done
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
        let writer_fut = if writer.in_flight == 0 {
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
            n = writer_fut => LoopEvent::Written(n),
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
        let mut cancel = cancel;
        let deadline = FutureExt::fuse(deadline);
        let writer_fut = poll_fn(|cx| writer.poll_done(cx)).fuse();
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
            n = writer_fut => LoopEvent::Written(n),
            _ = strategy_tick => LoopEvent::Strategy,
            _ = throughput_tick => LoopEvent::Throughput,
            event = runner_fut => LoopEvent::Runner(event),
        }
    }

    /// Priority: cancel > deadline > writer > strategy > throughput > runner.
    fn poll_event(&mut self, cx: &mut Context<'_>) -> Poll<LoopEvent> {
        if let Poll::Ready(()) = self.cancel.poll_unpin(cx) {
            return Poll::Ready(LoopEvent::Cancelled);
        }
        if Pin::new(&mut self.deadline).poll(cx).is_ready() {
            return Poll::Ready(LoopEvent::Deadline);
        }
        if let Poll::Ready(n) = self.writer.poll_done(cx) {
            return Poll::Ready(LoopEvent::Written(n));
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

    /// Returns the stop latency once the loop must end.
    fn handle(&mut self, event: LoopEvent) -> ControlFlow<Option<Duration>> {
        match event {
            LoopEvent::Cancelled => {
                let at = self.cancelled_at.lock().unwrap().expect("cancel time");
                return ControlFlow::Break(Some(at.elapsed()));
            }
            LoopEvent::Deadline => {
                let due = self.last_progress
                    + match self.cfg.stop {
                        Stop::DeadlineAfter(after) => after,
                        _ => LOOP_NO_PROGRESS,
                    };
                let now = Instant::now();
                if now >= due {
                    return ControlFlow::Break(Some(now - due));
                }
                self.deadline.set_at(due);
            }
            LoopEvent::Written(n) => {
                black_box(n);
            }
            LoopEvent::Strategy => {
                self.strategy_ticks += 1;
                black_box(self.state.runner_to_chunk.len());
            }
            LoopEvent::Throughput => {
                self.throughput_ticks += 1;
                black_box(self.state.sample());
            }
            LoopEvent::Runner(RunnerEvent::Lifecycle(Some((_, Lifecycle::Finished)))) => {
                self.finished += 1;
                if self.finished == self.cfg.runners {
                    return ControlFlow::Break(None);
                }
            }
            LoopEvent::Runner(RunnerEvent::Lifecycle(_) | RunnerEvent::Pending) => {}
            LoopEvent::Runner(RunnerEvent::Data(None)) => self.data_done = true,
            LoopEvent::Runner(RunnerEvent::Data(Some((id, mut bytes)))) => {
                self.frames += 1;
                let range = self.state.on_frame(id, bytes.len());
                bytes.truncate((range.end - range.start) as usize);
                if !range.is_empty() && !matches!(self.cfg.stop, Stop::DeadlineAfter(_)) {
                    self.last_progress = Instant::now();
                }
                self.checksum = self.checksum.wrapping_add(range.start);
                self.writer.dispatch(range, bytes);
            }
        }
        ControlFlow::Continue(())
    }

    async fn finish(self, stop_latency: Option<Duration>) -> LoopOutcome {
        let Self {
            mut state,
            writer,
            lifecycle,
            data,
            producers,
            frames,
            strategy_ticks,
            throughput_ticks,
            checksum,
            ..
        } = self;
        // Dropping the consumers releases producers blocked on a full channel.
        drop((lifecycle, data));
        writer.finish().await;
        for producer in producers {
            producer.await.expect("producer");
        }
        black_box(checksum);
        LoopOutcome {
            downloaded: state.sample().1,
            frames,
            strategy_ticks,
            throughput_ticks,
            stop_latency,
        }
    }
}

async fn download_loop(cfg: LoopConfig) -> LoopOutcome {
    let mut model = LoopModel::new(cfg);
    let stop_latency = loop {
        let event = match cfg.shape {
            LoopShape::RebuiltSelect => {
                model.writer.try_drain();
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
    if matches!(cfg.stop, Stop::Complete) {
        assert_eq!(outcome.downloaded, LOOP_TOTAL, "{cfg:?} lost bytes");
    }
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

/// Few active runners over many retained chunks: per-frame O(chunks) total vs a counter.
fn retained_chunks_state(retained: usize, active: usize) -> State {
    let ranges = split_ranges(LOOP_TOTAL, retained + active);
    let mut state = State::new(&ranges);
    // Retained chunks are finished and no longer have a runner.
    for id in active..retained + active {
        let key = state
            .runner_to_chunk
            .remove(&(id as RunnerId))
            .expect("chunk");
        let chunk = state.chunks.get_mut(&key).expect("chunk");
        chunk.downloaded.end = chunk.allocated.end;
        chunk.finished = true;
    }
    state
}

fn frames_with_sum(state: &mut State, active: usize, frames: usize) -> u64 {
    (0..frames)
        .map(|i| state.on_frame((i % active) as RunnerId, 1).end)
        .fold(0, u64::wrapping_add)
}

fn frames_with_counter(state: &mut State, active: usize, frames: usize) -> u64 {
    let mut accepted = 0u64;
    let mut acc = 0u64;
    for i in 0..frames {
        let range = state.on_frame_counted((i % active) as RunnerId, 1);
        accepted += range.end - range.start;
        acc = acc.wrapping_add(range.end);
    }
    acc.wrapping_add(accepted)
}

impl State {
    /// `on_frame` with the planned `accepted_bytes` counter instead of the O(chunks) sum.
    fn on_frame_counted(&mut self, runner_id: RunnerId, len: usize) -> Range<u64> {
        *self.meters.entry(runner_id).or_insert(0) += len;
        let key = self.runner_to_chunk[&runner_id];
        let chunk = self.chunks.get_mut(&key).expect("chunk");
        let new_end = (chunk.downloaded.end + len as u64).min(chunk.allocated.end);
        let prev = std::mem::replace(&mut chunk.downloaded.end, new_end);
        if chunk.downloaded.end >= chunk.allocated.end {
            chunk.finished = true;
        }
        prev..new_end
    }
}

// ───────────────────────────── allocation counter ─────────────────────────────

/// Counts allocations while `COUNT_ALLOCS` is set (profile mode only).
struct CountingAlloc;

static COUNT_ALLOCS: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards every call to `System` unchanged.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNT_ALLOCS.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNT_ALLOCS.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

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

/// Write path at equal backend work; `q1` forces the full-queue fallback.
fn bench_download_loop_writer(c: &mut Criterion) {
    let rt = Runtime::new().expect("Failed to create runtime");
    let mut group = loop_group(c, "download_loop_writer");
    for (label, queue, write_cost_ns) in [("q2048", WRITER_QUEUE, 0), ("q1/cost5us", 1, 5_000)] {
        for runners in [4usize, 16] {
            for write in [WriteMode::SpawnOneshot, WriteMode::DirectCompletion] {
                let cfg = LoopConfig {
                    write,
                    queue,
                    write_cost_ns,
                    ..LoopConfig::new(LoopShape::DirectPoll, TimerKind::Tokio, runners, 32 * 1024)
                };
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
        group.bench_with_input(
            BenchmarkId::new("progress_sum", format!("a{ACTIVE}/r{retained}")),
            &retained,
            |b, &r| {
                b.iter_batched_ref(
                    || retained_chunks_state(r, ACTIVE),
                    |state| frames_with_sum(state, ACTIVE, FRAMES),
                    criterion::BatchSize::LargeInput,
                )
            },
        );
        group.bench_with_input(
            BenchmarkId::new("progress_counter", format!("a{ACTIVE}/r{retained}")),
            &retained,
            |b, &r| {
                b.iter_batched_ref(
                    || retained_chunks_state(r, ACTIVE),
                    |state| frames_with_counter(state, ACTIVE, FRAMES),
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
    bench_download_loop_latency,
    bench_frame_bookkeeping
);

/// `COORD_PROFILE=loop:shape:timer:write:runners:frame_size:iterations[:queue[:write_cost_ns]]`
/// runs one download loop config outside criterion, printing allocations per frame
/// and full-queue fallbacks.
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
        queue: parts.get(6).map_or(WRITER_QUEUE, |s| parse("queue", s)),
        write_cost_ns: parts.get(7).map_or(0, |s| parse("write_cost_ns", s) as u64),
        ..LoopConfig::new(
            shape,
            timer,
            parse("runners", parts[3]),
            parse("frame_size", parts[4]),
        )
    };
    let iterations = parse("iterations", parts[5]);
    let rt = Runtime::new().expect("Failed to create runtime");
    let start = Instant::now();
    COUNT_ALLOCS.store(true, Ordering::Relaxed);
    let (mut frames, mut strategy_ticks, mut throughput_ticks) = (0, 0, 0);
    for _ in 0..iterations {
        let outcome = rt.block_on(download_loop(cfg));
        frames += outcome.frames;
        strategy_ticks += outcome.strategy_ticks;
        throughput_ticks += outcome.throughput_ticks;
    }
    COUNT_ALLOCS.store(false, Ordering::Relaxed);
    let elapsed = start.elapsed();
    let gib = (LOOP_TOTAL * iterations as u64) as f64 / (1u64 << 30) as f64;
    println!(
        "loop:{}: {gib:.2} GiB in {elapsed:?} -> {:.2} GiB/s wall, {:.1} ns/frame, {:.2} \
         allocs/frame, {} fallbacks, {strategy_ticks} strategy + {throughput_ticks} throughput \
         ticks",
        parts.join(":"),
        gib / elapsed.as_secs_f64(),
        elapsed.as_nanos() as f64 / frames as f64,
        ALLOCS.load(Ordering::Relaxed) as f64 / frames as f64,
        WRITE_FALLBACKS.load(Ordering::Relaxed),
    );
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
