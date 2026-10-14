use std::{
    path::Path,
    pin::pin,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use async_waitgroup::WaitGroup;
use bolt_load_tests::adapter::simple::SimpleTestAdapter;
use futures::{
    future::{Either, FutureObj},
    task::{Spawn, SpawnError},
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
    adapter::DynBoltLoadAdapter,
    runtime::{
        DowncastLocalRuntime, DynTimer, ThreadedRuntime, ThreadedRuntimeImpl, Timer, TimerBuilder,
        TimerImpl,
    },
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
    let (event_tx, _event_rx) = async_channel::unbounded();
    let mut download = DownloadLoop::new(
        1000,
        spawner,
        &file_writer,
        event_tx,
        &TaskOptions::default(),
    );
    let reg = download.manager.allocate_runner_with_chunk(0..100).unwrap();

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
    assert!(reg.control_rx.try_recv().is_err());
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
    /// Run the future to completion on a thread of its own.
    Run,
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
}

impl Spawn for ScriptedRuntime {
    fn spawn_obj(&self, future: FutureObj<'static, ()>) -> Result<(), SpawnError> {
        let mode = *self.mode.lock().unwrap();
        match mode {
            SpawnMode::Park => self.parked.lock().unwrap().push(future),
            SpawnMode::Fail => return Err(SpawnError::shutdown()),
            SpawnMode::Run => drop(std::thread::spawn(move || {
                futures::executor::block_on(future)
            })),
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
    let fut = pin!(fut);
    match futures::future::select(fut, async_io::Timer::after(Duration::from_secs(10))).await {
        Either::Left((value, _)) => value,
        Either::Right(_) => panic!("the event loop never returned"),
    }
}

/// Cancel `token` from another thread after `delay`.
fn cancel_after(token: &CancellationToken, delay: Duration) {
    let token = token.clone();
    drop(std::thread::spawn(move || {
        std::thread::sleep(delay);
        token.cancel();
    }));
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
    let Err(TaskInstanceError::Failed { source }) = result else {
        panic!("unexpected loop result: {result:?}");
    };
    match &**source {
        TaskError::Cancelled => "cancel",
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
    events: async_channel::Receiver<TaskEvent>,
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
        let (event_tx, events) = async_channel::unbounded();
        let download = DownloadLoop::new(LOOP_TOTAL, spawner, &file_writer, event_tx, &options);
        Self {
            download,
            file_writer,
            rt,
            cancel: CancellationToken::new(),
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

    /// Run the loop; it is cancelled after `cancel_after` if still running.
    async fn run(&mut self, cancel_after_delay: Duration) -> Result<()> {
        cancel_after(&self.cancel, cancel_after_delay);
        with_watchdog(self.download.run(&self.cancel)).await
    }

    fn progress_events(&self) -> usize {
        std::iter::from_fn(|| self.events.try_recv().ok())
            .filter(|e| matches!(e, TaskEvent::Downloading(_)))
            .count()
    }

    async fn teardown(self) {
        drop(self.download);
        self.file_writer.finalize().await.unwrap();
    }
}

/// Pairs `(higher, lower)` that `run` already orders correctly.
const PRIORITY_PAIRS: &[(Source, Source)] = &[
    (Source::Cancel, Source::Deadline),
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
    let result = harness.run(Duration::from_millis(200)).await;
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
#[ignore = "fixed in Phase 3b: run() drains writer completions before polling cancel"]
fn test_loop_priority_cancel_beats_writer() {
    on_each_executor(|| assert_priority(Source::Cancel, Source::Writer));
}

#[test]
#[ignore = "fixed in Phase 3b: run() drains writer completions before polling the deadline"]
fn test_loop_priority_deadline_beats_writer() {
    on_each_executor(|| assert_priority(Source::Deadline, Source::Writer));
}

#[test]
fn test_custom_timer_survives_other_events() {
    on_each_executor(|| async {
        let mut harness = LoopHarness::scripted(false).await;
        // Writer completions arrive while the loop waits.
        harness.rt.set_mode(SpawnMode::Run);
        while !harness.download.writer.is_full() {
            harness
                .download
                .writer
                .write_range(0..1, bytes::Bytes::from_static(b"x"))
                .unwrap();
        }
        harness.rt.set_mode(SpawnMode::Park);
        for _ in 0..3 {
            harness.rt.script.permit("strategy");
        }
        let script = harness.rt.script.clone();
        drop(std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            script.permit("throughput");
        }));

        let result = harness.run(Duration::from_millis(300)).await;

        assert_eq!(outcome(&result), "cancel");
        assert_eq!(
            harness.rt.script.log(),
            ["strategy", "strategy", "strategy", "throughput"]
        );
        assert!(harness.download.writer.is_idle());
        assert_eq!(harness.progress_events(), 1);
        harness.teardown().await;
    });
}

#[test]
fn test_timer_rearms_after_consecutive_ticks() {
    // Scripted: every permit is one more tick.
    on_each_executor(|| async {
        let mut harness = LoopHarness::scripted(false).await;
        let script = harness.rt.script.clone();
        drop(std::thread::spawn(move || {
            for _ in 0..3 {
                std::thread::sleep(Duration::from_millis(20));
                script.permit("throughput");
            }
        }));
        let result = harness.run(Duration::from_millis(300)).await;
        assert_eq!(outcome(&result), "cancel");
        assert_eq!(harness.progress_events(), 3);
        harness.teardown().await;
    });

    // Native: the first tick is immediate, then one per sample interval.
    let native = |timers| async move {
        let mut harness = LoopHarness::new(timers, TaskOptions::default()).await;
        let result = harness
            .run(DEFAULT_SAMPLE_INTERVAL * 2 + DEFAULT_SAMPLE_INTERVAL / 2)
            .await;
        assert_eq!(outcome(&result), "cancel");
        let ticks = harness.progress_events();
        assert!(ticks >= 3, "{timers:?}: {ticks} throughput ticks");
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
        harness.download.policy.on_bytes_accepted(1, Instant::now());
        std::thread::sleep(TIMEOUT / 4);
        let last_progress = Instant::now();
        harness.download.policy.on_bytes_accepted(2, last_progress);

        let result = harness.run(Duration::from_secs(5)).await;

        let elapsed = last_progress.elapsed();
        assert_eq!(outcome(&result), "deadline");
        assert!(elapsed >= TIMEOUT, "fired early: {elapsed:?}");
        assert!(elapsed < TIMEOUT * 2, "fired late: {elapsed:?}");
        harness.teardown().await;
    });
}

#[test]
fn test_cancel_after_registration_wakes_loop() {
    on_each_executor(|| async {
        let mut harness = LoopHarness::scripted(false).await;
        let started = Instant::now();
        let result = harness.run(Duration::from_millis(50)).await;
        assert_eq!(outcome(&result), "cancel");
        assert!(started.elapsed() >= Duration::from_millis(50));
        harness.teardown().await;
    });
}
