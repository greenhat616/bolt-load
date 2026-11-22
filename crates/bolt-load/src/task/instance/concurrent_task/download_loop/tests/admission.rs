//! The connection limit in the loop (design §8.3).
//!
//! The test drives the healthy runners through their channels. Every runner
//! the loop spawns connects through an adapter that refuses it, or never
//! answers it, and ends through the lifecycle as a real connect would. Every
//! step gets its time passed in; the tests against the HTTP fault server
//! run on real time.

use std::{ops::Range, pin::Pin, task::Context};

use async_ringbuf::{
    AsyncHeapRb,
    traits::{Producer, Split},
};
use futures::FutureExt;
use pretty_assertions::assert_eq;

use super::{
    super::super::{chunk_planner::ChunkStatus, runner_manager::RunnerChannels, strategy::Backoff},
    *,
};
use crate::{
    adapter::{BoltLoadAdapter, RetryableError, UnretryableError},
    runner::{
        DATA_FRAME_CHANNEL_CAPACITY, DEFAULT_CONNECT_TIMEOUT, DataFrame, DataFrameSender,
        LIFECYCLE_CHANNEL_CAPACITY, LifecycleEvent, LifecycleSender, LimitReceiver,
    },
};

const TOTAL: u64 = 1 << 30;
const TICK: Duration = DEFAULT_STRATEGY_TICK_INTERVAL;
const SECOND: Duration = Duration::from_secs(1);
/// What every running runner delivers at each tick.
const DELIVERED: u64 = 64 << 10;
const NANO: Duration = Duration::from_nanos(1);

/// How the server answers the connects of the runners the loop spawns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Server {
    /// Refuses them at once.
    Refuses,
    /// Never answers them: they time out after the connect timeout.
    Hangs,
    /// Admits them.
    Admits,
    /// Refuses one, admits the next, and so on.
    Alternates,
}

/// Connects nothing: refuses at once, or never answers.
struct ServerAdapter {
    hangs: bool,
}

impl BoltLoadAdapter for ServerAdapter {
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
        if self.hangs {
            futures::future::pending().await
        } else {
            Err(RetryableError::Io {
                source: Arc::new(std::io::Error::from(std::io::ErrorKind::ConnectionRefused)),
            }
            .into())
        }
    }
}

/// The ends of a runner the test drives.
struct TestRunner {
    lifecycle: LifecycleSender,
    _data: DataFrameSender,
    _limits: LimitReceiver,
}

/// A runner on `range` whose lifecycle the test drives.
fn spawn_test_runner(download: &mut DownloadLoop, range: Range<u64>) -> TestRunner {
    let (lifecycle, lifecycle_rx) =
        AsyncHeapRb::<LifecycleEvent>::new(LIFECYCLE_CHANNEL_CAPACITY).split();
    let (data, data_rx) = AsyncHeapRb::<DataFrame>::new(DATA_FRAME_CHANNEL_CAPACITY).split();
    let mut limits = None;
    download
        .manager
        .spawn_runner_with_chunk(range, |_, runner_limits| {
            limits = Some(runner_limits);
            Ok(RunnerChannels {
                lifecycle_rx,
                data_rx,
                stop: Default::default(),
            })
        })
        .unwrap();
    TestRunner {
        lifecycle,
        _data: data,
        _limits: limits.unwrap(),
    }
}

fn congestion() -> TaskError {
    TaskError::StreamError {
        source: AdapterError::Unretryable {
            source: UnretryableError::Congestion {
                status: 503,
                retry_after: None,
            },
        },
    }
}

fn reset() -> TaskError {
    TaskError::StreamError {
        source: AdapterError::Retryable {
            source: RetryableError::Io {
                source: Arc::new(std::io::Error::from(std::io::ErrorKind::ConnectionReset)),
            },
        },
    }
}

/// A loop with `max_connections` whose spawned runners are parked, and
/// connect through a server that refuses them or, when `hangs`, never
/// answers them. Backoffs have no jitter.
async fn admission_loop(
    rt: &ScriptedRuntime,
    hangs: bool,
    max_connections: usize,
    budget: u32,
) -> (DownloadLoop, FileWriter) {
    let file_writer = FileWriter::new_with_kind(Path::new(""), TOTAL, FileRangeWriterKind::Null)
        .await
        .unwrap();
    let spawner = RunnerSpawner::new(
        ThreadedRuntimeImpl::new_other_rt(rt.clone()),
        DynBoltLoadAdapter::new_arc(ServerAdapter { hangs }),
        WaitGroup::new(),
        CancellationToken::new(),
        crate::runner::DEFAULT_IDLE_TIMEOUT,
    );
    // A runner that is never answered times out at its first poll, which
    // the test makes once the connect timeout passed in its own time.
    let spawner = if hangs {
        spawner.connect_timeout(Duration::ZERO)
    } else {
        spawner
    };
    let (reporter, _status) = ProgressReporter::for_test();
    let options = TaskOptions::default()
        .max_connections(NonZeroUsize::new(max_connections).unwrap())
        .max_attempts_per_range(std::num::NonZeroU32::new(budget).unwrap());
    let mut download = DownloadLoop::new(
        TOTAL,
        spawner,
        &file_writer,
        reporter,
        &options,
        CancellationToken::new(),
    );
    download.backoff = Backoff::without_jitter();
    // Every write parks in the test's runtime, so the test decides when it
    // completes; the loop's own writer hands writes to background workers.
    download.writer = PendingWriter::new(
        Arc::new(file_writer.clone()),
        ThreadedRuntimeImpl::new_other_rt(rt.clone()),
        max_connections,
    );
    (download, file_writer)
}

/// Complete the parked `writes`, hand their completions to the loop at
/// `now`, and
/// complete the writes that dispatches in turn, until the writer is idle.
fn drain(
    download: &mut DownloadLoop,
    rt: &ScriptedRuntime,
    writes: &mut Vec<FutureObj<'static, ()>>,
    now: Instant,
) {
    while !download.writer.is_idle() {
        for write in writes.drain(..) {
            futures::executor::block_on(write);
        }
        while let Some(LoopEvent::WriterCompleted(Some(completions), was_full)) =
            std::future::poll_fn(|cx| download.poll_writer(cx)).now_or_never()
        {
            download
                .record_completions(completions, was_full, now)
                .unwrap();
        }
        // The write the writer held back, dispatched once a slot freed.
        writes.extend(rt.take_parked());
    }
}

/// A loop driven event by event in the test's own time.
struct Sim {
    download: DownloadLoop,
    file_writer: FileWriter,
    rt: ScriptedRuntime,
    server: Server,
    /// Whether [`Server::Alternates`] refuses the next connect.
    refuse_next: bool,
    healthy: Vec<TestRunner>,
    /// The connects of the runners the loop spawned, not answered yet, and
    /// when they began.
    connecting: Vec<(Instant, FutureObj<'static, ()>)>,
    /// The runners the server admitted, kept so that they stay connected.
    admitted: Vec<FutureObj<'static, ()>>,
    /// Writes parked by [`Self::fill_writer`].
    writes: Vec<FutureObj<'static, ()>>,
    now: Instant,
    next_tick: Instant,
    /// When the loop spawned each runner.
    spawned: Vec<Instant>,
    /// When the loop handled each failed attempt, and its error.
    failures: Vec<(Instant, TaskError)>,
    /// The charged failures of each failure's range, this one included,
    /// and whether it was exempt, in the order of `failures`.
    charges: Vec<(u32, bool)>,
    /// The error that ended the task, which stops the run.
    ended: Option<DownloadError>,
    /// Whether the running runners deliver at each tick.
    delivering: bool,
    /// The loop's limit, and each change with its time.
    limits: Vec<(Instant, usize)>,
}

impl Sim {
    /// A loop with `max_connections`, and a connected runner on each of
    /// `healthy` that has delivered bytes; the first tick at `t0` creates
    /// runners for the rest.
    async fn new(server: Server, max_connections: usize, healthy: &[Range<u64>]) -> Self {
        // Most tests are about the connection limit: the range budget is
        // left out of the way.
        Self::build(server, max_connections, healthy, u32::MAX, true).await
    }

    /// [`Self::new`] with `budget` charged failures allowed per range;
    /// unless `proven`, the healthy runners deliver only from the first
    /// tick after `t0`.
    async fn build(
        server: Server,
        max_connections: usize,
        healthy: &[Range<u64>],
        budget: u32,
        proven: bool,
    ) -> Self {
        let rt = ScriptedRuntime::new(TimerKind::Scripted);
        let (mut download, file_writer) =
            admission_loop(&rt, server == Server::Hangs, max_connections, budget).await;
        let t0 = Instant::now();
        let mut runners = Vec::new();
        for range in healthy {
            let mut runner = spawn_test_runner(&mut download, range.clone());
            runner.lifecycle.try_push(LifecycleEvent::Started).unwrap();
            let tick = download.manager.tick().await;
            assert!(download.on_runner(tick, t0).unwrap().is_continue());
            runners.push(runner);
        }
        let mut sim = Self {
            download,
            file_writer,
            rt,
            server,
            refuse_next: true,
            healthy: runners,
            connecting: Vec::new(),
            admitted: Vec::new(),
            writes: Vec::new(),
            now: t0,
            next_tick: t0 + TICK,
            spawned: Vec::new(),
            failures: Vec::new(),
            charges: Vec::new(),
            ended: None,
            delivering: true,
            limits: vec![(t0, max_connections)],
        };
        if proven {
            sim.deliver();
        }
        sim.download.on_strategy_tick(t0);
        sim.settle();
        sim
    }

    fn t0(&self) -> Instant {
        self.limits[0].0
    }

    fn limit(&self) -> usize {
        self.download.max_concurrency
    }

    /// The runners whose chunk has `status`.
    fn runners(&self, status: ChunkStatus) -> Vec<crate::task::RunnerId> {
        (0..64)
            .filter(|&id| {
                self.download
                    .manager
                    .get_runner_state(id)
                    .is_some_and(|state| state.status == status)
            })
            .collect()
    }

    /// Every running runner delivers, and its bytes are written; nothing
    /// while the writer is full, as the loop then polls no data.
    fn deliver(&mut self) {
        if !self.delivering || !self.download.writer.can_write() {
            return;
        }
        for runner_id in self.runners(ChunkStatus::Running) {
            self.download
                .manager
                .update_progress(runner_id, DELIVERED)
                .unwrap();
        }
        self.download.policy.on_progress(self.now);
        self.download.policy.on_write_ok(1);
    }

    /// Answer the connects that are due, and handle what the runners
    /// report.
    fn settle(&mut self) {
        let now = self.now;
        for future in self.rt.take_parked() {
            self.spawned.push(now);
            self.connecting.push((now, future));
        }
        let (due, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.connecting)
            .into_iter()
            .partition(|(began, _)| {
                self.server != Server::Hangs || *began + DEFAULT_CONNECT_TIMEOUT <= now
            });
        self.connecting = waiting;
        let mut admitted = false;
        for (_, mut future) in due {
            let refuse = match self.server {
                Server::Refuses | Server::Hangs => true,
                Server::Admits => false,
                Server::Alternates => {
                    self.refuse_next = !self.refuse_next;
                    !self.refuse_next
                }
            };
            if refuse {
                let waker = futures::task::noop_waker();
                let polled = Pin::new(&mut future).poll(&mut Context::from_waker(&waker));
                assert!(polled.is_ready(), "the connect ends at once");
            } else {
                self.admitted.push(future);
                admitted = true;
            }
        }
        if admitted {
            for runner_id in self.runners(ChunkStatus::Pending) {
                self.download.manager.mark_started(runner_id);
            }
        }
        // As the loop polls its runners: data only while the writer can
        // take it.
        while let Some(LoopEvent::Runner(tick)) = {
            let download = &mut self.download;
            std::future::poll_fn(|cx| download.poll_runner(cx)).now_or_never()
        } {
            if let Some(failure) = &tick.failure {
                self.failures.push((now, failure.error.clone()));
                self.charges.push((failure.charged, failure.exempt));
            }
            match self.download.on_runner(tick, now) {
                Ok(flow) => assert!(flow.is_continue()),
                Err(error) => {
                    self.ended = Some(error);
                    return;
                }
            }
        }
    }

    /// Healthy runner `n` receives the rest of its chunk and finishes.
    fn complete_healthy(&mut self, n: usize) {
        self.download.manager.update_progress(n, u64::MAX).unwrap();
        self.healthy[n]
            .lifecycle
            .try_push(LifecycleEvent::finished())
            .unwrap();
        self.settle();
    }

    /// Fill the writer with writes, which stay parked: it takes no more
    /// data.
    fn fill_writer(&mut self) {
        self.settle();
        while self.download.writer.can_write() {
            self.download
                .writer
                .write_range(0..1, bytes::Bytes::from_static(b"x"))
                .unwrap();
        }
        self.writes.extend(self.rt.take_parked());
        assert!(!self.download.writer.can_write());
    }

    /// Complete the parked writes, and hand their completions to the loop.
    fn drain_writer(&mut self) {
        drain(&mut self.download, &self.rt, &mut self.writes, self.now);
    }

    /// Drop the admitted runners: their streams close.
    fn lose_admitted(&mut self) {
        self.admitted.clear();
        self.settle();
    }

    /// Fail healthy runner `n` with `error`, after `Started`.
    fn fail_healthy(&mut self, n: usize, error: TaskError) {
        self.healthy[n]
            .lifecycle
            .try_push(LifecycleEvent::failed(error))
            .unwrap();
        self.settle();
    }

    /// Run the loop's events in order until `end`: the strategy ticks, the
    /// wake-ups for backed-off ranges and the ends of the connect timeouts.
    /// A wake-up at the time of a tick comes first.
    fn run_until(&mut self, end: Instant) {
        while self.ended.is_none() {
            let timeout = (self.server == Server::Hangs)
                .then(|| {
                    self.connecting
                        .iter()
                        .map(|(began, _)| *began + DEFAULT_CONNECT_TIMEOUT)
                        .min()
                })
                .flatten();
            let wake = [self.download.next_retry(self.now), timeout]
                .into_iter()
                .flatten()
                .min();
            match wake {
                Some(at) if at <= self.next_tick => {
                    if at > end {
                        return;
                    }
                    self.now = at;
                    self.download.on_deadline(at).unwrap();
                }
                _ => {
                    if self.next_tick > end {
                        return;
                    }
                    self.now = self.next_tick;
                    self.next_tick += TICK;
                    self.deliver();
                    self.download.on_strategy_tick(self.now);
                    if self.limit() != self.limits.last().unwrap().1 {
                        self.limits.push((self.now, self.limit()));
                    }
                }
            }
            self.settle();
        }
    }

    /// The failures handled in `range` of time.
    fn failures_in(&self, range: Range<Instant>) -> Vec<&TaskError> {
        self.failures
            .iter()
            .filter(|(at, _)| range.contains(at))
            .map(|(_, error)| error)
            .collect()
    }

    async fn teardown(self) {
        assert!(self.ended.is_none(), "the task ended: {:?}", self.ended);
        drop(self.download);
        self.file_writer.finalize().await.unwrap();
    }
}

/// `n` ranges that split the first `n` eighths of the file.
fn eighths(n: u64) -> Vec<Range<u64>> {
    (0..n).map(|i| i * TOTAL / 8..(i + 1) * TOTAL / 8).collect()
}

/// `n` ranges that split the file.
fn whole(n: u64) -> Vec<Range<u64>> {
    (0..n).map(|i| i * TOTAL / n..(i + 1) * TOTAL / n).collect()
}

/// The first tick at or after `at`.
fn first_tick_from(t0: Instant, at: Instant) -> Instant {
    let ticks = (at - t0).as_nanos().div_ceil(TICK.as_nanos());
    t0 + TICK * u32::try_from(ticks).unwrap()
}

/// A server that admits four of eight connections and refuses the others,
/// at once or by never answering: three refusals in a row, each beside the
/// four that deliver, lower the limit to the four at the first tick at
/// least one tick interval after the third. Nothing above four connects
/// after that, and the task goes on.
#[tokio::test]
async fn a_silent_limit_lowers_the_limit_to_the_connected_runners() {
    for server in [Server::Refuses, Server::Hangs] {
        let mut sim = Sim::new(server, 8, &eighths(4)).await;
        let t0 = sim.t0();
        let horizon = match server {
            Server::Hangs => 3 * DEFAULT_CONNECT_TIMEOUT + 10 * SECOND,
            _ => 10 * SECOND,
        };
        sim.run_until(t0 + horizon);
        let [(_, 8), (lowered_at, 4)] = sim.limits[..] else {
            panic!("{server:?}: limits {:?}", sim.limits);
        };
        let third = sim.failures[2].0;
        assert!(
            sim.failures_in(t0..lowered_at)
                .iter()
                .all(|error| matches!(error, TaskError::StreamError { .. } | TaskError::Timeout)),
            "{server:?}: {:?}",
            sim.failures
        );
        assert_eq!(lowered_at, first_tick_from(t0, third + TICK), "{server:?}");
        if server == Server::Hangs {
            assert!(matches!(sim.failures[0].1, TaskError::Timeout));
            assert_eq!(sim.failures[0].0, t0 + DEFAULT_CONNECT_TIMEOUT);
        }

        // Nothing connects above the four while they run.
        let connects = sim.spawned.len();
        sim.run_until(lowered_at + 25 * SECOND);
        assert_eq!(sim.spawned.len(), connects, "{server:?}");
        assert_eq!(sim.limit(), 4, "{server:?}");
        assert_eq!(sim.download.manager.get_active_runners_count(), 4);
        sim.teardown().await;
    }
}

/// A 503 in the body stream of a running runner, beside four healthy
/// ones, lowers the limit to the four at the next step.
#[tokio::test]
async fn a_streaming_congestion_lowers_the_limit() {
    let mut sim = Sim::new(Server::Refuses, 8, &whole(5)).await;
    let t0 = sim.t0();
    sim.run_until(t0 + 3 * TICK);
    assert_eq!(sim.limits.len(), 1);
    sim.fail_healthy(4, congestion());
    assert!(sim.download.backoff.congestion_until().is_some());
    sim.run_until(t0 + 4 * TICK);
    assert_eq!(sim.limits[1..], [(t0 + 4 * TICK, 4)]);
    // Nothing raises it while the four run.
    sim.run_until(t0 + 4 * TICK + 25 * SECOND);
    assert_eq!(sim.limits.len(), 2);
    assert_eq!(sim.limit(), 4);
    sim.teardown().await;
}

/// A runner lost while connecting is no refusal, however often.
#[tokio::test]
async fn a_lost_runner_is_no_refusal() {
    let mut sim = Sim::new(Server::Admits, 8, &eighths(4)).await;
    let t0 = sim.t0();
    for n in 1..=20 {
        // The loop's runner is lost before it connects.
        for (_, future) in std::mem::take(&mut sim.connecting) {
            drop(future);
        }
        sim.admitted.clear();
        sim.settle();
        sim.run_until(t0 + TICK * n);
    }
    assert!(sim.failures.len() >= 5, "{:?}", sim.failures);
    assert_eq!(sim.limits.len(), 1, "{:?}", sim.limits);
    assert_eq!(sim.download.manager.admission().streak_count(), 0);
    sim.teardown().await;
}

/// All four runners fail at once and their reconnects are refused while
/// none delivers: the refusals are not corroborated, and once the server
/// admits again the limit is where it was.
#[tokio::test]
async fn a_blip_keeps_the_limit() {
    let mut sim = Sim::new(Server::Refuses, 8, &whole(4)).await;
    let t0 = sim.t0();
    sim.run_until(t0 + 2 * TICK);
    for n in 0..4 {
        sim.fail_healthy(n, reset());
    }
    assert_eq!(sim.download.manager.get_active_runners_count(), 0);
    sim.run_until(t0 + 10 * SECOND);
    assert!(sim.failures.len() >= 8, "{:?}", sim.failures);
    sim.server = Server::Admits;
    sim.run_until(t0 + 40 * SECOND);
    assert_eq!(sim.limits.len(), 1, "{:?}", sim.limits);
    assert_eq!(sim.download.manager.get_active_runners_count(), 4);
    sim.teardown().await;
}

/// A refusal, then a connect at its level that later stops, again and
/// again: no three refusals are ever in a row.
#[tokio::test]
async fn interleaved_stops_and_refusals_keep_the_limit() {
    let mut sim = Sim::new(Server::Alternates, 8, &eighths(4)).await;
    let t0 = sim.t0();
    let mut streak = 0;
    for n in 1..=80 {
        sim.run_until(t0 + TICK * n);
        streak = streak.max(sim.download.manager.admission().streak_count());
        if n % 3 == 0 {
            sim.lose_admitted();
        }
    }
    let refusals = sim
        .failures
        .iter()
        .filter(|(_, error)| matches!(error, TaskError::StreamError { .. }))
        .count();
    assert!(refusals >= 4, "{:?}", sim.failures);
    assert_eq!(streak, 1);
    assert_eq!(sim.limits.len(), 1, "{:?}", sim.limits);
    sim.teardown().await;
}

/// The ticks of `sim` at which a step began, and at which it ended, with
/// the limit controller's state right after each tick.
#[derive(Debug, Default)]
struct Steps {
    began: Vec<Instant>,
    /// When each ended, and when the next was due then.
    ended: Vec<(Instant, Instant)>,
}

impl Sim {
    /// Run tick by tick until `end`, and record the steps.
    fn run_recording_steps(&mut self, end: Instant, steps: &mut Steps) {
        while self.next_tick <= end {
            let tick = self.next_tick;
            let before = self.download.strategy.limit_state().1;
            self.run_until(tick);
            let (_, step, due, _) = self.download.strategy.limit_state();
            match (before, step) {
                (None, Some(began)) => steps.began.push(began),
                (Some(_), None) => steps.ended.push((tick, due.unwrap())),
                _ => {}
            }
        }
    }
}

/// The silent limit of design §8.3 over 300 s of the loop's time: after
/// the lowering to the four connected runners, the only connects above
/// four are one per step, each refused, and each step rejected.
///
/// Each step begins at the first tick at or after its due time: the
/// lowering's time plus the base wait, then the rejection's tick plus the
/// wait, which doubles. Asserted against the recorded rejections and due
/// times, not against fixed seconds.
#[tokio::test]
async fn a_silent_limit_settles_at_the_connected_runners() {
    for server in [Server::Refuses, Server::Hangs] {
        let mut sim = Sim::new(server, 8, &eighths(4)).await;
        let t0 = sim.t0();
        let mut steps = Steps::default();
        let horizon = match server {
            Server::Hangs => 3 * DEFAULT_CONNECT_TIMEOUT + 10 * SECOND,
            _ => 10 * SECOND,
        };
        sim.run_until(t0 + horizon);
        let (lowered_at, 4) = sim.limits[1] else {
            panic!("{server:?}: {:?}", sim.limits);
        };
        let failures = sim.failures.len();
        let spawned = sim.spawned.len();
        sim.run_recording_steps(lowered_at + 300 * SECOND, &mut steps);

        let base = Duration::from_secs(30);
        assert!(steps.began.len() >= 3, "{server:?}: {steps:?}");
        let mut due = lowered_at + base;
        for (n, began) in steps.began.iter().enumerate() {
            assert_eq!(*began, first_tick_from(t0, due), "{server:?}: step {n}");
            let Some(&(ended, next_due)) = steps.ended.get(n) else {
                break;
            };
            assert_eq!(
                next_due,
                ended + base * 2u32.pow(n as u32 + 1),
                "{server:?}"
            );
            due = next_due;
        }
        // One connect a step, refused, and it rejected the step.
        let trials = &sim.spawned[spawned..];
        assert_eq!(trials, &steps.began[..], "{server:?}");
        let refusals: Vec<_> = sim.failures[failures..].iter().map(|f| f.0).collect();
        assert_eq!(refusals.len(), steps.ended.len(), "{server:?}");
        // Each rejected at the tick that follows the refusal.
        for (refused, (ended, _)) in refusals.iter().zip(&steps.ended) {
            assert!(
                *refused <= *ended && *ended <= *refused + TICK,
                "{server:?}: refused {refused:?}, ended {ended:?}"
            );
        }
        if server == Server::Hangs {
            for (refused, began) in refusals.iter().zip(&steps.began) {
                assert_eq!(*refused, *began + DEFAULT_CONNECT_TIMEOUT);
            }
        }
        // The cap alternates between the limit and one more, never higher.
        assert!(
            sim.limits[2..]
                .iter()
                .enumerate()
                .all(|(n, (_, limit))| *limit == if n % 2 == 0 { 5 } else { 4 }),
            "{server:?}: {:?}",
            sim.limits
        );
        assert_eq!(sim.download.strategy.limit_state().0, 4);
        sim.teardown().await;
    }
}

/// A step's trial on a range that failed once is refused just after the
/// step's tick; its backoff of 1 s ends before the next tick. The wake-up
/// at the end of the backoff opens no second trial: the step is already
/// rejected, and the next tick ends it.
#[tokio::test]
async fn a_rejected_step_opens_no_second_trial_before_its_tick() {
    let mut sim = Sim::new(Server::Refuses, 8, &whole(4)).await;
    let t0 = sim.t0();
    sim.run_until(t0 + TICK);
    // A 503 on one of the four lowers the limit to the other three.
    sim.fail_healthy(3, congestion());
    let mut steps = Steps::default();
    sim.run_recording_steps(sim.next_tick, &mut steps);
    let (_, 3) = sim.limits[1] else {
        panic!("{:?}", sim.limits);
    };
    let spawned = sim.spawned.len();
    while steps.began.is_empty() {
        assert!(sim.now < t0 + 40 * SECOND, "no step began");
        sim.run_recording_steps(sim.next_tick, &mut steps);
    }
    let began = steps.began[0];
    // The trial, on the range of the 503, refused at the step's tick.
    assert_eq!(sim.spawned[spawned..], [began]);
    assert_eq!(sim.failures.last().unwrap().0, began);
    // Its range backs off for 1 s, less than a tick.
    let retry = sim.download.next_retry(began).unwrap();
    assert!(retry < began + TICK, "{:?}", retry - began);
    sim.run_recording_steps(began + TICK, &mut steps);
    assert_eq!(sim.spawned[spawned..], [began], "a second trial opened");
    assert_eq!(steps.ended.len(), 1, "{steps:?}");
    assert_eq!(steps.ended[0].0, began + TICK);
    sim.teardown().await;
}

// ── The range budget and the proven window (design §14.13) ─────────

/// Four healthy runners on four eighths, delivering, and the rest of the
/// file refused for every connect the loop makes, with `budget` charged
/// failures allowed per range.
async fn refused_rest(budget: u32) -> Sim {
    Sim::build(Server::Refuses, 8, &eighths(4), budget, true).await
}

/// Each refusal of the silent limit comes beside four proven runners, with
/// four others in flight: above the proven window, so never charged. The
/// task settles at four for 300 s, whatever the budget, steps included.
#[tokio::test]
async fn refusals_above_the_proven_window_are_not_charged() {
    for budget in [1, 2, 3, 5] {
        let mut sim = refused_rest(budget).await;
        let t0 = sim.t0();
        sim.run_until(t0 + 300 * SECOND);
        assert!(sim.ended.is_none(), "budget {budget}: {:?}", sim.ended);
        assert_eq!(sim.limits[1].1, 4, "budget {budget}");
        assert!(sim.charges.len() >= 5, "budget {budget}");
        assert!(
            sim.charges
                .iter()
                .all(|&(charged, exempt)| charged == 0 && exempt),
            "budget {budget}: {:?}",
            sim.charges
        );
        sim.teardown().await;
    }
}

/// A fifth runner, proven beside the four, resets after its bytes, and the
/// server admits only the four from then on. Its range is refused with
/// four others in flight, fewer than the five proven: inside the window,
/// so each refusal is charged, and the B-th ends the task. The four stop
/// delivering, so no lowering intervenes.
#[tokio::test]
async fn a_refused_range_inside_the_proven_window_is_exhausted() {
    for budget in [1, 2, 3, 5] {
        let mut sim = Sim::build(Server::Refuses, 8, &whole(5), budget, true).await;
        let t0 = sim.t0();
        sim.run_until(t0 + TICK);
        assert_eq!(sim.download.manager.admission().proven_window(), 5);
        sim.delivering = false;
        sim.fail_healthy(4, reset());
        sim.run_until(t0 + 30 * SECOND);
        let Some(DownloadError::Transfer { source }) = sim.ended.take() else {
            panic!("budget {budget}: the task goes on: {:?}", sim.charges);
        };
        let TaskError::RangeExhausted { attempts, .. } = &*source else {
            panic!("budget {budget}: {source:?}");
        };
        assert_eq!(*attempts, budget);
        // The restart, then one charge a refusal.
        let charged: Vec<_> = sim.charges.iter().map(|&(charged, _)| charged).collect();
        assert_eq!(charged, (0..=budget).collect::<Vec<_>>(), "budget {budget}");
        assert_eq!(sim.limits.len(), 1);
        sim.teardown().await;
    }
}

/// A range whose connects were never proven is exempt while it is refused
/// above the window. Once the limit is lowered to the four, the window is
/// four; when one of them finishes, its next retry has three others in
/// flight, inside the window, and is charged once.
#[tokio::test]
async fn a_range_never_proven_is_charged_once_inside_the_window() {
    let mut sim = refused_rest(5).await;
    let t0 = sim.t0();
    sim.run_until(t0 + 10 * SECOND);
    assert_eq!(sim.limits[1].1, 4);
    assert!(sim.charges.iter().all(|&(charged, _)| charged == 0));
    assert_eq!(sim.download.manager.admission().proven_window(), 4);
    let before = sim.charges.len();
    // One of the four completes: a slot is free below the limit.
    sim.complete_healthy(0);
    sim.run_until(t0 + 25 * SECOND);
    assert!(sim.ended.is_none(), "{:?}", sim.ended);
    let after = &sim.charges[before..];
    assert!(!after.is_empty(), "the range was not retried");
    assert_eq!(after[0], (1, false), "{after:?}");
    sim.teardown().await;
}

/// The four healthy runners of a resumed start on alternate eighths, with
/// the four unassigned eighths between them.
fn alternate_eighths() -> Vec<Range<u64>> {
    (0..4u64)
        .map(|i| 2 * i * TOTAL / 8..(2 * i + 1) * TOTAL / 8)
        .collect()
}

/// The same start when the four first deliver at the first tick, 1.5 s in:
/// until then every refusal is uncorroborated and charged. Without jitter
/// the ranges are retried at 0.5 s and 1.5 s, the second before the tick's
/// delivery, so a budget of two or three ends the task before any
/// corroboration; five has two to spare, and its later refusals are exempt.
#[tokio::test]
async fn a_resumed_start_with_late_delivery_charges_until_corroborated() {
    for budget in [2, 3, 5] {
        let mut sim = Sim::build(Server::Refuses, 8, &alternate_eighths(), budget, false).await;
        let t0 = sim.t0();
        sim.run_until(t0 + 20 * SECOND);
        let charged = |sim: &Sim| sim.charges.iter().map(|&(charged, _)| charged).max();
        if budget <= 3 {
            let Some(DownloadError::Transfer { source }) = sim.ended.take() else {
                panic!("budget {budget}: the task goes on: {:?}", sim.charges);
            };
            assert!(
                matches!(&*source, TaskError::RangeExhausted { attempts, .. } if *attempts == budget),
                "budget {budget}: {source:?}"
            );
            assert!(sim.charges.iter().all(|&(_, exempt)| !exempt));
        } else {
            assert!(sim.ended.is_none(), "{:?}", sim.ended);
            assert_eq!(charged(&sim), Some(3), "{:?}", sim.charges);
            assert!(sim.charges[12..].iter().all(|&(_, exempt)| exempt));
            assert_eq!(sim.limits[1].1, 4);
        }
        sim.teardown().await;
    }
}

/// A resumed start opens a connect for every unassigned range before any
/// runner delivers: each refusal then is uncorroborated and charged once.
/// A budget of one ends the task there; a larger one goes on, the next
/// refusals come beside the delivering runners above the window, and the
/// limit is lowered to the four. This needs the runners to deliver before
/// the first retries; see the test with late delivery.
#[tokio::test]
async fn a_resumed_start_charges_its_first_refusals() {
    let healthy = alternate_eighths();
    for budget in [1, 2, 3, 5] {
        let mut sim = Sim::build(Server::Refuses, 8, &healthy, budget, false).await;
        let t0 = sim.t0();
        if budget == 1 {
            let Some(DownloadError::Transfer { source }) = sim.ended.take() else {
                panic!("the task goes on");
            };
            assert!(matches!(
                &*source,
                TaskError::RangeExhausted { attempts: 1, .. }
            ));
            sim.teardown().await;
            continue;
        }
        let first: Vec<_> = sim.charges.clone();
        assert_eq!(first, [(1, false); 4], "budget {budget}");
        // Then the four deliver.
        sim.deliver();
        sim.run_until(t0 + 20 * SECOND);
        assert!(sim.ended.is_none(), "budget {budget}: {:?}", sim.ended);
        assert!(
            sim.charges[4..]
                .iter()
                .all(|&(charged, exempt)| charged == 1 && exempt),
            "budget {budget}: {:?}",
            sim.charges
        );
        assert_eq!(sim.limits[1].1, 4, "budget {budget}");
        sim.teardown().await;
    }
}

/// Eight proven runners, then the server goes from eight to seven: one of
/// them resets after its bytes, and its range, retried alone, is refused
/// with seven others in flight, inside the window of eight. Each refusal
/// is charged until three in a row and progress lower the limit to the
/// seven, which clamps the window; the range is then left to the steps.
/// A budget of up to three ends the task at its B-th charge; five has one
/// to spare.
#[tokio::test]
async fn a_shrinking_limit_charges_until_it_is_lowered() {
    for budget in [1, 2, 3, 5] {
        let mut sim = Sim::build(Server::Refuses, 8, &whole(8), budget, true).await;
        let t0 = sim.t0();
        sim.run_until(t0 + TICK);
        assert_eq!(sim.download.manager.admission().proven_window(), 8);
        sim.fail_healthy(7, reset());
        sim.run_until(t0 + 30 * SECOND);
        let charged: Vec<_> = sim.charges[1..]
            .iter()
            .map(|&(charged, _)| charged)
            .collect();
        if budget <= 3 {
            let Some(DownloadError::Transfer { source }) = sim.ended.take() else {
                panic!("budget {budget}: the task goes on: {charged:?}");
            };
            assert!(
                matches!(&*source, TaskError::RangeExhausted { attempts, .. } if *attempts == budget),
                "budget {budget}: {source:?}"
            );
            assert_eq!(charged, (1..=budget).collect::<Vec<_>>());
        } else {
            assert!(sim.ended.is_none(), "{:?}", sim.ended);
            assert_eq!(sim.limits[1].1, 7, "{:?}", sim.limits);
            let max = *charged.iter().max().unwrap();
            assert!((3..budget).contains(&max), "{charged:?}");
        }
        sim.teardown().await;
    }
}

/// A 503 in the body stream of a healthy runner while a step's trial is
/// connecting rejects the step: the wait doubles, counted from the end of
/// the congestion's hold, and the limit is not raised.
#[tokio::test]
async fn a_streaming_congestion_rejects_a_running_step() {
    let mut sim = Sim::new(Server::Hangs, 8, &eighths(4)).await;
    let t0 = sim.t0();
    let mut steps = Steps::default();
    sim.run_until(t0 + 3 * DEFAULT_CONNECT_TIMEOUT + 10 * SECOND);
    let (lowered_at, 4) = sim.limits[1] else {
        panic!("{:?}", sim.limits);
    };
    sim.run_recording_steps(lowered_at + 40 * SECOND, &mut steps);
    let [began] = steps.began[..] else {
        panic!("{steps:?}");
    };
    assert!(steps.ended.is_empty());
    sim.fail_healthy(0, congestion());
    let hold_end = sim.download.backoff.congestion_until().unwrap();
    sim.run_recording_steps(sim.next_tick, &mut steps);
    let [(ended, due)] = steps.ended[..] else {
        panic!("{steps:?}");
    };
    assert!(ended > began);
    assert_eq!(due, hold_end.max(ended) + Duration::from_secs(60));
    // The trial still connects: four are active, so the limit stays.
    assert_eq!(sim.download.strategy.limit_state().0, 4);
    assert_eq!(sim.limit(), 4);
    sim.teardown().await;
}

/// The writer is full for 15 s while refusals arrive beside the four that
/// deliver: the loop still handles every runner's end, but accepts no data,
/// so the silent rule waits. Once the writes complete, the next tick lowers
/// the limit.
#[tokio::test]
async fn a_full_writer_holds_the_silent_rule_until_it_drains() {
    let mut sim = Sim::new(Server::Refuses, 8, &eighths(4)).await;
    let t0 = sim.t0();
    sim.fill_writer();
    sim.run_until(t0 + 15 * SECOND);
    assert!(sim.failures.len() >= 3, "{:?}", sim.failures);
    assert!(sim.download.manager.admission().streak_count() >= 3);
    assert_eq!(sim.limits.len(), 1, "{:?}", sim.limits);
    assert_eq!(sim.download.policy.last_progress_at(), t0);

    let drained_at = sim.now;
    sim.drain_writer();
    sim.run_until(drained_at + 3 * TICK);
    assert_eq!(
        sim.limits[1..],
        [(first_tick_from(t0, drained_at + NANO), 4)]
    );
    sim.teardown().await;
}

/// Random sequences of connects, refusals, losses, congestion, failures
/// after bytes, completions, writer backpressure and ticks, on time and
/// late, through the manager's lifecycle handling (design §8.3).
///
/// The test keeps its own model of the connected runners and of the
/// running step's occupancy, from the events it reports, and checks the
/// invariants against the model, not against the evidence.
mod admission_properties {
    use std::collections::BTreeSet;

    use pretty_assertions::assert_eq;
    use proptest::prelude::*;

    use super::{super::super::super::strategy::ConcurrencyControlConfig, *};

    const MAX_CONNECTIONS: usize = 6;

    #[derive(Clone, Debug)]
    enum Op {
        /// The `n`th connecting runner reports `Started`.
        Connect(usize),
        /// Every running runner delivers, unless the writer is full.
        Deliver,
        /// The `n`th running runner finishes.
        Complete(usize),
        /// The `n`th connecting runner is refused.
        Refuse(usize),
        /// The `n`th running runner fails after its bytes.
        FailAfterBytes(usize),
        /// The `n`th connecting or running runner is lost.
        Lose(usize),
        /// The `n`th connecting or running runner gets a 503.
        Congest(usize),
        /// Writes fill the writer, which then takes no data.
        FillWriter,
        /// The parked writes complete, and the loop takes their completions.
        DrainWriter,
        /// A strategy tick one interval after the previous, at `speed` MiB/s.
        Tick(u64),
        /// A strategy tick three intervals after the previous.
        LateTick(u64),
        /// The loop wakes to retry the ranges whose backoff ended.
        Retry,
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            4 => any::<usize>().prop_map(Op::Connect),
            6 => Just(Op::Deliver),
            1 => any::<usize>().prop_map(Op::Complete),
            2 => any::<usize>().prop_map(Op::Refuse),
            1 => any::<usize>().prop_map(Op::FailAfterBytes),
            1 => any::<usize>().prop_map(Op::Lose),
            1 => any::<usize>().prop_map(Op::Congest),
            1 => Just(Op::FillWriter),
            2 => Just(Op::DrainWriter),
            30 => (0..8u64).prop_map(Op::Tick),
            1 => (0..8u64).prop_map(Op::LateTick),
            1 => Just(Op::Retry),
        ]
    }

    fn nth<T: Copy>(items: &[T], n: usize) -> Option<T> {
        (!items.is_empty()).then(|| items[n % items.len()])
    }

    fn io() -> TaskError {
        TaskError::StreamError {
            source: AdapterError::Retryable {
                source: RetryableError::Io {
                    source: Arc::new(std::io::Error::from(std::io::ErrorKind::ConnectionRefused)),
                },
            },
        }
    }

    /// What the invariants compare, taken before and after each step.
    #[derive(Clone, Debug, PartialEq)]
    struct Snapshot {
        limit: usize,
        step: Option<Instant>,
        due: Option<Instant>,
        cap: usize,
        active: usize,
        allocated: usize,
        loop_limit: usize,
        streak: usize,
        third: Option<Instant>,
        congestion_pending: bool,
        rejected: bool,
        unclear: bool,
        writer_ready: bool,
        last_progress_at: Instant,
        /// The model's occupancy of the running step.
        occupied_since: Option<Instant>,
    }

    struct Prop {
        download: DownloadLoop,
        _file_writer: FileWriter,
        rt: ScriptedRuntime,
        now: Instant,
        /// The runners the loop spawned, parked and never polled.
        runners: Vec<FutureObj<'static, ()>>,
        /// The writes parked by [`Op::FillWriter`].
        writes: Vec<FutureObj<'static, ()>>,
        /// The model: the runners the test reported `Started` for and has
        /// not ended since.
        connected: BTreeSet<crate::task::RunnerId>,
        /// The model: since when the running step's trial has been
        /// connected without a break.
        occupied_since: Option<Instant>,
    }

    impl Prop {
        /// The runners stay parked; the test reports their lifecycle events
        /// itself.
        fn new(rt: ScriptedRuntime, max_connections: usize, t0: Instant) -> Self {
            let (mut download, file_writer) =
                futures::executor::block_on(admission_loop(&rt, false, max_connections, u32::MAX));
            download.policy.on_progress(t0);
            Self {
                download,
                _file_writer: file_writer,
                rt,
                now: t0,
                runners: Vec::new(),
                writes: Vec::new(),
                connected: BTreeSet::new(),
                occupied_since: None,
            }
        }

        fn snapshot(&self) -> Snapshot {
            let (limit, step, due, _) = self.download.strategy.limit_state();
            let admission = self.download.manager.admission();
            Snapshot {
                limit,
                step,
                due,
                cap: self.download.strategy.cap(),
                active: self.download.manager.get_active_runners_count(),
                allocated: self.download.manager.allocated_runner_count(),
                loop_limit: self.download.max_concurrency,
                streak: admission.streak_count(),
                third: admission.third_refusal_at(),
                congestion_pending: admission.congestion_pending(),
                rejected: admission.step_rejected(),
                unclear: admission.step_unclear(),
                writer_ready: self.download.writer.can_write(),
                last_progress_at: self.download.policy.last_progress_at(),
                occupied_since: self.occupied_since,
            }
        }

        fn runners(&self, statuses: &[ChunkStatus]) -> Vec<crate::task::RunnerId> {
            (0..MAX_CONNECTIONS)
                .filter(|&id| {
                    self.download
                        .manager
                        .get_runner_state(id)
                        .is_some_and(|state| statuses.contains(&state.status))
                })
                .collect()
        }

        /// Hand `tick` to the loop; `false` once the task ends.
        fn on_runner(&mut self, tick: RunnerTick) -> bool {
            match self.download.on_runner(tick, self.now) {
                Ok(flow) => flow.is_continue(),
                Err(_) => false,
            }
        }

        fn tick(&mut self, intervals: u32, speed: u64) {
            self.now += TICK * intervals;
            self.download.throughput.speed = (speed << 20) as f64;
            let active = self.download.manager.get_active_runners_count().max(1);
            self.download.throughput.per_runner_speed = (speed << 20) as f64 / active as f64;
            self.download.on_strategy_tick(self.now);
        }

        /// Report `item` for runner `id`, and update the model.
        fn report(&mut self, id: crate::task::RunnerId, item: LifecycleEvent) -> bool {
            let refusal = matches!(
                &item,
                LifecycleEvent::Stopped(crate::runner::StoppedReason::Failed(error))
                    if matches!(error, TaskError::StreamError { source: AdapterError::Retryable { .. } })
            ) && !self.connected.contains(&id);
            if matches!(item, LifecycleEvent::Started) {
                self.connected.insert(id);
            } else {
                self.connected.remove(&id);
            }
            let charged = self.charged(id);
            let tick = self.download.manager.inject_lifecycle(id, item);
            check_charge(charged, &tick, refusal);
            self.on_runner(tick)
        }

        /// The charged failures of runner `id`'s range before it fails.
        fn charged(&self, id: crate::task::RunnerId) -> u32 {
            self.download
                .manager
                .get_runner_state(id)
                .map_or(0, |state| state.charged)
        }

        /// Apply `op`; `false` once the task ends.
        fn apply(&mut self, op: &Op) -> bool {
            use ChunkStatus::{Pending, Running};

            let going_on = match *op {
                Op::Connect(n) => match nth(&self.runners(&[Pending]), n) {
                    Some(id) => self.report(id, LifecycleEvent::Started),
                    None => true,
                },
                Op::Deliver => {
                    // Under backpressure the loop polls no data.
                    if self.download.writer.can_write() {
                        for id in self.runners(&[Running]) {
                            self.download
                                .manager
                                .update_progress(id, DELIVERED)
                                .unwrap();
                        }
                        self.download.policy.on_progress(self.now);
                    }
                    true
                }
                Op::Complete(n) => match nth(&self.runners(&[Running]), n) {
                    Some(id) => {
                        self.download.manager.update_progress(id, u64::MAX).unwrap();
                        self.report(id, LifecycleEvent::finished())
                    }
                    None => true,
                },
                Op::Refuse(n) => match nth(&self.runners(&[Pending]), n) {
                    Some(id) => self.report(id, LifecycleEvent::failed(io())),
                    None => true,
                },
                Op::FailAfterBytes(n) => match nth(&self.runners(&[Running]), n) {
                    Some(id) => self.report(id, LifecycleEvent::failed(reset())),
                    None => true,
                },
                Op::Lose(n) => match nth(&self.runners(&[Pending, Running]), n) {
                    Some(id) => {
                        self.connected.remove(&id);
                        let charged = self.charged(id);
                        let tick = self.download.manager.inject_lost(id);
                        check_charge(charged, &tick, false);
                        self.on_runner(tick)
                    }
                    None => true,
                },
                Op::Congest(n) => match nth(&self.runners(&[Pending, Running]), n) {
                    Some(id) => self.report(id, LifecycleEvent::failed(congestion())),
                    None => true,
                },
                Op::FillWriter => {
                    self.runners.extend(self.rt.take_parked());
                    while self.download.writer.can_write() {
                        self.download
                            .writer
                            .write_range(0..1, bytes::Bytes::from_static(b"x"))
                            .unwrap();
                    }
                    self.writes.extend(self.rt.take_parked());
                    true
                }
                Op::DrainWriter => {
                    drain(&mut self.download, &self.rt, &mut self.writes, self.now);
                    true
                }
                Op::Tick(speed) => {
                    self.tick(1, speed);
                    true
                }
                Op::LateTick(speed) => {
                    self.tick(3, speed);
                    true
                }
                Op::Retry => {
                    self.download.retry_backed_off(self.now);
                    true
                }
            };
            self.runners.extend(self.rt.take_parked());
            going_on
        }

        /// Update the model's occupancy after an operation, from the step
        /// the controller runs and the runners the model has connected.
        fn update_occupancy(&mut self, before: Option<Instant>) {
            let (limit, step, _, _) = self.download.strategy.limit_state();
            let reached = self.connected.len() > limit;
            self.occupied_since = match step {
                None => None,
                Some(began) if before != Some(began) => reached.then_some(began),
                Some(_) if !reached => None,
                Some(_) => self.occupied_since.or(Some(self.now)),
            };
        }
    }

    /// Invariant 7: the range's charged failures grow only by a failure
    /// that is not exempt, at most once per attempt; only a refusal is
    /// exempt, and a refusal that is not exempt is charged.
    fn check_charge(before: u32, tick: &RunnerTick, refusal: bool) {
        let Some(failure) = &tick.failure else {
            return;
        };
        let grown = failure.charged - before;
        assert!(grown <= 1, "{failure:?}");
        if failure.exempt {
            assert!(refusal && grown == 0, "{failure:?}");
        } else if refusal {
            assert_eq!(grown, 1, "{failure:?}");
        }
    }

    /// Run `ops`; returns the snapshot after each, and every action the
    /// loop applied, until the task ended.
    fn run(max_connections: usize, t0: Instant, ops: &[Op]) -> (Vec<Snapshot>, Vec<String>) {
        let rt = ScriptedRuntime::new(TimerKind::Scripted);
        let mut prop = Prop::new(rt, max_connections, t0);
        let config = ConcurrencyControlConfig::default();
        // The first tick creates the first runner.
        prop.download.on_strategy_tick(t0);
        let mut before = prop.snapshot();
        let mut snapshots = Vec::new();
        for op in ops {
            let going_on = prop.apply(op);
            prop.update_occupancy(before.step);
            let after = prop.snapshot();
            check(&prop, &config, max_connections, op, &before, &after);
            snapshots.push(after.clone());
            if !going_on {
                break;
            }
            before = after;
        }
        (snapshots, std::mem::take(&mut prop.download.applied))
    }

    /// The invariants of design §8.3 across one operation.
    fn check(
        prop: &Prop,
        config: &ConcurrencyControlConfig,
        max_connections: usize,
        op: &Op,
        before: &Snapshot,
        after: &Snapshot,
    ) {
        let now = prop.now;
        let ticked = matches!(op, Op::Tick(_) | Op::LateTick(_));
        let late = matches!(op, Op::LateTick(_));
        // The model and the evidence agree on what is connected, and on
        // the occupancy.
        let admission = prop.download.manager.admission();
        assert_eq!(admission.connected(), prop.connected.len(), "{op:?}");
        assert_eq!(admission.occupied_since(), after.occupied_since, "{op:?}");
        // 1. The limit stays within bounds; nothing is added at the cap.
        assert!((1..=max_connections).contains(&after.limit), "{after:?}");
        if after.allocated > before.allocated {
            assert!(
                after.active <= after.cap,
                "{op:?} added runners above the cap: {before:?} -> {after:?}"
            );
        }
        // 2, 3. It rises only by one, when a step is accepted: an on-time
        // tick, after the model's unbroken occupancy for the observation
        // period.
        if after.limit > before.limit {
            assert_eq!(after.limit, before.limit + 1, "{op:?}");
            assert!(ticked && !late, "{op:?} raised the limit");
            assert!(before.step.is_some(), "raised without a step");
            let since = before.occupied_since.expect("occupied");
            assert!(now - since >= config.observation, "{before:?}");
            assert!(!before.congestion_pending && !before.rejected);
        }
        // 4. The silent rule lowers only with progress after the third
        // refusal, and a writer that takes data; a congestion lowers at
        // once.
        if after.limit < before.limit {
            assert!(ticked, "{op:?} lowered the limit");
            if !before.congestion_pending {
                assert!(!late && before.step.is_none(), "{op:?}: {before:?}");
                assert!(before.writer_ready, "lowered behind a full writer");
                assert!(before.streak >= 3, "{before:?}");
                let third = before.third.expect("the third refusal was stamped");
                assert!(before.last_progress_at >= third + config.tick_interval);
            }
        }
        // 5. A step ends at the first tick at or after its deadline.
        if ticked
            && let Some(began) = before.step
            && now >= began + config.step_deadline
        {
            assert_ne!(after.step, Some(began), "the step outlived its deadline");
        }
        // 6. Failures after `Started`, losses, completions and the writer
        // create no refusal evidence and change no limit.
        if matches!(
            op,
            Op::FailAfterBytes(_)
                | Op::Lose(_)
                | Op::Complete(_)
                | Op::FillWriter
                | Op::DrainWriter
        ) {
            assert!(after.streak <= before.streak, "{op:?}");
            assert_eq!(after.congestion_pending, before.congestion_pending);
            assert_eq!(
                (after.rejected, after.unclear),
                (before.rejected, before.unclear)
            );
        }
        if !ticked {
            assert_eq!(
                (after.limit, after.step),
                (before.limit, before.step),
                "{op:?}"
            );
        }
        // The loop's limit follows the controller's cap.
        assert_eq!(after.loop_limit, after.cap, "{op:?}");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn the_limit_moves_only_on_its_evidence(
            max_connections in 1..=MAX_CONNECTIONS,
            ops in prop::collection::vec(op(), 1..400),
        ) {
            let t0 = Instant::now();
            let first = run(max_connections, t0, &ops);
            // 8. The same operations at the same instants apply the same
            // actions.
            let second = run(max_connections, t0, &ops);
            prop_assert_eq!(first, second);
        }
    }
}

#[cfg(all(feature = "reqwest", feature = "tokio"))]
mod fault_server {
    use bolt_load_tests::adapter::{
        fault::LimitMode,
        http_server::create_limited_range_server,
        simple::{calculate_blake3, create_deterministic_content},
    };
    use pretty_assertions::assert_eq;

    use super::{super::super::super::strategy::StrategyAction, *};
    use crate::task::Progress;

    /// The limit controller's state, see `StrategyControl::limit_state`.
    type LimitState = (usize, Option<Instant>, Option<Instant>, Duration);

    /// Aborts the server task when dropped, also when a test panics.
    struct AbortOnDrop(tokio::task::JoinHandle<()>);

    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    /// Handle the loop's events one by one until `done` holds, or until
    /// the loop ends when `done` is `None`; record each change of the
    /// limit, and each change of the limit controller's state, whose
    /// instants are the loop's own.
    async fn drive(
        download: &mut DownloadLoop,
        limits: &mut Vec<(Instant, usize)>,
        states: &mut Vec<LimitState>,
        charges: &mut Vec<u32>,
        mut done: Option<&mut dyn FnMut(&DownloadLoop) -> bool>,
    ) -> Result<()> {
        loop {
            if done.as_mut().is_some_and(|done| done(download)) {
                return Ok(());
            }
            let event = std::future::poll_fn(|cx| download.poll_event(cx)).await;
            if let LoopEvent::Runner(RunnerTick {
                failure: Some(failure),
                ..
            }) = &event
            {
                charges.push(failure.charged);
            }
            let flow = download.handle_event(event)?;
            if download.max_concurrency != limits.last().unwrap().1 {
                limits.push((Instant::now(), download.max_concurrency));
            }
            let state = download.strategy.limit_state();
            if states.last() != Some(&state) {
                states.push(state);
            }
            if flow.is_break() {
                return match done {
                    None => Ok(()),
                    Some(_) => Err(DownloadError::other(
                        "the loop ended before the condition held",
                    )),
                };
            }
        }
    }

    /// Await `fut`, failing the test if it takes longer than a minute: the
    /// download takes about twenty seconds.
    async fn within_a_minute<T>(fut: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(60), fut)
            .await
            .expect("the download never ended")
    }

    fn running(download: &DownloadLoop, runner_id: crate::task::RunnerId) -> bool {
        download
            .manager
            .get_runner_state(runner_id)
            .is_some_and(|state| state.status == ChunkStatus::Running)
    }

    /// Whether runner `runner_id` runs and had bytes accepted.
    fn proven(download: &DownloadLoop, runner_id: crate::task::RunnerId) -> bool {
        download
            .manager
            .get_runner_state(runner_id)
            .is_some_and(|state| {
                state.status == ChunkStatus::Running && !state.downloaded.is_empty()
            })
    }

    /// The silent limit of design §8.3 against the fault server, with the
    /// reqwest adapter and the loop's real timers: eight connections
    /// allowed, three admitted, the others turned away as `mode` says.
    ///
    /// The test opens the fourth connection itself by splitting: a task
    /// only splits on its own when the dynamic strategy's speed samples
    /// call for it. The download completes with the right content, the
    /// server never serves more than three at once, the limit is lowered
    /// to the three, and no connection is turned away after that.
    async fn a_silent_limit_completes(mode: LimitMode) {
        use bolt_load_adapter::reqwest::IntoReqwestAdapter;

        const RATE: u64 = 256 << 10;
        // Four ranges of 10 s each at the rate: the limit is lowered
        // before the first of them ends.
        const SIZE: u64 = 40 * RATE;
        let dir = tempfile::TempDir::new().unwrap();
        let content = bytes::Bytes::from(create_deterministic_content(SIZE as usize));
        let (port, server, log) = create_limited_range_server(content.clone(), 3, mode, RATE)
            .await
            .unwrap();
        let server = AbortOnDrop(server);
        let url = reqwest::Url::parse(&format!("http://127.0.0.1:{port}/file")).unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let adapter =
            DynBoltLoadAdapter::new_arc(client.into_reqwest_adapter((reqwest::Method::GET, url)));
        let path = dir.path().join("out.bin");
        let file_writer = FileWriter::new(&path, SIZE).await.unwrap();
        let options = TaskOptions::default().connect_timeout(Duration::from_millis(300));
        let spawner = RunnerSpawner::new(
            ThreadedRuntimeImpl::new_tokio_rt(),
            adapter,
            WaitGroup::new(),
            CancellationToken::new(),
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .connect_timeout(options.connect_timeout);
        let (reporter, _status) = ProgressReporter::for_test();
        let mut download = DownloadLoop::new(
            SIZE,
            spawner,
            &file_writer,
            reporter,
            &options,
            CancellationToken::new(),
        );
        let mut limits = vec![(Instant::now(), download.max_concurrency)];
        let mut states = vec![download.strategy.limit_state()];
        let mut charges = Vec::new();

        // The first step creates the first runner.
        download.on_strategy_tick(Instant::now());
        let driven = within_a_minute(async {
            drive(
                &mut download,
                &mut limits,
                &mut states,
                &mut charges,
                Some(&mut |d| running(d, 0)),
            )
            .await?;
            download.apply(StrategyAction::SplitGivenTask(0), Instant::now());
            drive(
                &mut download,
                &mut limits,
                &mut states,
                &mut charges,
                Some(&mut |d| proven(d, 0) && proven(d, 1)),
            )
            .await?;
            // Both split: a fourth connection, refused beside two proven.
            download.apply(StrategyAction::SplitAllTask(1), Instant::now());
            drive(&mut download, &mut limits, &mut states, &mut charges, None).await
        })
        .await;
        let mut progress = Progress::default();
        let finished = within_a_minute(download.finish(driven, &mut progress)).await;
        let finalized = file_writer.finalize().await;
        drop(server);

        finished.expect("the download completes");
        finalized.unwrap();
        assert_eq!(progress.downloaded, SIZE);
        assert_eq!(
            calculate_blake3(&std::fs::read(&path).unwrap()),
            calculate_blake3(&content)
        );
        assert!(log.max_open() <= 3, "{mode:?}: {:?}", log.events());
        // Every refusal came beside proven connections, above the proven
        // window: no range was charged (design §14.13).
        assert!(
            !charges.is_empty() && charges.iter().all(|c| *c == 0),
            "{mode:?}: {charges:?}"
        );
        // Lowered to the three first; the cadence of recovery is asserted
        // in the tests with injected time.
        assert!(
            limits.starts_with(&[(limits[0].0, 8)]) && limits.get(1).is_some_and(|l| l.1 == 3),
            "{mode:?}: limits {limits:?}"
        );
        assert!(
            states.iter().all(|(limit, ..)| *limit == 3 || *limit == 8),
            "{mode:?}: the limit rose: {states:?}"
        );
        // The controller's own instants, taken before it acted: when it
        // lowered the limit, and when each step began.
        let base = Duration::from_secs(30);
        let lowered_at = states
            .iter()
            .find(|(limit, ..)| *limit == 3)
            .and_then(|(_, _, due, _)| *due)
            .expect("the lowering sets the next step's due time")
            - base;
        let steps: Vec<Instant> = states
            .windows(2)
            .filter_map(|pair| match (pair[0].1, pair[1].1) {
                (None, Some(began)) => Some(began),
                _ => None,
            })
            .collect();
        // The server's log is compared with nothing the loop recorded: when
        // the server logs a connection it turned away may lag the loop's
        // instants, as with a connect that is never answered. Which
        // connections the steps opened, and when, is asserted with
        // injected time.
        let turned_away = log.turned_away();
        assert!(turned_away.len() >= 3, "{mode:?}: {turned_away:?}");
        if let Some(&first) = steps.first() {
            assert!(first >= lowered_at + base, "{mode:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_silent_limit_that_closes_connections_completes() {
        a_silent_limit_completes(LimitMode::Close).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_silent_limit_that_never_answers_completes() {
        a_silent_limit_completes(LimitMode::Hang).await;
    }
}
