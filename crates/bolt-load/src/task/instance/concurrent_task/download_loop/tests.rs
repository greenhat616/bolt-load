use std::{
    path::Path,
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

use async_waitgroup::WaitGroup;
use bolt_load_tests::adapter::simple::SimpleTestAdapter;
use futures::{
    future::{Either, FutureObj},
    task::{ArcWake, Spawn, SpawnError},
};
use pretty_assertions::assert_eq;
use smol_cancellation_token::CancellationToken;

use super::{
    super::{
        file_writer::{FileRangeWriter, FileRangeWriterKind, FileWriter},
        test_support::FailOnSpawnRuntime,
    },
    *,
};
use crate::{
    adapter::{AdapterError, DynBoltLoadAdapter},
    runner::TaskError,
    runtime::{
        DowncastLocalRuntime, DynTimer, ThreadedRuntime, ThreadedRuntimeImpl, Timer, TimerBuilder,
        TimerImpl,
    },
    task::{StatusReceiver, status::ProgressReporter},
};

#[tokio::test(flavor = "multi_thread")]
async fn test_split_after_failed_resize_does_not_allocate_tail() {
    let dir = tempfile::TempDir::new().unwrap();
    let file_writer = FileWriter::new(&dir.path().join("file.bin"), 1000)
        .await
        .unwrap();
    let rt = FailOnSpawnRuntime::new(usize::MAX);
    let spawns = rt.calls.clone();
    let spawner = RunnerSpawner::new(
        ThreadedRuntimeImpl::new_other_rt(rt),
        DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(1000).with_range_support(true)),
        WaitGroup::new(),
        CancellationToken::new(),
        crate::runner::DEFAULT_IDLE_TIMEOUT,
    );
    let (reporter, _status) = ProgressReporter::for_test();
    let mut download = DownloadLoop::new(
        1000,
        spawner,
        &file_writer,
        reporter,
        &TaskOptions::default(),
        CancellationToken::new(),
    );
    let mut reg = download.manager.allocate_runner_with_chunk(0..100).unwrap();

    // Half of this range would grow the chunk past its end.
    download.split(reg.runner_id, 0..400);

    assert_eq!(
        download
            .manager
            .get_runner_state(reg.runner_id)
            .unwrap()
            .allocated,
        0..100
    );
    assert_eq!(download.manager.get_available_ranges(), vec![100..1000]);
    assert_eq!(spawns.load(Ordering::SeqCst), 0, "no runner may be spawned");
    assert_eq!(reg.limits.try_recv(), None);
    drop(download);
    file_writer.finalize().await.unwrap();
}

// ── DownloadLoop: event priority and timer characterisation ─────────

/// How [`ScriptedRuntime`] treats spawned futures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpawnMode {
    /// Keep the future alive without ever polling it.
    Park,
    /// Reject the spawn.
    Fail,
}

#[derive(Clone, Copy, Debug)]
enum TimerKind {
    /// Custom timers that tick once per permit, see [`TimerScript`].
    Scripted,
    Tokio,
    #[cfg(feature = "smol")]
    Smol,
}

/// Hands out one scripted timer per loop timer and logs every tick.
#[derive(Clone)]
struct TimerScript {
    strategy: (async_channel::Sender<()>, async_channel::Receiver<()>),
    throughput: (async_channel::Sender<()>, async_channel::Receiver<()>),
    log: Arc<std::sync::Mutex<Vec<&'static str>>>,
}

impl TimerScript {
    fn new() -> Self {
        Self {
            strategy: async_channel::unbounded(),
            throughput: async_channel::unbounded(),
            log: Arc::default(),
        }
    }

    fn timer(&self, duration: Duration) -> ScriptedTimer {
        let (name, permits) = if duration == DEFAULT_STRATEGY_TICK_INTERVAL {
            ("strategy", self.strategy.1.clone())
        } else if duration == DEFAULT_SAMPLE_INTERVAL {
            ("throughput", self.throughput.1.clone())
        } else {
            panic!("unexpected timer interval: {duration:?}");
        };
        ScriptedTimer {
            name,
            permits,
            log: self.log.clone(),
        }
    }

    /// Let the named timer tick once more.
    fn permit(&self, name: &str) {
        let tx = match name {
            "strategy" => &self.strategy.0,
            "throughput" => &self.throughput.0,
            _ => panic!("unknown timer: {name}"),
        };
        tx.try_send(()).unwrap();
    }

    fn log(&self) -> Vec<&'static str> {
        self.log.lock().unwrap().clone()
    }
}

/// Ticks once per permit; dropping a pending tick keeps the permit.
struct ScriptedTimer {
    name: &'static str,
    permits: async_channel::Receiver<()>,
    log: Arc<std::sync::Mutex<Vec<&'static str>>>,
}

impl Timer for ScriptedTimer {
    async fn tick(&mut self) {
        if self.permits.recv().await.is_err() {
            futures::future::pending::<()>().await;
        }
        self.log.lock().unwrap().push(self.name);
    }
}

/// A runtime with a switchable spawn behavior and selectable timers.
#[derive(Clone)]
struct ScriptedRuntime {
    mode: Arc<std::sync::Mutex<SpawnMode>>,
    parked: Arc<std::sync::Mutex<Vec<FutureObj<'static, ()>>>>,
    timers: TimerKind,
    script: TimerScript,
}

impl ScriptedRuntime {
    fn new(timers: TimerKind) -> Self {
        Self {
            mode: Arc::new(std::sync::Mutex::new(SpawnMode::Park)),
            parked: Arc::default(),
            timers,
            script: TimerScript::new(),
        }
    }

    fn set_mode(&self, mode: SpawnMode) {
        *self.mode.lock().unwrap() = mode;
    }

    /// Take the parked futures, in spawn order.
    fn take_parked(&self) -> Vec<FutureObj<'static, ()>> {
        std::mem::take(&mut *self.parked.lock().unwrap())
    }
}

impl Spawn for ScriptedRuntime {
    fn spawn_obj(&self, future: FutureObj<'static, ()>) -> Result<(), SpawnError> {
        let mode = *self.mode.lock().unwrap();
        match mode {
            SpawnMode::Park => self.parked.lock().unwrap().push(future),
            SpawnMode::Fail => return Err(SpawnError::shutdown()),
        }
        Ok(())
    }
}

impl DowncastLocalRuntime for ScriptedRuntime {}

impl TimerBuilder for ScriptedRuntime {
    fn create_delayed_timer(&self, duration: Duration) -> TimerImpl {
        match self.timers {
            TimerKind::Scripted => {
                TimerImpl::Custom(DynTimer::new_box(self.script.timer(duration)))
            }
            TimerKind::Tokio => TimerImpl::Tokio(crate::runtime::TokioDelayedTimer::new(duration)),
            #[cfg(feature = "smol")]
            TimerKind::Smol => TimerImpl::Smol(crate::runtime::SmolDelayedTimer::new(duration)),
        }
    }
}

impl ThreadedRuntime for ScriptedRuntime {}

/// Run `test` on a tokio and, when enabled, a smol executor.
fn on_each_executor<F: Future<Output = ()>>(test: impl Fn() -> F) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(test());
    #[cfg(feature = "smol")]
    smol::block_on(test());
}

/// Await `fut`, failing the test if it takes longer than a few seconds.
async fn with_watchdog<T>(fut: impl Future<Output = T>) -> T {
    // Poll the timeout first: its wake-up must not rescue a future that lost its own.
    match futures::future::select(async_io::Timer::after(Duration::from_secs(10)), pin!(fut)).await
    {
        Either::Left(_) => panic!("the event loop never returned"),
        Either::Right((value, _)) => value,
    }
}

/// Counts the wake-ups of a manually polled future.
#[derive(Default)]
struct WakeCount(AtomicUsize);

impl WakeCount {
    fn get(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }

    /// Wait until the count exceeds `seen`, failing on a lost wake-up.
    fn wait_past(&self, seen: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.get() <= seen {
            assert!(Instant::now() < deadline, "lost wake-up");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl ArcWake for WakeCount {
    fn wake_by_ref(this: &Arc<Self>) {
        this.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Cancel,
    Deadline,
    Writer,
    Strategy,
    Throughput,
    Runner,
}

impl Source {
    fn name(self) -> &'static str {
        match self {
            Source::Cancel => "cancel",
            Source::Deadline => "deadline",
            Source::Writer => "writer",
            Source::Strategy => "strategy",
            Source::Throughput => "throughput",
            Source::Runner => "runner",
        }
    }

    /// Whether handling this event ends the loop.
    fn is_terminal(self) -> bool {
        !matches!(self, Source::Strategy | Source::Throughput)
    }
}

/// The source whose handling ended the loop.
fn outcome(result: &Result<()>) -> &'static str {
    let source = match result {
        Err(DownloadError::Cancelled) => return "cancel",
        Err(DownloadError::Transfer { source }) => source,
        _ => panic!("unexpected loop result: {result:?}"),
    };
    match &**source {
        TaskError::NoProgress { .. } => "deadline",
        TaskError::Other { message } if message.starts_with("write failed") => "writer",
        // A rejected runner spawn is an unretryable attempt failure.
        TaskError::Other { .. } => "runner",
        other => panic!("unexpected loop error: {other:?}"),
    }
}

const LOOP_TOTAL: u64 = 1000;

/// A [`DownloadLoop`] whose event sources are made ready on demand.
struct LoopHarness {
    download: DownloadLoop,
    file_writer: FileWriter,
    rt: ScriptedRuntime,
    cancel: CancellationToken,
    events: StatusReceiver,
}

impl LoopHarness {
    async fn new(timers: TimerKind, options: TaskOptions) -> Self {
        let rt = ScriptedRuntime::new(timers);
        let file_writer =
            FileWriter::new_with_kind(Path::new(""), LOOP_TOTAL, FileRangeWriterKind::Null)
                .await
                .unwrap();
        let spawner = RunnerSpawner::new(
            ThreadedRuntimeImpl::new_other_rt(rt.clone()),
            DynBoltLoadAdapter::new_arc(
                SimpleTestAdapter::new(LOOP_TOTAL as usize).with_range_support(true),
            ),
            WaitGroup::new(),
            CancellationToken::new(),
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        );
        let (reporter, events) = ProgressReporter::for_test();
        let cancel = CancellationToken::new();
        let mut download = DownloadLoop::new(
            LOOP_TOTAL,
            spawner,
            &file_writer,
            reporter,
            &options,
            cancel.clone(),
        );
        // This harness scripts spawn rejection and parked write tasks.
        // Direct submission is covered separately by the writer tests.
        download.writer = PendingWriter::new(
            Arc::new(file_writer.clone()),
            download.spawner.rt.clone(),
            download.max_concurrency,
        );
        Self {
            download,
            file_writer,
            rt,
            cancel,
            events,
        }
    }

    /// A harness whose deadline is either already due or far away.
    async fn scripted(deadline_due: bool) -> Self {
        let options = if deadline_due {
            TaskOptions::default().no_progress_timeout(Duration::from_nanos(1))
        } else {
            TaskOptions::default()
        };
        Self::new(TimerKind::Scripted, options).await
    }

    /// Make `source` ready without running the loop. The deadline is
    /// armed at construction, see [`Self::scripted`].
    fn make_ready(&mut self, source: Source) {
        match source {
            Source::Cancel => self.cancel.cancel(),
            Source::Deadline => {}
            Source::Writer => {
                // A rejected spawn queues a failed completion synchronously.
                self.rt.set_mode(SpawnMode::Fail);
                self.download
                    .writer
                    .write_range(0..1, bytes::Bytes::from_static(b"x"))
                    .unwrap();
                self.rt.set_mode(SpawnMode::Park);
            }
            Source::Strategy | Source::Throughput => self.rt.script.permit(source.name()),
            Source::Runner => {
                self.rt.set_mode(SpawnMode::Fail);
                self.download.spawn_runner(0..LOOP_TOTAL);
                self.rt.set_mode(SpawnMode::Park);
            }
        }
    }

    /// Run the loop until it returns on its own.
    async fn run_to_end(&mut self) -> Result<()> {
        with_watchdog(self.download.run()).await
    }

    async fn teardown(self) {
        drop(self.download);
        self.file_writer.finalize().await.unwrap();
    }
}

/// Whether progress was published since the last call.
fn progress_published(events: &mut StatusReceiver) -> bool {
    let changed = events.has_changed();
    events.mark_unchanged();
    changed
}

/// Pairs `(higher, lower)`: every pair of the six sources.
const PRIORITY_PAIRS: &[(Source, Source)] = &[
    (Source::Cancel, Source::Deadline),
    (Source::Cancel, Source::Writer),
    (Source::Deadline, Source::Writer),
    (Source::Cancel, Source::Strategy),
    (Source::Cancel, Source::Throughput),
    (Source::Cancel, Source::Runner),
    (Source::Deadline, Source::Strategy),
    (Source::Deadline, Source::Throughput),
    (Source::Deadline, Source::Runner),
    (Source::Writer, Source::Strategy),
    (Source::Writer, Source::Throughput),
    (Source::Writer, Source::Runner),
    (Source::Strategy, Source::Throughput),
    (Source::Strategy, Source::Runner),
    (Source::Throughput, Source::Runner),
];

/// With `high` and `low` ready together, `run` handles `high` first.
async fn assert_priority(high: Source, low: Source) {
    let mut harness =
        LoopHarness::scripted(Source::Deadline == high || Source::Deadline == low).await;
    harness.make_ready(low);
    harness.make_ready(high);
    let cancel = harness.cancel.clone();
    let result = {
        let mut run = pin!(harness.download.run());
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        // One poll handles every event that is already ready.
        match run.as_mut().poll(&mut cx) {
            Poll::Ready(result) => result,
            Poll::Pending => {
                assert!(
                    !high.is_terminal() && !low.is_terminal(),
                    "{high:?} vs {low:?}: parked with a terminal event ready"
                );
                cancel.cancel();
                let Poll::Ready(result) = run.as_mut().poll(&mut cx) else {
                    panic!("{high:?} vs {low:?}: parked after cancellation");
                };
                result
            }
        }
    };
    let log = harness.rt.script.log();
    let context = format!("{high:?} vs {low:?}: result {result:?}, ticks {log:?}");
    if high.is_terminal() {
        assert_eq!(outcome(&result), high.name(), "{context}");
        assert!(log.is_empty(), "{context}");
    } else if low.is_terminal() {
        assert_eq!(log, [high.name()], "{context}");
        assert_eq!(outcome(&result), low.name(), "{context}");
    } else {
        assert_eq!(log, [high.name(), low.name()], "{context}");
        assert_eq!(outcome(&result), "cancel", "{context}");
    }
    harness.teardown().await;
}

#[test]
fn test_loop_priority_pairs() {
    on_each_executor(|| async {
        for &(high, low) in PRIORITY_PAIRS {
            assert_priority(high, low).await;
        }
    });
}

#[test]
fn test_custom_timer_survives_other_events() {
    on_each_executor(|| async {
        let mut harness = LoopHarness::scripted(false).await;
        // Writes and a connecting runner stay parked until the timer waits.
        while !harness.download.writer.is_full() {
            harness
                .download
                .writer
                .write_range(0..1, bytes::Bytes::from_static(b"x"))
                .unwrap();
        }
        let writes = harness.rt.take_parked();
        harness.download.spawn_runner(0..LOOP_TOTAL);
        let connecting = harness.rt.take_parked();
        let script = harness.rt.script.clone();
        let cancel = harness.cancel.clone();
        let wakes = Arc::new(WakeCount::default());
        let waker = futures::task::waker(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        let result = {
            let mut run = pin!(harness.download.run());
            assert!(run.as_mut().poll(&mut cx).is_pending());

            // Writer completions and a lost connection arrive while it waits.
            let seen = wakes.get();
            let writers: Vec<_> = writes
                .into_iter()
                .map(|write| std::thread::spawn(move || futures::executor::block_on(write)))
                .collect();
            writers.into_iter().for_each(|w| w.join().unwrap());
            drop(connecting);
            wakes.wait_past(seen);
            assert!(run.as_mut().poll(&mut cx).is_pending());

            for _ in 0..3 {
                let seen = wakes.get();
                script.permit("strategy");
                wakes.wait_past(seen);
                assert!(run.as_mut().poll(&mut cx).is_pending());
            }

            let seen = wakes.get();
            script.permit("throughput");
            wakes.wait_past(seen);
            assert!(run.as_mut().poll(&mut cx).is_pending());

            cancel.cancel();
            let Poll::Ready(result) = run.as_mut().poll(&mut cx) else {
                panic!("parked after cancellation");
            };
            result
        };

        assert_eq!(outcome(&result), "cancel");
        assert_eq!(
            script.log(),
            ["strategy", "strategy", "strategy", "throughput"]
        );
        assert!(harness.download.writer.is_idle());
        let stats = harness
            .download
            .manager
            .runner_outcome_sampler_mut()
            .stats();
        assert_eq!(stats.connect_failed_retryable, 1, "{stats:?}");
        assert!(progress_published(&mut harness.events));
        harness.teardown().await;
    });
}

#[test]
fn test_timer_rearms_after_consecutive_ticks() {
    // Scripted: the loop parks between ticks and every permit is one more tick.
    on_each_executor(|| async {
        let mut harness = LoopHarness::scripted(false).await;
        let script = harness.rt.script.clone();
        let mut events = harness.events.clone();
        let cancel = harness.cancel.clone();
        let wakes = Arc::new(WakeCount::default());
        let waker = futures::task::waker(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        let result = {
            let mut run = pin!(harness.download.run());
            assert!(run.as_mut().poll(&mut cx).is_pending());
            for tick in 1..=3 {
                let seen = wakes.get();
                script.permit("throughput");
                wakes.wait_past(seen);
                assert!(run.as_mut().poll(&mut cx).is_pending());
                assert!(progress_published(&mut events), "tick {tick}");
            }
            cancel.cancel();
            let Poll::Ready(result) = run.as_mut().poll(&mut cx) else {
                panic!("parked after cancellation");
            };
            result
        };
        assert_eq!(outcome(&result), "cancel");
        harness.teardown().await;
    });

    // Native: the first tick is immediate, then one per sample interval.
    let native = |timers| async move {
        // Before the harness creates the timers the ticks are counted from.
        let started = Instant::now();
        let mut harness = LoopHarness::new(timers, TaskOptions::default()).await;
        let mut events = harness.events.clone();
        let cancel = harness.cancel.clone();
        drop(std::thread::spawn(move || {
            // Each change needs at least one tick.
            for _ in 0..3 {
                if events.changed_blocking().is_err() {
                    return;
                }
            }
            cancel.cancel();
        }));
        let result = harness.run_to_end().await;
        let elapsed = started.elapsed();
        assert_eq!(outcome(&result), "cancel");
        assert!(
            elapsed >= DEFAULT_SAMPLE_INTERVAL * 2,
            "{timers:?}: three ticks within {elapsed:?}"
        );
        harness.teardown().await;
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(native(TimerKind::Tokio));
    #[cfg(feature = "smol")]
    smol::block_on(native(TimerKind::Smol));
}

#[test]
fn test_deadline_moved_repeatedly_before_fire() {
    const TIMEOUT: Duration = Duration::from_millis(200);
    on_each_executor(|| async {
        let mut harness = LoopHarness::new(
            TimerKind::Scripted,
            TaskOptions::default().no_progress_timeout(TIMEOUT),
        )
        .await;
        // The deadline timer stays armed at the first deadline; each
        // progress report moves the policy deadline past it.
        std::thread::sleep(TIMEOUT / 2);
        harness.download.policy.on_progress(Instant::now());
        std::thread::sleep(TIMEOUT / 4);
        let last_progress = Instant::now();
        harness.download.policy.on_progress(last_progress);
        let policy = &harness.download.policy;
        assert!(
            policy
                .check(last_progress + TIMEOUT - Duration::from_nanos(1))
                .is_none()
        );
        assert!(policy.check(last_progress + TIMEOUT).is_some());

        let result = harness.run_to_end().await;

        // The timer first fires at the stale deadline, a quarter timeout after the
        // last progress; it must re-arm instead of failing there.
        let elapsed = last_progress.elapsed();
        assert_eq!(outcome(&result), "deadline");
        assert!(elapsed >= TIMEOUT, "fired early: {elapsed:?}");
        harness.teardown().await;
    });
}

#[tokio::test]
async fn test_clipped_frame_does_not_count_as_progress() {
    use super::super::runner_manager::{DownloadedChunk, TaskState};

    fn frame(range: std::ops::Range<u64>) -> RunnerTick {
        RunnerTick {
            state: TaskState::Downloading,
            downloaded: Some(DownloadedChunk {
                bytes: bytes::Bytes::from(vec![0; (range.end - range.start) as usize]),
                range,
            }),
            failure: None,
        }
    }

    let mut harness = LoopHarness::scripted(false).await;
    // Fail the writes synchronously, so no write task outlives the loop.
    harness.rt.set_mode(SpawnMode::Fail);
    let initial = harness.download.policy.deadline();
    std::thread::sleep(Duration::from_millis(1));
    assert!(
        harness
            .download
            .on_runner(frame(5..5))
            .unwrap()
            .is_continue()
    );
    assert_eq!(harness.download.policy.deadline(), initial);
    assert!(
        harness
            .download
            .on_runner(frame(0..5))
            .unwrap()
            .is_continue()
    );
    assert!(harness.download.policy.deadline() > initial);
    harness.teardown().await;
}

#[test]
fn test_cancel_after_registration_wakes_loop() {
    on_each_executor(|| async {
        let mut harness = LoopHarness::scripted(false).await;
        let cancel = harness.cancel.clone();
        let wakes = Arc::new(WakeCount::default());
        let waker = futures::task::waker(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        let result = {
            let mut run = pin!(harness.download.run());
            assert!(run.as_mut().poll(&mut cx).is_pending());
            let seen = wakes.get();
            cancel.cancel();
            assert!(wakes.get() > seen, "cancellation did not wake the loop");
            let Poll::Ready(result) = run.as_mut().poll(&mut cx) else {
                panic!("parked after cancellation");
            };
            result
        };
        assert_eq!(outcome(&result), "cancel");
        harness.teardown().await;
    });
}

// ── Connecting runners: control messages queued before the runner starts ─

/// A loop on `rt` whose runners connect through `adapter`, with the given
/// concurrency.
async fn connecting_loop(
    rt: ThreadedRuntimeImpl,
    adapter: Arc<crate::adapter::AnyAdapter>,
    total: u64,
    max_concurrency: usize,
) -> (DownloadLoop, FileWriter) {
    let file_writer = FileWriter::new_with_kind(Path::new(""), total, FileRangeWriterKind::Null)
        .await
        .unwrap();
    let spawner = RunnerSpawner::new(
        rt,
        adapter,
        WaitGroup::new(),
        CancellationToken::new(),
        crate::runner::DEFAULT_IDLE_TIMEOUT,
    );
    let (reporter, _status) = ProgressReporter::for_test();
    let mut download = DownloadLoop::new(
        total,
        spawner,
        &file_writer,
        reporter,
        &TaskOptions::default(),
        CancellationToken::new(),
    );
    download.max_concurrency = max_concurrency;
    download.manager = RunnerManager::new(total, max_concurrency);
    download.strategy = StrategyControl::new(max_concurrency);
    (download, file_writer)
}

/// A loop whose spawned runners are parked before their first poll, so they
/// never start connecting.
async fn parked_loop(total: u64, max_concurrency: usize) -> (DownloadLoop, FileWriter) {
    connecting_loop(
        ThreadedRuntimeImpl::new_other_rt(ScriptedRuntime::new(TimerKind::Scripted)),
        // Never connected, so its content is never read.
        DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(1).with_range_support(true)),
        total,
        max_concurrency,
    )
    .await
}

/// Reports the start of every range connect, then never completes it.
struct HandshakeAdapter {
    connecting: async_channel::Sender<u64>,
}

impl crate::adapter::BoltLoadAdapter for HandshakeAdapter {
    async fn is_range_stream_available(&self) -> bool {
        true
    }

    async fn retrieve_meta(&self) -> Result<crate::adapter::BoltLoadAdapterMeta, AdapterError> {
        unreachable!("the loop is given its total")
    }

    async fn full_stream(&self) -> Result<crate::adapter::AnyBytesStream, AdapterError> {
        unreachable!("the loop only connects ranges")
    }

    async fn range_stream(
        &self,
        start: u64,
        _end: u64,
    ) -> Result<crate::adapter::AnyBytesStream, AdapterError> {
        self.connecting.try_send(start).unwrap();
        futures::future::pending().await
    }
}

/// Split runner 0 more often than any bounded queue of limits would hold;
/// returns its latest limit.
fn split_connecting_runner_repeatedly(download: &mut DownloadLoop) -> u64 {
    const SPLITS: u32 = 12;
    let total = download.manager.total();
    for _ in 0..SPLITS {
        let incomplete = download
            .manager
            .get_runner_state(0)
            .unwrap()
            .incomplete_range();
        download.split(0, incomplete);
    }
    assert_eq!(
        download.manager.get_runner_state(0).unwrap().allocated,
        0..total >> SPLITS
    );
    assert_eq!(
        download.manager.allocated_runner_count(),
        SPLITS as usize + 1
    );
    total >> SPLITS
}

#[tokio::test]
async fn test_split_parked_runner_keeps_the_latest_limit() {
    const TOTAL: u64 = 1 << 30;
    let (mut download, file_writer) = parked_loop(TOTAL, 64).await;
    download.spawn_runner(0..TOTAL);
    let limit = split_connecting_runner_repeatedly(&mut download);
    // Never polled, so the runner is left the latest limit to read.
    assert_eq!(download.manager.unread_limit(0), Some(limit));
    drop(download);
    file_writer.finalize().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_split_handshaking_runner_keeps_the_latest_limit() {
    const TOTAL: u64 = 1 << 30;
    let (connecting, connects) = async_channel::unbounded();
    let (mut download, file_writer) = connecting_loop(
        ThreadedRuntimeImpl::new_tokio_rt(),
        DynBoltLoadAdapter::new_arc(HandshakeAdapter { connecting }),
        TOTAL,
        64,
    )
    .await;
    download.spawn_runner(0..TOTAL);
    assert_eq!(with_watchdog(connects.recv()).await.unwrap(), 0);
    split_connecting_runner_repeatedly(&mut download);
    // The connecting runner reads its limits.
    with_watchdog(async {
        while download.manager.unread_limit(0).is_some() {
            async_io::Timer::after(Duration::from_millis(1)).await;
        }
    })
    .await;

    let (cancel, wg) = (
        download.spawner.cancel_token.clone(),
        download.spawner.wg.clone(),
    );
    drop(download);
    cancel.cancel();
    with_watchdog(wg.wait()).await;
    file_writer.finalize().await.unwrap();
}

/// Drive the real strategy with one running runner while every runner it
/// spawns stays connecting; returns the runners each `SplitAllTask` targeted,
/// per round.
///
/// Every target must be split, and no connecting runner may be sent a limit.
async fn probe_strategy(max_concurrency: usize, ticks: usize) -> Vec<Vec<crate::task::RunnerId>> {
    use async_ringbuf::{
        AsyncHeapRb,
        traits::{Producer, Split},
    };

    use super::super::{runner_manager::RunnerChannels, strategy::StrategyAction};
    use crate::runner::{
        DATA_FRAME_CHANNEL_CAPACITY, DataFrame, LIFECYCLE_CHANNEL_CAPACITY, LifecycleEvent,
    };

    const TOTAL: u64 = 1 << 34;
    let (mut download, file_writer) = parked_loop(TOTAL, max_concurrency).await;
    let (_data_tx, data_rx) = AsyncHeapRb::<DataFrame>::new(DATA_FRAME_CHANNEL_CAPACITY).split();
    let (mut lifecycle_tx, lifecycle_rx) =
        AsyncHeapRb::<LifecycleEvent>::new(LIFECYCLE_CHANNEL_CAPACITY).split();
    let mut limits = None;
    download
        .manager
        .spawn_runner_with_chunk(0..TOTAL, |_, runner_limits| {
            limits = Some(runner_limits);
            Ok(RunnerChannels {
                lifecycle_rx,
                data_rx,
            })
        })
        .unwrap();
    let mut limits = limits.unwrap();
    lifecycle_tx.try_push(LifecycleEvent::Started).unwrap();
    download.manager.tick().await;
    assert_eq!(
        download.manager.get_runner_state(0).unwrap().status,
        super::super::chunk_planner::ChunkStatus::Running
    );

    // A steady 2 MiB/s keeps the dynamic strategy in its quick start stage.
    download.throughput.speed = 2.0 * 1024.0 * 1024.0;
    download.throughput.per_runner_speed = download.throughput.speed;
    let mut rounds = Vec::new();
    for _ in 0..ticks {
        // `on_strategy_tick`, observing each split.
        let Some((_, actions)) = download.strategy.execute(
            download.max_concurrency,
            download.throughput.speed,
            download.throughput.per_runner_speed,
            &mut download.manager,
        ) else {
            continue;
        };
        for action in actions {
            let StrategyAction::SplitAllTask(planned) = action else {
                download.apply(action);
                continue;
            };
            let targets = download.manager.get_incomplete_states(Some(planned));
            download.apply(action);
            for (runner_id, incomplete) in &targets {
                let state = download.manager.get_runner_state(*runner_id).unwrap();
                assert!(
                    state.allocated.end < incomplete.end,
                    "runner {runner_id} was not split"
                );
            }
            rounds.push(targets.into_iter().map(|(id, _)| id).collect());
        }
        // The running runner reads its limits.
        limits.try_recv();
        for runner_id in 1..max_concurrency {
            assert_eq!(
                download.manager.unread_limit(runner_id),
                None,
                "connecting runner {runner_id} was split"
            );
        }
    }
    drop(download);
    file_writer.finalize().await.unwrap();
    rounds
}

/// Each quick start round splits every running runner: only runner 0 runs,
/// so the runners it spawns are left to connect with their whole range.
#[tokio::test]
async fn test_strategy_splits_only_running_runners() {
    for max_concurrency in [4, 16, 64, 1024] {
        let rounds = probe_strategy(max_concurrency, 64).await;
        assert_eq!(rounds[0], [0], "max_concurrency {max_concurrency}");
        // Until runner 0 is too small to split.
        for targets in &rounds {
            assert!(
                targets.is_empty() || targets == &[0],
                "max_concurrency {max_concurrency}: {targets:?}"
            );
        }
    }
}
