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
    let options =
        TaskOptions::default().max_connections(NonZeroUsize::new(max_connections).unwrap());
    let mut download = DownloadLoop::new(
        TOTAL,
        spawner,
        &file_writer,
        reporter,
        &options,
        CancellationToken::new(),
    );
    download.backoff = Backoff::without_jitter();
    (download, file_writer)
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
    now: Instant,
    next_tick: Instant,
    /// When the loop spawned each runner.
    spawned: Vec<Instant>,
    /// When the loop handled each failed attempt, and its error.
    failures: Vec<(Instant, TaskError)>,
    /// The loop's limit, and each change with its time.
    limits: Vec<(Instant, usize)>,
}

impl Sim {
    /// A loop with `max_connections`, and a connected runner on each of
    /// `healthy` that has delivered bytes; the first tick at `t0` creates
    /// runners for the rest.
    async fn new(server: Server, max_connections: usize, healthy: &[Range<u64>]) -> Self {
        let rt = ScriptedRuntime::new(TimerKind::Scripted);
        let (mut download, file_writer) =
            admission_loop(&rt, server == Server::Hangs, max_connections).await;
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
            now: t0,
            next_tick: t0 + TICK,
            spawned: Vec::new(),
            failures: Vec::new(),
            limits: vec![(t0, max_connections)],
        };
        sim.deliver();
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

    /// Every running runner delivers, and its bytes are written.
    fn deliver(&mut self) {
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
        while let Some(tick) = self.download.manager.tick().now_or_never() {
            if let Some(failure) = &tick.failure {
                self.failures.push((now, failure.error.clone()));
            }
            let flow = self
                .download
                .on_runner(tick, now)
                .expect("the task goes on");
            assert!(flow.is_continue());
        }
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
        loop {
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

    /// Aborts the server task when dropped, also when a test panics.
    struct AbortOnDrop(tokio::task::JoinHandle<()>);

    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    /// Handle the loop's events one by one until `done` holds, or until
    /// the loop ends when `done` is `None`; record each change of the
    /// limit and when it happened.
    async fn drive(
        download: &mut DownloadLoop,
        limits: &mut Vec<(Instant, usize)>,
        mut done: Option<&mut dyn FnMut(&DownloadLoop) -> bool>,
    ) -> Result<()> {
        loop {
            if done.as_mut().is_some_and(|done| done(download)) {
                return Ok(());
            }
            let event = std::future::poll_fn(|cx| download.poll_event(cx)).await;
            let flow = download.handle_event(event)?;
            if download.max_concurrency != limits.last().unwrap().1 {
                limits.push((Instant::now(), download.max_concurrency));
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

        // The first step creates the first runner.
        download.on_strategy_tick(Instant::now());
        let driven = within_a_minute(async {
            drive(&mut download, &mut limits, Some(&mut |d| running(d, 0))).await?;
            download.apply(StrategyAction::SplitGivenTask(0), Instant::now());
            drive(&mut download, &mut limits, Some(&mut |d| running(d, 1))).await?;
            // Both split: a fourth connection.
            download.apply(StrategyAction::SplitAllTask(1), Instant::now());
            drive(&mut download, &mut limits, None).await
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
        // Lowered to the three first; what recovery does after that is
        // asserted in the tests with injected time.
        assert!(
            limits.starts_with(&[(limits[0].0, 8)]) && limits.get(1).is_some_and(|l| l.1 == 3),
            "{mode:?}: limits {limits:?}"
        );
        let turned_away = log.turned_away();
        assert!(turned_away.len() >= 3, "{mode:?}: {turned_away:?}");
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
