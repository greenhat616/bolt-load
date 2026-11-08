use std::{
    num::NonZeroUsize,
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
    download.split(reg.runner_id, 0..400, Instant::now());

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
    // The slot reserved for the second half is given back.
    assert_eq!(download.manager.allocated_runner_count(), 1);
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
        // The write `make_ready` dispatched, whose spawn was rejected.
        TaskError::Write { range, .. } if *range == (0..1) => "writer",
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
                self.download.spawn_runner(0..LOOP_TOTAL, Instant::now());
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
        harness.download.spawn_runner(0..LOOP_TOTAL, Instant::now());
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
                runner_id: 0,
                bytes: bytes::Bytes::from(vec![0; (range.end - range.start) as usize]),
                range,
            }),
            failure: None,
            side_failure: None,
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
            .on_runner(frame(5..5), Instant::now())
            .unwrap()
            .is_continue()
    );
    assert_eq!(harness.download.policy.deadline(), initial);
    assert!(
        harness
            .download
            .on_runner(frame(0..5), Instant::now())
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

/// A loop on `rt` whose runners connect through `adapter`, built as a task
/// does with `max_concurrency` as [`TaskOptions::max_connections`].
async fn connecting_loop(
    rt: ThreadedRuntimeImpl,
    adapter: Arc<crate::adapter::AnyAdapter>,
    total: u64,
    max_concurrency: usize,
) -> (DownloadLoop, FileWriter) {
    let options =
        TaskOptions::default().max_connections(NonZeroUsize::new(max_concurrency).unwrap());
    loop_with_options(rt, adapter, total, &options).await
}

/// A loop on `rt` whose runners connect through `adapter`, built as a task
/// does with `options`.
async fn loop_with_options(
    rt: ThreadedRuntimeImpl,
    adapter: Arc<crate::adapter::AnyAdapter>,
    total: u64,
    options: &TaskOptions,
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
    let download = DownloadLoop::new(
        total,
        spawner,
        &file_writer,
        reporter,
        options,
        CancellationToken::new(),
    );
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

    async fn full_stream(&self) -> Result<crate::adapter::FullResponse, AdapterError> {
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
        download.split(0, incomplete, Instant::now());
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
    download.spawn_runner(0..TOTAL, Instant::now());
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
    download.spawn_runner(0..TOTAL, Instant::now());
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

/// Grow by as many runners as allowed, a round at a time, with one running
/// runner while every runner it spawns stays connecting; returns the
/// runners each round added.
///
/// Every split must take runner 0's chunk, the only running one: no
/// connecting runner may be sent a limit, and there are never more runners
/// than allowed.
async fn probe_growth(max_concurrency: usize, rounds: usize) -> Vec<usize> {
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
                stop: Default::default(),

                reads: Default::default(),
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

    // 2 MiB/s, before runner 0 has a rate of its own: 4 MiB planned.
    download.throughput.per_runner_speed = 2.0 * 1024.0 * 1024.0;
    let mut added = Vec::new();
    for _ in 0..rounds {
        let before = download.manager.get_active_runners_count();
        let end = download.manager.get_runner_state(0).unwrap().allocated.end;
        download.apply(StrategyAction::Grow(max_concurrency), Instant::now());
        let after = download.manager.get_active_runners_count();
        assert!(
            after <= max_concurrency,
            "{after} runners, at most {max_concurrency}"
        );
        if after > before {
            assert!(
                download.manager.get_runner_state(0).unwrap().allocated.end < end,
                "runner 0 was not split"
            );
        }
        added.push(after - before);
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
    added
}

/// With one connection allowed, growth never splits the only runner.
#[tokio::test]
async fn test_one_connection_is_never_split() {
    let added = probe_growth(1, 64).await;
    assert!(added.iter().all(|&added| added == 0), "{added:?}");
}

/// Growth splits only running runners: only runner 0 runs, so the runners
/// it spawns are left to connect with their whole range. The first round
/// splits runner 0 for every free slot, until what it has left is under the
/// planned 4 MiB: 16 GiB halved 13 times.
#[tokio::test]
async fn test_growth_splits_only_running_runners() {
    const SPLITS: usize = 13;
    for max_concurrency in [4, 16, 64, 1024] {
        let added = probe_growth(max_concurrency, 8).await;
        assert_eq!(
            added[0],
            (max_concurrency - 1).min(SPLITS),
            "max_concurrency {max_concurrency}"
        );
        assert!(
            added[1..].iter().all(|&added| added == 0),
            "max_concurrency {max_concurrency}: {added:?}"
        );
    }
}

// ── Splitting within the connection limit ───────────────────────────

/// With every runner slot taken, growing, splitting or creating a runner
/// changes nothing; a freed slot is used again.
#[tokio::test]
#[expect(clippy::single_range_in_vec_init, reason = "a list holding one range")]
async fn test_no_free_slot_skips_splits_and_new_runners() {
    use super::super::strategy::StrategyAction;

    const TOTAL: u64 = 1 << 30;
    const HALF: u64 = TOTAL / 2;
    let (mut download, file_writer) = parked_loop(TOTAL, 2).await;
    let mut first = download
        .manager
        .allocate_runner_with_chunk(0..HALF)
        .unwrap();
    let mut second = download
        .manager
        .allocate_runner_with_chunk(HALF..TOTAL)
        .unwrap();

    download.throughput.per_runner_speed = 1.0;
    download.apply(StrategyAction::Grow(2), Instant::now());
    assert!(!download.split(first.runner_id, 0..HALF, Instant::now()));
    let allocated = |download: &DownloadLoop, runner_id| {
        download
            .manager
            .get_runner_state(runner_id)
            .unwrap()
            .allocated
            .clone()
    };
    assert_eq!(allocated(&download, first.runner_id), 0..HALF);
    assert_eq!(allocated(&download, second.runner_id), HALF..TOTAL);
    assert_eq!(first.limits.try_recv(), None);
    assert_eq!(second.limits.try_recv(), None);
    assert_eq!(download.manager.allocated_runner_count(), 2);
    assert_eq!(download.manager.get_available_ranges(), []);

    // The second runner fails, and its slot is taken before a runner is
    // created for its range.
    download
        .manager
        .release_runner_full(second.runner_id, false)
        .unwrap();
    let taken = download.manager.try_reserve_slot().unwrap();
    download.apply(StrategyAction::CreateTask(HALF..TOTAL), Instant::now());
    assert_eq!(download.manager.get_available_ranges(), [HALF..TOTAL]);

    download.manager.release(taken);
    download.apply(StrategyAction::CreateTask(HALF..TOTAL), Instant::now());
    assert_eq!(download.manager.get_available_ranges(), []);
    assert_eq!(download.manager.allocated_runner_count(), 2);
    drop(download);
    file_writer.finalize().await.unwrap();
}

/// A runner whose chunk is complete keeps its slot until it stops, so
/// growth may see room that the slots do not have.
#[tokio::test]
async fn test_strategy_tick_while_finished_runners_keep_their_slots() {
    use super::super::strategy::StrategyAction;

    const TOTAL: u64 = 1 << 34;
    const QUARTER: u64 = TOTAL / 4;
    let (mut download, file_writer) = parked_loop(TOTAL, 4).await;
    let mut slots: Vec<_> = (0..4)
        .map(|i| {
            download
                .manager
                .allocate_runner_with_chunk(i * QUARTER..(i + 1) * QUARTER)
                .unwrap()
        })
        .collect();
    // Runners 2 and 3 received their whole chunk but have not stopped.
    for runner_id in [2, 3] {
        download
            .manager
            .update_progress(runner_id, QUARTER)
            .unwrap();
    }
    assert_eq!(download.manager.get_active_runners_count(), 2);
    // 1 MiB/s from each runner.
    download.throughput.per_runner_speed = 1024.0 * 1024.0;

    // Two active runners of four allowed: growth splits, but no slot is
    // free.
    download.apply(StrategyAction::Grow(2), Instant::now());
    for (runner_id, slot) in slots.iter_mut().enumerate() {
        assert_eq!(
            download
                .manager
                .get_runner_state(runner_id)
                .unwrap()
                .allocated,
            runner_id as u64 * QUARTER..(runner_id as u64 + 1) * QUARTER
        );
        assert_eq!(slot.limits.try_recv(), None, "runner {runner_id}");
    }
    assert_eq!(download.manager.get_available_ranges(), []);

    // Runner 3 stops: one split takes its slot, the next finds none.
    download.manager.release_runner_full(3, true).unwrap();
    download.apply(StrategyAction::Grow(2), Instant::now());
    let split = |download: &DownloadLoop, runner_id: usize| {
        download
            .manager
            .get_runner_state(runner_id)
            .unwrap()
            .allocated
            .end
            < (runner_id as u64 + 1) * QUARTER
    };
    // Runners 0 and 1 have as much left: one of them is split.
    assert_ne!(split(&download, 0), split(&download, 1));
    let limits: Vec<_> = slots[..2]
        .iter_mut()
        .map(|slot| slot.limits.try_recv())
        .collect();
    assert_eq!(limits.iter().filter(|limit| limit.is_some()).count(), 1);
    assert_eq!(download.manager.allocated_runner_count(), 4);
    assert_eq!(download.manager.get_available_ranges(), []);
    drop(download);
    file_writer.finalize().await.unwrap();
}

/// A split whose new runner the runtime rejects keeps the new slot until
/// the loop handles the rejection from the runner manager's lifecycle
/// poll, then reclaims that slot once: a second split in between keeps
/// its own. The original runner stays shrunk and the tail becomes
/// assignable again.
///
/// A rejected spawn is an unretryable connect failure, so it ends the
/// task; the loop's state afterwards is what it leaves for its teardown.
/// The loop creates no runner after that; the test creates one by hand
/// to probe that the freed slot and the tail can be allocated again.
#[tokio::test]
#[expect(clippy::single_range_in_vec_init, reason = "a list holding one range")]
async fn test_rejected_split_spawn_reclaims_its_slot_once() {
    use async_ringbuf::{
        AsyncHeapRb,
        traits::{Producer, Split},
    };

    use super::super::{
        chunk_planner::ChunkStatus, runner_manager::RunnerChannels, strategy::StrategyAction,
    };
    use crate::runner::{
        DATA_FRAME_CHANNEL_CAPACITY, DataFrame, LIFECYCLE_CHANNEL_CAPACITY, LifecycleEvent,
    };

    const TOTAL: u64 = 1 << 30;
    const HALF: u64 = TOTAL / 2;
    const QUARTER: u64 = TOTAL / 4;
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, file_writer) = connecting_loop(
        ThreadedRuntimeImpl::new_other_rt(rt.clone()),
        DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(1).with_range_support(true)),
        TOTAL,
        3,
    )
    .await;
    // Runner 0 starts through its lifecycle channel.
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
                stop: Default::default(),

                reads: Default::default(),
            })
        })
        .unwrap();
    let mut limits = limits.unwrap();
    lifecycle_tx.try_push(LifecycleEvent::Started).unwrap();
    download.manager.tick().await;
    let state = |download: &DownloadLoop, runner_id| {
        download
            .manager
            .get_runner_state(runner_id)
            .map(|state| (state.allocated.clone(), state.status.clone()))
    };
    assert_eq!(state(&download, 0), Some((0..TOTAL, ChunkStatus::Running)));

    // The runtime rejects the split's runner, which keeps its slot until
    // the rejection is handled.
    rt.set_mode(SpawnMode::Fail);
    download.split(0, 0..TOTAL, Instant::now());
    rt.set_mode(SpawnMode::Park);
    assert_eq!(limits.try_recv(), Some(HALF));
    assert_eq!(state(&download, 0), Some((0..HALF, ChunkStatus::Running)));
    assert_eq!(
        state(&download, 1),
        Some((HALF..TOTAL, ChunkStatus::Pending))
    );
    assert_eq!(download.manager.allocated_runner_count(), 2);

    // Meanwhile another split takes the last slot.
    assert!(download.split(0, 0..HALF, Instant::now()));
    assert_eq!(limits.try_recv(), Some(QUARTER));
    assert_eq!(
        state(&download, 2),
        Some((QUARTER..HALF, ChunkStatus::Pending))
    );
    assert_eq!(download.manager.allocated_runner_count(), 3);
    // Parked, so its lifecycle stays open.
    let mut parked = rt.take_parked();
    assert_eq!(parked.len(), 1);

    // The lifecycle poll reports the rejection; the loop ends the task.
    let tick = download.manager.tick().await;
    let failure = tick.failure.as_ref().expect("the rejection is reported");
    assert!(
        matches!(failure.error, TaskError::Other { .. }) && !failure.retryable,
        "{failure:?}"
    );
    assert!(matches!(
        download.on_runner(tick, Instant::now()),
        Err(DownloadError::Transfer { .. })
    ));
    // Only the rejected runner's slot is reclaimed, and only once.
    assert_eq!(download.manager.allocated_runner_count(), 2);
    assert_eq!(state(&download, 1), None);
    assert_eq!(
        state(&download, 0),
        Some((0..QUARTER, ChunkStatus::Running))
    );
    assert_eq!(
        state(&download, 2),
        Some((QUARTER..HALF, ChunkStatus::Pending))
    );
    assert_eq!(download.manager.get_available_ranges(), [HALF..TOTAL]);
    assert!(futures::poll!(pin!(download.manager.tick())).is_pending());

    // Probe by hand: the tail can get a runner in the freed slot.
    download.apply(StrategyAction::CreateTask(HALF..TOTAL), Instant::now());
    assert_eq!(
        state(&download, 1),
        Some((HALF..TOTAL, ChunkStatus::Pending))
    );
    assert_eq!(download.manager.allocated_runner_count(), 3);
    assert_eq!(download.manager.get_available_ranges(), []);
    parked.extend(rt.take_parked());
    assert_eq!(parked.len(), 2);

    drop(download);
    drop(parked);
    file_writer.finalize().await.unwrap();
}

mod growth;
mod hedge;
mod restart;
mod retry;
mod silent;

/// Random sequences of the loop's steps, including failures that back their
/// range off, congestion, growth and time passing.
mod split_properties {
    use pretty_assertions::assert_eq;
    use proptest::prelude::*;

    use super::{
        super::super::{
            chunk_planner::{ChunkStatus, Cooldown},
            runner_manager::AttemptFailure,
            strategy::{MIN_RANGE_BACKOFF, StrategyAction},
        },
        *,
    };
    use crate::adapter::UnretryableError;

    const TOTAL: u64 = 64 << 20;
    const MAX_CONNECTIONS: usize = 8;

    /// How a runner fails.
    #[derive(Clone, Copy, Debug)]
    enum Failure {
        Retryable,
        /// A restart once the runner received bytes (design §8.2).
        Timeout,
        /// A 503 with this `Retry-After`, in seconds.
        Congestion(Option<u64>),
    }

    impl Failure {
        fn error(self) -> TaskError {
            match self {
                Failure::Retryable => TaskError::ChannelClosed,
                Failure::Timeout => TaskError::Timeout,
                Failure::Congestion(retry_after) => TaskError::StreamError {
                    source: AdapterError::Unretryable {
                        source: UnretryableError::Congestion {
                            status: 503,
                            retry_after: retry_after.map(Duration::from_secs),
                        },
                    },
                },
            }
        }
    }

    /// One step of a download, as the loop would take it.
    #[derive(Clone, Debug)]
    enum Op {
        /// Create a runner for the `n`th unassigned range, or, past the
        /// last, for the whole file, which is taken unless it is all
        /// unassigned.
        Create(usize),
        /// The `n`th connecting runner reports `Started`.
        Start(usize),
        /// The `n`th running runner receives `bytes`.
        Progress(usize, u64),
        /// The `n`th running runner, or one whose chunk is complete,
        /// receives the rest of its chunk and stops.
        Finish(usize),
        /// The `n`th connecting or running runner fails, and the loop
        /// backs its range off.
        Fail(usize, Failure),
        /// Grow by up to `n` runners.
        Grow(usize),
        /// Split runner `id`, which may not exist, at the half of what it
        /// has left.
        SplitGiven(usize),
        ChangeMaxConcurrency(usize),
        /// A strategy tick at `speed` bytes per second.
        Tick(u64),
        /// Time passes, in milliseconds.
        Advance(u64),
        /// The loop wakes to retry the ranges whose backoff ended.
        Retry,
    }

    fn failure() -> impl Strategy<Value = Failure> {
        prop_oneof![
            Just(Failure::Retryable),
            Just(Failure::Timeout),
            prop::option::of(0..5u64).prop_map(Failure::Congestion),
        ]
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            any::<usize>().prop_map(Op::Create),
            any::<usize>().prop_map(Op::Start),
            (any::<usize>(), 0..TOTAL / 4).prop_map(|(n, bytes)| Op::Progress(n, bytes)),
            any::<usize>().prop_map(Op::Finish),
            (any::<usize>(), failure()).prop_map(|(n, failure)| Op::Fail(n, failure)),
            (0..2 * MAX_CONNECTIONS).prop_map(Op::Grow),
            (0..2 * MAX_CONNECTIONS).prop_map(Op::SplitGiven),
            (1..=2 * MAX_CONNECTIONS).prop_map(Op::ChangeMaxConcurrency),
            (0..TOTAL).prop_map(Op::Tick),
            (0..5_000u64).prop_map(Op::Advance),
            Just(Op::Retry),
        ]
    }

    /// The runners whose chunk has one of `statuses`, by ID.
    fn runners(
        download: &DownloadLoop,
        max_connections: usize,
        statuses: &[ChunkStatus],
    ) -> Vec<crate::task::RunnerId> {
        (0..max_connections)
            .filter(|&runner_id| {
                download
                    .manager
                    .get_runner_state(runner_id)
                    .is_some_and(|state| statuses.contains(&state.status))
            })
            .collect()
    }

    fn nth<T: Copy>(items: &[T], n: usize) -> Option<T> {
        (!items.is_empty()).then(|| items[n % items.len()])
    }

    fn overlaps(a: &std::ops::Range<u64>, b: &std::ops::Range<u64>) -> bool {
        a.start < b.end && b.start < a.end
    }

    fn step(download: &mut DownloadLoop, max_connections: usize, now: &mut Instant, op: Op) {
        use ChunkStatus::{Finished, Pending, Running};

        match op {
            Op::Create(n) => {
                let mut ranges = download.manager.get_available_ranges();
                ranges.push(0..TOTAL);
                download.apply(
                    StrategyAction::CreateTask(ranges[n % ranges.len()].clone()),
                    *now,
                );
            }
            Op::Start(n) => {
                if let Some(runner_id) = nth(&runners(download, max_connections, &[Pending]), n) {
                    download.manager.mark_started(runner_id);
                }
            }
            Op::Progress(n, bytes) => {
                if let Some(runner_id) = nth(&runners(download, max_connections, &[Running]), n) {
                    download.manager.update_progress(runner_id, bytes).unwrap();
                }
            }
            Op::Finish(n) => {
                let held = runners(download, max_connections, &[Running, Finished]);
                if let Some(runner_id) = nth(&held, n) {
                    download
                        .manager
                        .update_progress(runner_id, u64::MAX)
                        .unwrap();
                    download
                        .manager
                        .release_runner_full(runner_id, true)
                        .unwrap();
                }
            }
            Op::Fail(n, failure) => {
                let active = runners(download, max_connections, &[Pending, Running]);
                let Some(runner_id) = nth(&active, n) else {
                    return;
                };
                let state = download.manager.get_runner_state(runner_id).unwrap();
                let (attempts, charged) = (state.attempts, state.charged);
                let released = download.manager.mark_failed(runner_id).unwrap();
                download.manager.release_runner(runner_id);
                assert_eq!(released.attempts, attempts + 1, "the failure is counted");
                assert_eq!(released.charged, charged);
                let failure = AttemptFailure::new(
                    runner_id,
                    failure.error(),
                    !matches!(failure, Failure::Congestion(_)),
                    released.clone(),
                );
                let charge = match failure.error {
                    TaskError::ChannelClosed => 1,
                    TaskError::Timeout => u32::from(released.received == 0),
                    _ => 0,
                };
                assert_eq!(failure.charged, charged + charge, "{failure:?}");
                download.back_off(&failure, *now);
                if released.range.is_empty() {
                    return;
                }
                let cooldowns = download.manager.cooldowns();
                let (_, cooldown) = cooldowns
                    .iter()
                    .find(|(range, _)| {
                        range.start <= released.range.start && released.range.end <= range.end
                    })
                    .expect("the released range backs off");
                assert!(cooldown.attempts >= released.attempts);
                assert!(cooldown.charged >= failure.charged);
                assert!(cooldown.not_before >= *now + MIN_RANGE_BACKOFF);
            }
            Op::Grow(n) => download.apply(StrategyAction::Grow(n), *now),
            Op::SplitGiven(runner_id) => {
                if let Some(state) = download.manager.get_runner_state(runner_id) {
                    let incomplete = state.incomplete_range();
                    download.split(runner_id, incomplete, *now);
                }
            }
            Op::ChangeMaxConcurrency(n) => {
                download.apply(StrategyAction::ChangeMaxConcurrency(n), *now);
            }
            Op::Tick(speed) => {
                let active = download.manager.get_active_runners_count().max(1);
                download.throughput.per_runner_speed = speed as f64 / active as f64;
                download.on_strategy_tick(*now);
            }
            Op::Advance(ms) => *now += Duration::from_millis(ms),
            Op::Retry => download.retry_backed_off(*now),
        }
    }

    /// What a step must respect, taken before it.
    struct Before {
        now: Instant,
        held: Vec<crate::task::RunnerId>,
        congested: bool,
        cooldowns: Vec<(std::ops::Range<u64>, Cooldown)>,
        chunks: Vec<std::ops::Range<u64>>,
    }

    impl Before {
        fn of(download: &DownloadLoop, max_connections: usize, now: Instant) -> Self {
            use ChunkStatus::{Finished, Pending, Running};

            Self {
                now,
                held: runners(download, max_connections, &[Pending, Running, Finished]),
                congested: download.backoff.is_congested(now),
                cooldowns: download.manager.cooldowns(),
                chunks: download.manager.chunk_ranges(),
            }
        }
    }

    /// No more runners than allowed, every slot held by a runner with a
    /// chunk, and the chunks and the unassigned ranges cover the file
    /// exactly once.
    ///
    /// The cooldowns lie in the unassigned ranges, merged where they touch,
    /// and the loop wakes no later than the next one ends. Neither a
    /// cooldown nor a chunk charges more failures than it counts. A new
    /// runner opened no connection while congested, claimed no range
    /// backing off, and counts on from the failed and the charged attempts
    /// on its range; a split tail starts at none.
    fn check(download: &DownloadLoop, max_connections: usize, before: &Before) {
        use ChunkStatus::{Finished, Pending, Running};

        let manager = &download.manager;
        assert!(manager.allocated_runner_count() <= max_connections);
        assert!(manager.get_active_runners_count() <= max_connections);
        let held = runners(download, max_connections, &[Pending, Running, Finished]);
        // A hedge in its pair, or a beaten side not confirmed yet, holds a
        // slot without owning a chunk (design §9.6).
        let partners = (0..max_connections)
            .filter(|&runner_id| {
                manager.get_runner_state(runner_id).is_none()
                    && (manager.is_in_undecided_pair(runner_id) || manager.is_stopping(runner_id))
            })
            .count();
        assert_eq!(
            manager.allocated_runner_count(),
            held.len() + partners,
            "a slot leaked"
        );
        let mut ranges = manager.chunk_ranges();
        let available = manager.get_available_ranges();
        ranges.extend(available.iter().cloned());
        ranges.sort_by_key(|range| range.start);
        let mut end = 0;
        for range in ranges {
            assert!(
                range.start == end && range.start < range.end,
                "{range:?} after {end}"
            );
            end = range.end;
        }
        assert_eq!(end, TOTAL);

        let cooldowns = manager.cooldowns();
        for (range, cooldown) in &cooldowns {
            assert!(
                range.start < range.end && cooldown.attempts >= 1,
                "{range:?}"
            );
            assert!(cooldown.charged <= cooldown.attempts, "{cooldown:?}");
            assert!(
                available
                    .iter()
                    .any(|a| a.start <= range.start && range.end <= a.end),
                "{range:?} backs off outside the unassigned ranges {available:?}"
            );
        }
        for pair in cooldowns.windows(2) {
            assert!(pair[0].0.end < pair[1].0.start, "unmerged {pair:?}");
        }
        if let Some(retry) = download.next_retry(before.now) {
            assert!(
                download.deadline.at() <= retry,
                "the loop sleeps past {retry:?}"
            );
        }

        for &runner_id in &held {
            let state = manager.get_runner_state(runner_id).unwrap();
            assert!(
                state.charged <= state.attempts,
                "runner {runner_id}: {state:?}"
            );
        }
        for runner_id in held.into_iter().filter(|id| !before.held.contains(id)) {
            assert!(
                !before.congested,
                "runner {runner_id} connects while congested"
            );
            let state = manager.get_runner_state(runner_id).unwrap();
            // A runner on a range a chunk held before is a split tail.
            if before
                .chunks
                .iter()
                .any(|chunk| overlaps(chunk, &state.allocated))
            {
                assert_eq!(
                    (state.attempts, state.charged),
                    (0, 0),
                    "split tail of runner {runner_id}"
                );
            }
            let claimed: Vec<_> = before
                .cooldowns
                .iter()
                .filter(|(range, _)| overlaps(range, &state.allocated))
                .map(|(_, cooldown)| cooldown)
                .collect();
            assert!(
                claimed.iter().all(|c| c.not_before <= before.now),
                "runner {runner_id} claims {:?}, which backs off",
                state.allocated
            );
            assert_eq!(
                state.attempts,
                claimed.iter().map(|c| c.attempts).max().unwrap_or(0),
                "runner {runner_id} on {:?}",
                state.allocated
            );
            assert_eq!(
                state.charged,
                claimed.iter().map(|c| c.charged).max().unwrap_or(0),
                "runner {runner_id} on {:?}",
                state.allocated
            );
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn splits_stay_within_the_limit_and_cover_the_file(
            max_connections in 1..=MAX_CONNECTIONS,
            ops in prop::collection::vec(op(), 1..64),
        ) {
            let options = TaskOptions::default()
                .max_connections(NonZeroUsize::new(max_connections).unwrap());
            let (mut download, _file_writer) = futures::executor::block_on(loop_with_options(
                ThreadedRuntimeImpl::new_other_rt(ScriptedRuntime::new(TimerKind::Scripted)),
                DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(1).with_range_support(true)),
                TOTAL,
                &options,
            ));
            let mut now = Instant::now();
            let mut downloaded = 0;
            for op in ops {
                let before = Before::of(&download, max_connections, now);
                step(&mut download, max_connections, &mut now, op);
                check(&download, max_connections, &before);
                let total = download.manager.get_total_downloaded();
                prop_assert!(total >= downloaded, "downloaded bytes were dropped");
                downloaded = total;
            }
        }
    }
}
