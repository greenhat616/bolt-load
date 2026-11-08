//! Restarting slow runners in the loop (design §9.5, §14.15).
//!
//! Runners 0 to 2 run on the first three quarters of the file and are fed
//! through the manager. The runner judged runs on the last quarter: a
//! real runner, parked and polled by the test, or one whose channels and
//! stop the test holds. Every step gets its time passed in.

use std::{num::NonZeroU32, ops::Range};

use async_ringbuf::{
    AsyncHeapRb,
    traits::{Producer, Split},
};
use bytes::Bytes;
use futures::{StreamExt, future::FutureObj};
use pretty_assertions::assert_eq;

use super::{
    super::super::{
        chunk_planner::{ChunkStatus, Cooldown},
        runner_manager::{RunnerChannels, RunnerSlot},
        strategy::{MIN_RANGE_BACKOFF, SILENT_RESTART_AFTER},
    },
    *,
};
use crate::{
    runner::{
        DATA_FRAME_CHANNEL_CAPACITY, DataFrame, DataFrameSender, LIFECYCLE_CHANNEL_CAPACITY,
        LifecycleEvent, LifecycleSender,
    },
    task::{GrowthStrategy, RunnerId},
};

const TOTAL: u64 = 1 << 30;
const QUARTER: u64 = TOTAL / 4;
const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
const TICK: Duration = DEFAULT_STRATEGY_TICK_INTERVAL;
const NANO: Duration = Duration::from_nanos(1);
/// The runner on the last quarter.
const JUDGED: RunnerId = 3;

/// Connects at once, yields `first` bytes in one item, if any, then stalls
/// without closing, or with `fail` fails with an unretryable error.
#[derive(Clone, Copy)]
struct StallingAdapter {
    first: usize,
    fail: bool,
}

impl StallingAdapter {
    fn stalls_after(first: u64) -> Self {
        Self {
            first: first as usize,
            fail: false,
        }
    }
}

/// The unretryable error a [`StallingAdapter`] fails with.
fn unretryable() -> AdapterError {
    crate::adapter::UnretryableError::Io {
        source: Arc::new(std::io::Error::other("refused")),
    }
    .into()
}

impl crate::adapter::BoltLoadAdapter for StallingAdapter {
    async fn is_range_stream_available(&self) -> bool {
        true
    }

    async fn retrieve_meta(&self) -> Result<crate::adapter::BoltLoadAdapterMeta, AdapterError> {
        unreachable!("the loop is given its total")
    }

    async fn full_stream(&self) -> Result<crate::adapter::FullResponse, AdapterError> {
        unreachable!("the loop only connects ranges")
    }

    async fn range_stream(
        &self,
        _start: u64,
        _end: u64,
    ) -> Result<crate::adapter::AnyBytesStream, AdapterError> {
        let first = (self.first > 0).then(|| Ok(Bytes::from(vec![0; self.first])));
        let fail = self.fail.then(|| Err(unretryable()));
        Ok(futures::stream::iter(first.into_iter().chain(fail))
            .chain(futures::stream::pending())
            .boxed())
    }
}

/// A loop with `growth`, four connections and `options`, whose runners
/// connect through `body`; runners 0 to 2 run on the first three quarters.
async fn restart_loop(
    rt: &ScriptedRuntime,
    growth: GrowthStrategy,
    options: TaskOptions,
    body: StallingAdapter,
) -> (DownloadLoop, FileWriter, Vec<RunnerSlot>) {
    let options = options
        .max_connections(NonZeroUsize::new(4).unwrap())
        .growth_strategy(growth);
    let (mut download, file_writer) = loop_with_options(
        ThreadedRuntimeImpl::new_other_rt(rt.clone()),
        DynBoltLoadAdapter::new_arc(body),
        TOTAL,
        &options,
    )
    .await;
    let fast = (0..3)
        .map(|i| {
            download
                .manager
                .allocate_runner_with_chunk(i * QUARTER..(i + 1) * QUARTER)
                .unwrap()
        })
        .collect();
    (download, file_writer, fast)
}

/// Runners 0 to 2 deliver `bytes` each.
fn feed(download: &mut DownloadLoop, bytes: u64) {
    for runner_id in 0..3 {
        download.manager.update_progress(runner_id, bytes).unwrap();
    }
}

/// The only runner the loop spawned since the last call.
fn single_parked(rt: &ScriptedRuntime) -> FutureObj<'static, ()> {
    let mut parked = rt.take_parked();
    assert_eq!(parked.len(), 1, "expected one new runner");
    parked.pop().unwrap()
}

/// The only cooldown.
fn single_cooldown(download: &DownloadLoop) -> (Range<u64>, Cooldown) {
    let cooldowns = download.manager.cooldowns();
    let [cooldown] = cooldowns.as_slice() else {
        panic!("expected one cooldown, got {cooldowns:?}");
    };
    cooldown.clone()
}

/// Run the parked real runner until it waits, and handle its `Started`.
async fn start(download: &mut DownloadLoop, runner: &mut FutureObj<'static, ()>) {
    assert!(futures::poll!(runner).is_pending());
    download.manager.tick_lifecycle_only().await;
    assert_eq!(
        download.manager.get_runner_state(JUDGED).unwrap().status,
        ChunkStatus::Running
    );
}

/// A real runner delivers 64 KiB and stalls, while three others deliver
/// 0.6 MiB/s each. Silent since it last read, at the first tick, and
/// running there, it is silent for 4.5 s at the third (design §14.19), before its
/// slow window is full: a nanosecond before, nothing happens; then the
/// loop stops it through its own stop, and its slot is held until it
/// reports `Stopped`.
///
/// Its range comes back from the accepted bytes as a restart: one attempt
/// and nothing charged. One consecutive failure or one charged failure
/// would end the task here, so neither is counted. The range backs off,
/// and the wake-up at its end claims it for a new runner.
#[tokio::test]
async fn a_slow_runner_is_stopped_and_its_range_retried() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let options = TaskOptions::default()
        .max_consecutive_failures(1)
        .max_attempts_per_range(NonZeroU32::new(1).unwrap());
    let (mut download, file_writer, _fast) = restart_loop(
        &rt,
        GrowthStrategy::Doubling,
        options,
        StallingAdapter::stalls_after(96 * KIB),
    )
    .await;
    download.spawn_runner(3 * QUARTER..TOTAL, Instant::now());
    let mut runner = single_parked(&rt);
    start(&mut download, &mut runner).await;
    // Two full buffers; the rest stays in the runner's buffer.
    for frame in 0..2 {
        let tick = download.manager.tick().await;
        let start = 3 * QUARTER + frame * 32 * KIB;
        assert_eq!(tick.downloaded.unwrap().range, start..start + 32 * KIB);
    }
    // It stamped its reads by the clock; the test's clock starts after.
    let t0 = Instant::now();
    download.manager.read_activity(JUDGED).unwrap().note_at(t0);
    download.on_strategy_tick(t0);

    // Silent since `t0`, running at the sample there: stopped as silent,
    // before its slow window is full (design §14.19).
    for n in 1..=2 {
        feed(&mut download, MIB);
        download.on_strategy_tick(t0 + TICK * n);
        assert!(!download.manager.is_stopping(JUDGED), "tick {n}");
    }
    download.on_strategy_tick(t0 + SILENT_RESTART_AFTER - NANO);
    assert!(!download.manager.is_stopping(JUDGED));
    let stopped_at = t0 + SILENT_RESTART_AFTER;
    feed(&mut download, MIB);
    download.on_strategy_tick(stopped_at);
    assert!(download.manager.is_stopping(JUDGED));
    assert_eq!(download.manager.allocated_runner_count(), 4);

    // The runner sees its stop and reports it.
    assert!(futures::poll!(&mut runner).is_ready());
    let tick = download.manager.tick().await;
    let failure = tick.failure.as_ref().unwrap();
    assert!(failure.slow, "{failure:?}");
    let rest = 3 * QUARTER + 64 * KIB..TOTAL;
    assert_eq!(
        (
            failure.range.clone(),
            failure.received,
            failure.attempts,
            failure.charged
        ),
        (rest.clone(), 64 * KIB, 1, 0)
    );
    assert!(download.on_runner(tick, stopped_at).unwrap().is_continue());
    assert_eq!(download.manager.allocated_runner_count(), 3);
    let (range, cooldown) = single_cooldown(&download);
    assert_eq!(
        (range, cooldown.attempts, cooldown.charged),
        (rest.clone(), 1, 0)
    );
    let end = cooldown.not_before;
    assert!(
        stopped_at + MIN_RANGE_BACKOFF <= end && end <= stopped_at + 2 * MIN_RANGE_BACKOFF,
        "backs off for {:?}",
        end - stopped_at
    );
    assert_eq!(download.deadline.at(), end);

    download.on_deadline(end - NANO).unwrap();
    assert_eq!(download.manager.allocated_runner_count(), 3);
    download.on_deadline(end).unwrap();
    let state = download.manager.get_runner_state(JUDGED).unwrap();
    assert_eq!(
        (
            state.allocated.clone(),
            state.attempts,
            state.charged,
            state.status.clone()
        ),
        (rest, 1, 0, ChunkStatus::Pending)
    );
    drop(single_parked(&rt));

    drop(runner);
    drop(download);
    file_writer.finalize().await.unwrap();
}

/// A runner that never delivers, stopped as silent, is charged at every
/// stop, as a timeout without data is: the third stop exhausts its range's budget of three,
/// with the timeout as its last error. Consecutive failures are allowed
/// plenty, so only the budget ends the task.
#[tokio::test]
async fn a_slow_runner_without_data_exhausts_its_range() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let options = TaskOptions::default()
        .max_consecutive_failures(100)
        .max_attempts_per_range(NonZeroU32::new(3).unwrap());
    let (mut download, file_writer, _fast) = restart_loop(
        &rt,
        GrowthStrategy::Doubling,
        options,
        StallingAdapter::stalls_after(0),
    )
    .await;
    let mut now = Instant::now();
    download.spawn_runner(3 * QUARTER..TOTAL, now);
    for attempt in 1..=3 {
        let mut runner = single_parked(&rt);
        start(&mut download, &mut runner).await;
        let started = now;
        download.on_strategy_tick(started);
        // At 1.5 s ticks, it is silent for 4.5 s at the third (design
        // §14.19).
        for n in 1..=3 {
            feed(&mut download, MIB);
            now = started + TICK * n;
            download.on_strategy_tick(now);
            assert_eq!(
                download.manager.is_stopping(JUDGED),
                n == 3,
                "attempt {attempt}, tick {n}"
            );
        }
        assert!(futures::poll!(&mut runner).is_ready());
        let tick = download.manager.tick().await;
        let failure = tick.failure.as_ref().unwrap();
        assert_eq!(
            (
                failure.slow,
                failure.received,
                failure.attempts,
                failure.charged
            ),
            (true, 0, attempt, attempt)
        );
        if attempt == 3 {
            match download.on_runner(tick, now) {
                Err(DownloadError::Transfer { source }) => match &*source {
                    TaskError::RangeExhausted {
                        range,
                        attempts: 3,
                        source,
                    } => {
                        assert_eq!(*range, 3 * QUARTER..TOTAL);
                        assert!(matches!(**source, TaskError::Timeout), "{source:?}");
                    }
                    other => panic!("expected RangeExhausted, got {other:?}"),
                },
                other => panic!("expected RangeExhausted, got {other:?}"),
            }
            break;
        }
        assert!(download.on_runner(tick, now).unwrap().is_continue());
        let (_, cooldown) = single_cooldown(&download);
        assert_eq!(cooldown.charged, attempt);
        now = cooldown.not_before;
        download.on_deadline(now).unwrap();
        assert_eq!(
            download.manager.get_runner_state(JUDGED).unwrap().charged,
            attempt
        );
    }

    drop(download);
    file_writer.finalize().await.unwrap();
}

/// The ends of a runner the test drives, and its stop.
struct TestRunner {
    lifecycle: LifecycleSender,
    _data: DataFrameSender,
    stop: smol_cancellation_token::CancellationToken,
}

/// A running runner on the last quarter whose ends and stop the test
/// holds.
async fn spawn_test_runner(download: &mut DownloadLoop) -> TestRunner {
    let (mut lifecycle, lifecycle_rx) =
        AsyncHeapRb::<LifecycleEvent>::new(LIFECYCLE_CHANNEL_CAPACITY).split();
    let (data, data_rx) = AsyncHeapRb::<DataFrame>::new(DATA_FRAME_CHANNEL_CAPACITY).split();
    let stop = CancellationToken::new();
    download
        .manager
        .spawn_runner_with_chunk(3 * QUARTER..TOTAL, |_, _limits| {
            Ok(RunnerChannels {
                lifecycle_rx,
                data_rx,
                stop: stop.clone(),

                reads: Default::default(),
            })
        })
        .unwrap();
    lifecycle.try_push(LifecycleEvent::Started).unwrap();
    download.manager.tick().await;
    TestRunner {
        lifecycle,
        _data: data,
        stop,
    }
}

/// Feed runners 0 to 2 for ticks 1 to `ticks` after `t0`, on top of a
/// first tick at `t0`.
fn run_ticks(download: &mut DownloadLoop, t0: Instant, ticks: u32) {
    download.on_strategy_tick(t0);
    for n in 1..=ticks {
        feed(download, MIB);
        download.on_strategy_tick(t0 + TICK * n);
    }
}

/// A runner that still delivers, a frame every tick, is never silent; over
/// a full window it is slow next to the fast ones, and the slow path stops
/// it once the window is full (design §9.5, §14.19).
#[tokio::test]
async fn a_slow_runner_that_delivers_is_stopped_on_its_window() {
    const FRAME: u64 = crate::runner::BUFFER_SIZE as u64;
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, file_writer, _fast) = restart_loop(
        &rt,
        GrowthStrategy::Doubling,
        TaskOptions::default(),
        StallingAdapter::stalls_after(0),
    )
    .await;
    let runner = spawn_test_runner(&mut download).await;
    let t0 = Instant::now();
    download.manager.note_delivery(JUDGED, t0);
    let reads = download.manager.read_activity(JUDGED).unwrap().clone();
    reads.note_at(t0);
    download.on_strategy_tick(t0);
    // At 1.5 s ticks, the window is full at the seventh.
    for n in 1..=7 {
        let now = t0 + TICK * n;
        feed(&mut download, MIB);
        download.manager.update_progress(JUDGED, FRAME).unwrap();
        download.manager.note_delivery(JUDGED, now);
        reads.note_at(now);
        download.on_strategy_tick(now);
        assert_eq!(download.manager.is_stopping(JUDGED), n == 7, "tick {n}");
    }
    assert!(runner.stop.is_cancelled());
    drop(download);
    file_writer.finalize().await.unwrap();
}

/// A runner that completes its chunk after the loop decided to stop it,
/// and before it noticed, is no failure, whether it reports that it
/// finished or that it was cancelled: nothing is released or backed off,
/// and its slot is freed once. Until then, the next tick neither judges
/// it again nor grows from it.
#[tokio::test]
async fn a_runner_that_completes_while_stopping_is_no_failure() {
    for finished in [true, false] {
        let rt = ScriptedRuntime::new(TimerKind::Scripted);
        let (mut download, file_writer, _fast) = restart_loop(
            &rt,
            GrowthStrategy::Doubling,
            TaskOptions::default(),
            StallingAdapter::stalls_after(0),
        )
        .await;
        let mut runner = spawn_test_runner(&mut download).await;
        let t0 = Instant::now();
        run_ticks(&mut download, t0, 7);
        assert!(runner.stop.is_cancelled() && download.manager.is_stopping(JUDGED));
        // It has the most left, but is no growth's to split.
        let largest = download.manager.largest_running_chunk().unwrap();
        assert_ne!(largest.0, JUDGED);
        feed(&mut download, MIB);
        download.on_strategy_tick(t0 + TICK * 8);
        assert_eq!(
            download.manager.get_runner_state(JUDGED).unwrap().allocated,
            3 * QUARTER..TOTAL
        );

        // Its last bytes are accepted, then it stops.
        download.manager.update_progress(JUDGED, QUARTER).unwrap();
        let stopped = if finished {
            LifecycleEvent::finished()
        } else {
            LifecycleEvent::failed(TaskError::Cancelled)
        };
        runner.lifecycle.try_push(stopped).unwrap();
        let tick = download.manager.tick().await;
        assert!(tick.failure.is_none(), "finished {finished}: {tick:?}");
        assert!(
            download
                .on_runner(tick, t0 + TICK * 8)
                .unwrap()
                .is_continue()
        );
        assert_eq!(download.manager.get_available_ranges(), []);
        assert_eq!(download.manager.cooldowns(), []);
        assert_eq!(download.manager.allocated_runner_count(), 3);
        assert_eq!(download.next_retry(t0 + TICK * 8), None);
        drop(download);
        file_writer.finalize().await.unwrap();
    }
}

/// The same stalled runner among the same fast ones is stopped under
/// `Doubling`, as silent (design §14.19), and never under `Legacy`, which
/// does not even sample the rates.
#[tokio::test]
async fn legacy_never_restarts() {
    for growth in [GrowthStrategy::Legacy, GrowthStrategy::Doubling] {
        let rt = ScriptedRuntime::new(TimerKind::Scripted);
        let (mut download, file_writer, _fast) = restart_loop(
            &rt,
            growth,
            TaskOptions::default(),
            StallingAdapter::stalls_after(0),
        )
        .await;
        let runner = spawn_test_runner(&mut download).await;
        // 30 s of ticks.
        run_ticks(&mut download, Instant::now(), 20);
        let doubling = growth == GrowthStrategy::Doubling;
        assert_eq!(download.rates.is_some(), doubling);
        assert_eq!(runner.stop.is_cancelled(), doubling, "{growth:?}");
        assert_eq!(download.manager.is_stopping(JUDGED), doubling, "{growth:?}");
        assert_eq!(
            download.manager.get_runner_state(JUDGED).unwrap().status,
            ChunkStatus::Running
        );
        drop(download);
        file_writer.finalize().await.unwrap();
    }
}

/// The stop does not replace an error the runner already holds. Its stream
/// fails with an unretryable error after 96 KiB, while its frames still
/// wait to be taken; the loop then stops it for being slow (frames queued
/// keep it from the silent restart, design §14.19, not from the slow one).
/// It goes on waiting for its frames, then reports its own error, which
/// ends the task: not a slow attempt, retried after a backoff.
#[tokio::test]
async fn a_stop_keeps_the_error_a_runner_holds() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let body = StallingAdapter {
        first: (96 * KIB) as usize,
        fail: true,
    };
    let (mut download, file_writer, _fast) =
        restart_loop(&rt, GrowthStrategy::Doubling, TaskOptions::default(), body).await;
    let t0 = Instant::now();
    download.spawn_runner(3 * QUARTER..TOTAL, t0);
    let mut runner = single_parked(&rt);
    // It fails at once and waits for its three frames to be taken.
    start(&mut download, &mut runner).await;
    run_ticks(&mut download, t0, 6);
    assert!(
        !download.manager.is_stopping(JUDGED),
        "not silent: frames queued"
    );
    feed(&mut download, MIB);
    download.on_strategy_tick(t0 + TICK * 7);
    assert!(
        download.manager.is_stopping(JUDGED),
        "slow, at its full window"
    );

    assert!(futures::poll!(&mut runner).is_pending(), "still draining");
    for _ in 0..3 {
        assert!(download.manager.tick().await.downloaded.is_some());
    }
    assert!(futures::poll!(&mut runner).is_ready());
    let tick = download.manager.tick().await;
    let failure = tick.failure.as_ref().unwrap();
    assert!(
        !failure.slow && !failure.retryable && failure.error.is_stream_error(),
        "{failure:?}"
    );
    assert_eq!(failure.received, 96 * KIB);
    match download.on_runner(tick, t0 + TICK * 7) {
        Err(DownloadError::Transfer { source }) => {
            assert!(source.is_stream_error(), "{source:?}");
        }
        other => panic!("expected the stream error, got {other:?}"),
    }
    assert_eq!(download.manager.cooldowns(), []);

    drop(runner);
    drop(download);
    file_writer.finalize().await.unwrap();
}

/// Each range's body comes through the channel registered for its start,
/// and stalls while nothing is sent.
struct FedAdapter {
    bodies: std::sync::Mutex<std::collections::HashMap<u64, async_channel::Receiver<Bytes>>>,
}

impl crate::adapter::BoltLoadAdapter for FedAdapter {
    async fn is_range_stream_available(&self) -> bool {
        true
    }

    async fn retrieve_meta(&self) -> Result<crate::adapter::BoltLoadAdapterMeta, AdapterError> {
        unreachable!("the loop is given its total")
    }

    async fn full_stream(&self) -> Result<crate::adapter::FullResponse, AdapterError> {
        unreachable!("the loop only connects ranges")
    }

    async fn range_stream(
        &self,
        start: u64,
        _end: u64,
    ) -> Result<crate::adapter::AnyBytesStream, AdapterError> {
        let body = self.bodies.lock().unwrap().remove(&start).unwrap();
        Ok(body.map(Ok::<_, AdapterError>).boxed())
    }
}

/// Three real runners at the same 2 KiB/s, whose buffers hold 30, 15 and
/// 0 KiB when the window starts. A runner delivers a frame of 32 KiB at a
/// time, so over the first window two deliver a frame and the third none,
/// and over every window each delivers one or none. None is slow, over
/// 30 s of ticks: a frame either way is allowed.
#[tokio::test]
async fn runners_at_the_same_low_rate_are_never_restarted() {
    const FRAME: u64 = crate::runner::BUFFER_SIZE as u64;
    let ranges = [0..QUARTER, QUARTER..2 * QUARTER, 2 * QUARTER..TOTAL];
    let mut senders = Vec::new();
    let mut bodies = std::collections::HashMap::new();
    for range in &ranges {
        let (sender, body) = async_channel::unbounded();
        senders.push(sender);
        bodies.insert(range.start, body);
    }
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let options = TaskOptions::default()
        .max_connections(NonZeroUsize::new(3).unwrap())
        .growth_strategy(GrowthStrategy::Doubling);
    let (mut download, file_writer) = loop_with_options(
        ThreadedRuntimeImpl::new_other_rt(rt.clone()),
        DynBoltLoadAdapter::new_arc(FedAdapter {
            bodies: std::sync::Mutex::new(bodies),
        }),
        TOTAL,
        &options,
    )
    .await;
    let t0 = Instant::now();
    for range in ranges {
        download.spawn_runner(range, t0);
    }
    let mut runners = rt.take_parked();
    for (sender, held) in senders.iter().zip([30, 15, 0]) {
        sender.try_send(Bytes::from(vec![0; held * 1024])).unwrap();
    }
    for runner in &mut runners {
        assert!(futures::poll!(runner).is_pending());
    }
    for _ in 0..3 {
        download.manager.tick_lifecycle_only().await;
    }
    download.on_strategy_tick(t0);

    for n in 1..=20 {
        // 3 KiB a tick of 1.5 s.
        for sender in &senders {
            sender.try_send(Bytes::from(vec![0; 3 * 1024])).unwrap();
        }
        for runner in &mut runners {
            assert!(futures::poll!(runner).is_pending());
        }
        // They stamped their reads by the clock, a tick of the test's
        // clock behind; they read during the tick.
        for runner_id in 0..3 {
            download
                .manager
                .read_activity(runner_id)
                .unwrap()
                .note_at(t0 + TICK * n);
        }
        while let Poll::Ready(tick) = futures::poll!(pin!(download.manager.tick())) {
            assert!(tick.failure.is_none(), "{tick:?}");
        }
        download.on_strategy_tick(t0 + TICK * n);
        if n == 7 {
            let rates = download.rates.as_ref().unwrap();
            let mut bytes: Vec<_> = rates.window_bytes().into_iter().map(|(_, b)| b).collect();
            bytes.sort_unstable();
            assert_eq!(bytes, [0, FRAME, FRAME]);
        }
        for runner_id in 0..3 {
            assert!(
                !download.manager.is_stopping(runner_id),
                "tick {n}: runner {runner_id}"
            );
        }
    }

    drop(runners);
    drop(download);
    file_writer.finalize().await.unwrap();
}
