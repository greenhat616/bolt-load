use std::{
    ops::{ControlFlow, Range},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use async_waitgroup::WaitGroup;
use bolt_load_core::adapter::AdapterError;
use bolt_load_utils::telemetry::*;
use futures::{FutureExt, future::Either, task::SpawnExt};
use runner_manager::{MeterSnapshot, RunnerTick, TaskState};
use smol_cancellation_token::CancellationToken;

use super::{
    Result, TaskInstance,
    drive::{self, Transfer},
    output_file,
};
use crate::{
    adapter::{AnyAdapter, BoltLoadAdapter, UnretryableError},
    runner::{StreamConnector, TaskError, TaskRunner},
    runtime::{LocalRuntimeBuilderImpl, ThreadedRuntimeImpl, Timer, TimerBuilder, TimerImpl},
    task::{
        Progress, RunnerId, TaskOptions,
        instance::{
            ProgressWithSpeed, TaskControl, TaskEvent, TaskInstanceError,
            sampler::{DEFAULT_SAMPLE_INTERVAL, SpeedSampler},
        },
        meta,
    },
};

mod chunk_planner;
/// File writer implementations for concurrent downloads.
///
/// This module is public for benchmarking purposes.
pub mod file_writer;
mod runner_manager;
mod strategy;

use chunk_planner::*;
use file_writer::*;
pub use runner_manager::{ControlReceiver, ControlSender, RunnerManager, RunnerRegistration};
use strategy::*;

static DEFAULT_MAX_CONCURRENCY: usize = 4;

/// Default capacity for control channels
#[cfg_attr(not(test), expect(dead_code, reason = "only used by tests"))]
const DEFAULT_CONTROL_CHANNEL_CAPACITY: usize = 8;

#[expect(
    dead_code,
    reason = "the initial progress is kept for resumable downloads"
)]
pub struct ConcurrentTask {
    threaded_rt: ThreadedRuntimeImpl,
    local_runtime_builder: Option<LocalRuntimeBuilderImpl>,
    task: Option<TaskControl>,
    progress: Progress,
    options: TaskOptions,
}

impl ConcurrentTask {
    pub fn new(
        threaded_rt: ThreadedRuntimeImpl,
        local_runtime_builder: Option<LocalRuntimeBuilderImpl>,
    ) -> Self {
        Self::with_options(threaded_rt, local_runtime_builder, TaskOptions::default())
    }

    pub fn with_options(
        threaded_rt: ThreadedRuntimeImpl,
        local_runtime_builder: Option<LocalRuntimeBuilderImpl>,
        options: TaskOptions,
    ) -> Self {
        Self {
            threaded_rt,
            local_runtime_builder,
            task: None,
            progress: Progress::default(),
            options,
        }
    }
}

/// One download attempt, run on a local runtime by [`drive::spawn_on_local`].
pub(super) fn concurrent_attempt(
    adapter: Arc<AnyAdapter>,
    path: PathBuf,
    event_tx: async_channel::Sender<TaskEvent>,
    cancel_token: CancellationToken,
    rt: ThreadedRuntimeImpl,
    options: TaskOptions,
) -> impl Future<Output = ()> + 'static {
    let inner = ConcurrentTaskInner::new(adapter, path, event_tx.clone(), rt, options);
    let task = drive::drive(inner, event_tx, cancel_token);
    #[cfg(feature = "tracing")]
    let task = tracing::Instrument::instrument(
        task,
        tracing::trace_span!(parent: None, "ConcurrentTask::background_task"),
    );
    task
}

impl TaskInstance for ConcurrentTask {
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, name = "ConcurrentTask::run")
    )]
    fn run(
        &mut self,
        adapter: Arc<AnyAdapter>,
        path: PathBuf,
        event_tx: async_channel::Sender<TaskEvent>,
        cancel_token: CancellationToken,
    ) -> Result<()> {
        let options = self.options.clone();
        let task = drive::spawn_on_local(
            self.threaded_rt.clone(),
            self.local_runtime_builder.clone(),
            cancel_token.clone(),
            // Built on the local runtime's thread, so it does not need to be `Send`.
            move |rt| concurrent_attempt(adapter, path, event_tx, cancel_token, rt, options),
        );
        self.task = Some(task);
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(task) = self.task.as_mut() {
            task.stop().await;
        }
        Ok(())
    }

    async fn wait(&mut self) -> Result<()> {
        if let Some(task) = self.task.as_mut() {
            task.wait().await;
        }
        Ok(())
    }
}

pub struct ConcurrentTaskInner {
    rt: ThreadedRuntimeImpl,
    path: PathBuf,
    adapter: Arc<AnyAdapter>,
    /// The initial progress of the task, for task resume
    progress: Progress,
    event_tx: async_channel::Sender<TaskEvent>,
    options: TaskOptions,
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[expect(dead_code, reason = "reserved for persisting runner states")]
enum RunnerStatus {
    Running,
    Finished,
}

impl ConcurrentTaskInner {
    fn new(
        adapter: Arc<AnyAdapter>,
        path: PathBuf,
        event_tx: async_channel::Sender<TaskEvent>,
        rt: ThreadedRuntimeImpl,
        options: TaskOptions,
    ) -> Self {
        Self {
            rt,
            path,
            adapter,
            progress: Progress::default(),
            event_tx,
            options,
        }
    }
    /// Retrieve the meta data of the file
    async fn retrieve_meta(&mut self) -> Result<()> {
        let meta = meta::retrieve_meta(&self.adapter, &self.options.meta_retry, Some(&self.rt))
            .await
            .map_err(TaskInstanceError::new_retrieve_meta_failed)?;
        let total = if meta.content_size == 0 {
            return Err(TaskInstanceError::RetrieveMetaFailed {
                source: AdapterError::Unretryable {
                    source: UnretryableError::Whatever {
                        message: "content size is 0; concurrent task does not support 0-size file"
                            .to_string(),
                        source: None,
                    },
                },
            });
        } else {
            meta.content_size
        };
        if self.progress.total.is_some_and(|t| t != total) {
            self.progress.total = Some(total);
            self.progress.downloaded = 0;
        } else if self.progress.total.is_none() {
            self.progress.total = Some(total);
        }
        Ok(())
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    async fn create_file_writer(&self, tmp_path: &Path) -> Result<FileWriter> {
        let total_size = self.progress.total.ok_or_else(|| {
            TaskInstanceError::new_failed(TaskError::Other {
                message: "File writer creation called before meta retrieval".to_string(),
            })
        })?;
        FileWriter::new(tmp_path, total_size).await.map_err(|e| {
            TaskInstanceError::new_failed(TaskError::Other {
                message: e.to_string(),
            })
        })
    }

    /// Download into the temporary file, then publish it or discard it.
    ///
    /// Returns only after every owned worker has exited.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    async fn download(&mut self, cancel_token: &CancellationToken) -> Result<()> {
        let tmp_path = output_file::tmp_path_for(&self.path);
        let transfer = self.download_to(&tmp_path, cancel_token).await;
        output_file::commit(transfer, &tmp_path, &self.path, cancel_token).await
    }

    /// Run the transfer and tear it down on every path: success, failure and
    /// cancellation. Teardown never short-circuits, so all spawned work is joined.
    async fn download_to(
        &mut self,
        tmp_path: &Path,
        cancel_token: &CancellationToken,
    ) -> Result<()> {
        let total = self
            .progress
            .total
            .expect("A concurrent task should have a total size");
        let file_writer = self.create_file_writer(tmp_path).await?;

        let runners_cancel_token = cancel_token.child_token();
        // Fallback cancellation signal on unwinding; `DownloadLoop::finish` cancels explicitly.
        let _runner_cancel_guard = runners_cancel_token.clone().drop_guard();
        let spawner = RunnerSpawner::new(
            self.rt.clone(),
            self.adapter.clone(),
            WaitGroup::new(),
            runners_cancel_token,
            self.options.stream_idle_timeout,
        );

        let mut download = DownloadLoop::new(
            total,
            spawner,
            &file_writer,
            self.event_tx.clone(),
            &self.options,
        );
        let transfer_result = download.run(cancel_token).await;
        let transfer_result = download.finish(transfer_result, &mut self.progress).await;
        // Always join the writer workers, so the file handle is released before
        // the temporary file is renamed or removed.
        let finalize_result = file_writer.finalize().await.map_err(|e| {
            TaskInstanceError::new_failed(TaskError::Other {
                message: e.to_string(),
            })
        });

        // The first error takes precedence.
        transfer_result.and(finalize_result)
    }
}

/// Spawns range runners for one download attempt.
///
/// Every spawned task holds a clone of the wait group until it has exited.
struct RunnerSpawner {
    rt: ThreadedRuntimeImpl,
    adapter: Arc<AnyAdapter>,
    wg: WaitGroup,
    cancel_token: CancellationToken,
    idle_timeout: Duration,
}

impl RunnerSpawner {
    fn new(
        rt: ThreadedRuntimeImpl,
        adapter: Arc<AnyAdapter>,
        wg: WaitGroup,
        cancel_token: CancellationToken,
        idle_timeout: Duration,
    ) -> Self {
        Self {
            rt,
            adapter,
            wg,
            cancel_token,
            idle_timeout,
        }
    }

    /// Connect `range` and run a runner on it in one spawned task; the receiver
    /// yields the runner's channels.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, fields(runner_id, range))
    )]
    fn spawn(
        &self,
        range: Range<u64>,
        runner_id: RunnerId,
        control_rx: ControlReceiver,
    ) -> runner_manager::PendingRunnerReceiver {
        use runner_manager::pending::{PendingRunnerError, RunnerBuilderOutput};

        let (tx, rx) = oneshot::channel();
        let (start, end) = (range.start, range.end);
        let wg = self.wg.clone();
        let adapter = self.adapter.clone();
        let cancel_token = self.cancel_token.clone();
        let idle_timeout = self.idle_timeout;
        let worker = async move {
            // Declared first, so the runner and its upstream stream drop before
            // the wait group is released.
            let _wg = wg;
            trace!("[TASK] background range runner: id: {runner_id}, range: {range:?}");

            // Connect the stream, abandoning the request on cancellation
            let connector =
                StreamConnector::from(async move { adapter.range_stream(start, end).await });
            let connected = {
                let connect = connector.connect().fuse();
                let cancelled = cancel_token.cancelled().fuse();
                futures::pin_mut!(connect, cancelled);
                futures::select_biased! {
                    _ = cancelled => return,
                    res = connect => res,
                }
            };
            if cancel_token.is_cancelled() {
                return;
            }
            let stream = match connected {
                Ok(stream) => stream,
                Err(e) => {
                    error!("failed to connect background range runner: {e:?}");
                    let _ = tx.send(Err(PendingRunnerError::Connection { source: e }));
                    return;
                }
            };

            let built = TaskRunner::builder()
                .total(end - start)
                .stream(stream)
                .runner_id(runner_id)
                .control_signal(control_rx)
                .cancel_token(cancel_token)
                .idle_timeout(idle_timeout)
                .build();
            let (mut runner, lifecycle_rx, data_rx) = match built {
                Ok(built) => built,
                Err(e) => {
                    // Build error - should not happen with proper setup
                    error!("failed to build task runner: {e:?}");
                    let _ = tx.send(Err(PendingRunnerError::ReceiverClosed));
                    return;
                }
            };
            if let Err(e) = tx.send(Ok(RunnerBuilderOutput {
                data_rx,
                lifecycle_rx,
            })) {
                error!("failed to send channels to pending receiver: {e:?}");
                return;
            }

            let run = runner.run();
            #[cfg(feature = "tracing")]
            let run = tracing::Instrument::instrument(
                run,
                tracing::trace_span!("background_task::runner"),
            );
            run.await;
        };
        #[cfg(feature = "tracing")]
        let worker = tracing::Instrument::instrument(
            worker,
            tracing::trace_span!(
                parent: None,
                "background_task::runner_connector",
                runner_id = runner_id,
                range = ?start..end,
            ),
        );
        if let Err(e) = self.rt.spawn(worker) {
            error!("failed to spawn background range runner: {e}");
            return runner_manager::pending::failed_receiver(PendingRunnerError::Spawn {
                message: e.to_string(),
            });
        }
        rx
    }
}

/// Download and write throughput, sampled on the throughput timer.
struct Throughput {
    sampler: SpeedSampler,
    write_sampler: SpeedSampler,
    /// Bytes written since the last sample.
    written_bytes: usize,
    speed: f64,
    per_runner_speed: f64,
}

impl Throughput {
    fn new() -> Self {
        Self {
            sampler: SpeedSampler::new(),
            write_sampler: SpeedSampler::new(),
            written_bytes: 0,
            speed: 0.0,
            per_runner_speed: 0.0,
        }
    }

    /// Returns the download and write speed.
    fn sample(&mut self, meters: MeterSnapshot) -> (f64, f64) {
        let MeterSnapshot { runners, mut bytes } = meters;
        let (_, speed) = self.sampler.sample(&mut bytes);
        let (_, write_speed) = self.write_sampler.sample(&mut self.written_bytes);
        self.speed = speed;
        self.per_runner_speed = if runners == 0 {
            0.0
        } else {
            speed / runners as f64
        };
        (speed, write_speed)
    }
}

enum LoopEvent {
    Cancelled,
    Deadline,
    WriterCompleted(Option<Vec<WriteCompletion>>),
    StrategyTimer,
    ThroughputTimer,
    Runner(RunnerTick),
}

fn check_completions(completions: Vec<WriteCompletion>) -> Result<usize> {
    let mut written = 0usize;
    for c in &completions {
        match &c.result {
            Ok(()) => {
                written += (c.range.end - c.range.start) as usize;
            }
            Err(e) => {
                return Err(TaskInstanceError::new_failed(TaskError::Other {
                    message: format!("write failed at {:?}: {e}", c.range),
                }));
            }
        }
    }
    Ok(written)
}

/// The event loop of one concurrent download attempt.
struct DownloadLoop {
    spawner: RunnerSpawner,
    manager: RunnerManager,
    strategy: StrategyControl,
    policy: FailurePolicy,
    max_concurrency: usize,
    throughput: Throughput,
    writer: PendingWriter<FileWriter>,
    event_tx: async_channel::Sender<TaskEvent>,
    throughput_timer: TimerImpl,
    strategy_timer: TimerImpl,
    /// Woken at the no-progress deadline; re-armed lazily when progress moved it.
    deadline_timer: async_io::Timer,
}

impl DownloadLoop {
    fn new(
        total: u64,
        spawner: RunnerSpawner,
        file_writer: &FileWriter,
        event_tx: async_channel::Sender<TaskEvent>,
        options: &TaskOptions,
    ) -> Self {
        let max_concurrency = std::thread::available_parallelism()
            .map(|t| t.get())
            .unwrap_or(DEFAULT_MAX_CONCURRENCY);
        let manager = RunnerManager::new(total, max_concurrency);
        let strategy = StrategyControl::new(max_concurrency);
        let policy = FailurePolicy::new(options, Instant::now());
        let writer = PendingWriter::new(
            Arc::new(file_writer.clone()),
            spawner.rt.clone(),
            max_concurrency,
        );
        let throughput_timer = spawner.rt.create_delayed_timer(DEFAULT_SAMPLE_INTERVAL);
        let strategy_timer = spawner
            .rt
            .create_delayed_timer(DEFAULT_STRATEGY_TICK_INTERVAL);
        let deadline_timer = async_io::Timer::at(policy.deadline());
        Self {
            spawner,
            manager,
            strategy,
            policy,
            max_concurrency,
            throughput: Throughput::new(),
            writer,
            event_tx,
            throughput_timer,
            strategy_timer,
            deadline_timer,
        }
    }

    async fn run(&mut self, cancel_token: &CancellationToken) -> Result<()> {
        loop {
            let completions = self.writer.try_tick();
            self.record_completions(completions)?;

            match self.next_event(cancel_token).await {
                LoopEvent::Cancelled => {
                    return Err(TaskInstanceError::new_failed(TaskError::Cancelled));
                }
                LoopEvent::Deadline => self.on_deadline()?,
                LoopEvent::WriterCompleted(Some(completions)) => {
                    self.record_completions(completions)?;
                }
                LoopEvent::WriterCompleted(None) => {
                    return Err(TaskInstanceError::new_failed(TaskError::Other {
                        message: "writer completion channel closed unexpectedly".into(),
                    }));
                }
                LoopEvent::StrategyTimer => self.on_strategy_tick(),
                LoopEvent::ThroughputTimer => self.on_throughput_tick(),
                LoopEvent::Runner(tick) => {
                    if self.on_runner(tick)?.is_break() {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Priority: cancel > deadline > writer > strategy > throughput > runner.
    async fn next_event(&mut self, cancel_token: &CancellationToken) -> LoopEvent {
        let Self {
            manager,
            writer,
            throughput_timer,
            strategy_timer,
            deadline_timer,
            ..
        } = self;
        let can_write = writer.can_write();
        let writer_fut = if writer.is_idle() {
            Either::Left(futures::future::pending::<Option<Vec<WriteCompletion>>>())
        } else {
            Either::Right(writer.tick())
        }
        .fuse();
        let runner_fut = if can_write {
            Either::Left(manager.tick())
        } else {
            Either::Right(manager.tick_lifecycle_only())
        }
        .fuse();
        let cancelled = cancel_token.cancelled().fuse();
        let deadline = FutureExt::fuse(deadline_timer);
        let strategy_tick = strategy_timer.tick().fuse();
        let throughput_tick = throughput_timer.tick().fuse();

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
            completions = writer_fut => LoopEvent::WriterCompleted(completions),
            _ = strategy_tick => LoopEvent::StrategyTimer,
            _ = throughput_tick => LoopEvent::ThroughputTimer,
            tick = runner_fut => LoopEvent::Runner(tick),
        }
    }

    fn record_completions(&mut self, completions: Vec<WriteCompletion>) -> Result<()> {
        let written = check_completions(completions)?;
        self.throughput.written_bytes += written;
        self.policy.on_write_ok(written);
        Ok(())
    }

    fn on_deadline(&mut self) -> Result<()> {
        if let Some(e) = self.policy.check(Instant::now()) {
            return Err(TaskInstanceError::new_failed(e));
        }
        self.deadline_timer.set_at(self.policy.deadline());
        Ok(())
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    fn on_strategy_tick(&mut self) {
        let Some((strategy_name, actions)) = self.strategy.execute(
            self.max_concurrency,
            self.throughput.speed,
            self.throughput.per_runner_speed,
            &mut self.manager,
        ) else {
            return;
        };
        for action in actions {
            trace!("[TASK] applying strategy: {strategy_name}, action: {action:?}");
            self.apply(action);
        }
    }

    /// Sample the throughput and publish a periodic progress event; the event
    /// is dropped when the channel is full.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    fn on_throughput_tick(&mut self) {
        let meters = self.manager.take_meters_snapshot();
        let (speed, write_speed) = self.throughput.sample(meters);
        let downloaded_chunks = self.manager.get_downloaded_ranges();
        let downloaded = downloaded_chunks
            .iter()
            .map(|r| r.end - r.start)
            .sum::<u64>();
        let _ = self
            .event_tx
            .try_send(TaskEvent::Downloading(ProgressWithSpeed::new(
                Progress {
                    total: Some(self.manager.total()),
                    downloaded,
                    downloaded_chunks,
                },
                speed,
                write_speed,
            )));
    }

    fn on_runner(&mut self, tick: RunnerTick) -> Result<ControlFlow<()>> {
        if let Some(chunk) = tick.downloaded {
            self.writer
                .write_range(chunk.range, chunk.bytes)
                .map_err(|e: WriterFullError| {
                    TaskInstanceError::new_failed(TaskError::Other {
                        message: format!(
                            "writer full (invariant violation): {:?}",
                            e.pending_write.range
                        ),
                    })
                })?;
            self.policy
                .on_bytes_accepted(self.manager.get_total_downloaded(), Instant::now());
        }
        if let Some(failure) = tick.failure
            && let Some(e) = self
                .policy
                .on_failure(&failure, self.manager.allocated_runner_count())
        {
            return Err(TaskInstanceError::new_failed(e));
        }
        Ok(if tick.state == TaskState::Finished {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        })
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    fn apply(&mut self, action: StrategyAction) {
        match action {
            // Split current all task into two separate tasks
            StrategyAction::SplitAllTask(planned_chunk_size) => {
                trace!("[TASK] Strategy: split all task");
                let states = self.manager.get_incomplete_states(Some(planned_chunk_size));
                for (runner_id, incomplete_range) in states {
                    trace!("split task: {runner_id}, {incomplete_range:?}");
                    self.split(runner_id, incomplete_range);
                }
            }
            // Split the given task into two separate tasks
            StrategyAction::SplitGivenTask(task_id) => {
                trace!("[TASK] Strategy: split given task: {task_id}");
                let incomplete_range = self
                    .manager
                    .get_runner_state(task_id)
                    .map(|s| s.incomplete_range())
                    .expect("task id not found");
                self.split(task_id, incomplete_range);
            }
            // Change the max concurrency
            StrategyAction::ChangeMaxConcurrency(new_max_concurrency) => {
                trace!("[TASK] Strategy: change max concurrency: {new_max_concurrency}");
                self.max_concurrency = new_max_concurrency;
            }
            // Create background runners for specific range
            StrategyAction::CreateTask(range) => {
                trace!("[TASK] Strategy: create task: {range:?}");
                self.spawn_runner(range);
            }
        }
    }

    /// Split the given runner's incomplete range in two, spawning a runner for the second half.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, fields(runner_id, range))
    )]
    fn split(&mut self, runner_id: RunnerId, incomplete_range: Range<u64>) {
        let half_size = (incomplete_range.end - incomplete_range.start) / 2;
        if let Err(e) = self.manager.resize_runner(runner_id, half_size) {
            trace!("[TASK] skip splitting runner {runner_id}: {e}");
            return;
        }
        self.spawn_runner(incomplete_range.start + half_size..incomplete_range.end);
    }

    fn spawn_runner(&mut self, range: Range<u64>) {
        let spawner = &self.spawner;
        self.manager
            .allocate_pending_runner_with_chunk(range.clone(), |runner_id, control_rx| {
                spawner.spawn(range, runner_id, control_rx)
            })
            .expect("failed to allocate runner with chunk");
    }

    /// Join every spawned worker and flush the pending writes, recording the
    /// final progress. Never short-circuits; the first error takes precedence.
    async fn finish(self, transfer_result: Result<()>, progress: &mut Progress) -> Result<()> {
        let Self {
            spawner,
            manager,
            writer,
            ..
        } = self;
        spawner.cancel_token.cancel();
        progress.downloaded = manager.get_total_downloaded();
        progress.downloaded_chunks = manager.get_downloaded_ranges();
        // Releasing the receivers unblocks runners pushing lifecycle or data frames.
        drop(manager);
        spawner.wg.wait().await;
        let flush_result = check_completions(writer.finish().await).map(drop);
        transfer_result.and(flush_result)
    }
}

impl Transfer for ConcurrentTaskInner {
    async fn prepare(&mut self) -> Result<()> {
        self.retrieve_meta().await
    }

    fn progress(&self) -> Progress {
        self.progress.clone()
    }

    fn final_progress(&self) -> Progress {
        self.progress.clone()
    }

    async fn download(&mut self, cancel_token: &CancellationToken) -> Result<()> {
        ConcurrentTaskInner::download(self, cancel_token).await
    }
}

#[cfg(test)]
mod tests {
    use std::{
        pin::pin,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll},
        time::Duration,
    };

    use bolt_load_tests::adapter::simple::{SimpleTestAdapter, calculate_blake3};
    use futures::{
        StreamExt,
        future::FutureObj,
        task::{ArcWake, Spawn, SpawnError},
    };
    use pretty_assertions::assert_eq;
    use runner_manager::pending::PendingRunnerError;
    use smol_cancellation_token::CancellationToken;

    use super::*;
    use crate::{
        adapter::{BoltLoadAdapter, DynBoltLoadAdapter},
        runner::{LifecycleEvent, StoppedReason},
        runtime::{
            DowncastLocalRuntime, DynTimer, ThreadedRuntime, ThreadedRuntimeImpl, Timer,
            TimerBuilder, TimerImpl,
        },
        task::ControlEvent,
    };

    #[derive(Clone, Debug)]
    struct FailOnSpawnRuntime {
        fail_on: usize,
        calls: Arc<AtomicUsize>,
    }

    impl FailOnSpawnRuntime {
        fn new(fail_on: usize) -> Self {
            Self {
                fail_on,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl Spawn for FailOnSpawnRuntime {
        fn spawn_obj(&self, future: FutureObj<'static, ()>) -> Result<(), SpawnError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == self.fail_on {
                return Err(SpawnError::shutdown());
            }

            tokio::spawn(future);
            Ok(())
        }
    }

    impl DowncastLocalRuntime for FailOnSpawnRuntime {}

    impl TimerBuilder for FailOnSpawnRuntime {
        fn create_delayed_timer(&self, _duration: Duration) -> TimerImpl {
            TimerImpl::Custom(DynTimer::new_box(NoopTimer))
        }
    }

    impl ThreadedRuntime for FailOnSpawnRuntime {}

    struct NoopTimer;

    impl Timer for NoopTimer {
        async fn tick(&mut self) {}
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_success() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter =
            DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(10240).with_range_support(true));
        let range = 1000u64..3000u64;
        let runner_id = 1;
        let (_control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();
        let wg = WaitGroup::new();
        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter.clone(),
            wg.clone(),
            cancel_token,
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range.clone(), runner_id, control_rx);
        let result = receiver.await.expect("receiver closed");
        assert!(result.is_ok());
        let output = result.unwrap();
        let data_rx = output.data_rx;
        let lifecycle_rx = output.lifecycle_rx;

        // Verify that Started message is received
        let mut lifecycle_rx = pin!(lifecycle_rx);
        let msg = lifecycle_rx.next().await.unwrap();
        assert!(matches!(msg, LifecycleEvent::Started));

        // Verify that downloaded data is received via data channel
        let mut total_downloaded = 0;
        let mut downloaded_data = Vec::new();

        let mut data_rx = pin!(data_rx);
        loop {
            tokio::select! {
                frame = data_rx.next() => {
                    match frame {
                        Some(frame) => {
                            total_downloaded += frame.data.len();
                            downloaded_data.extend_from_slice(&frame.data);
                        }
                        None => break,
                    }
                }
                lifecycle = lifecycle_rx.next() => {
                    match lifecycle {
                        Some(LifecycleEvent::Stopped(StoppedReason::Finished)) => {
                            break;
                        }
                        Some(LifecycleEvent::Stopped(StoppedReason::Failed(e))) => {
                            panic!("Runner failed: {e:?}");
                        }
                        _ => {}
                    }
                }
            }
        }

        wg.wait().await;

        // Verify the amount of downloaded data
        assert_eq!(total_downloaded, (range.end - range.start) as usize);
        assert_eq!(downloaded_data.len(), (range.end - range.start) as usize);

        // Verify data determinism - create the same adapter to get data from the same range for comparison
        let reference_adapter = SimpleTestAdapter::new(10240).with_range_support(true);
        let reference_stream =
            BoltLoadAdapter::range_stream(&reference_adapter, range.start, range.end)
                .await
                .unwrap();
        let mut reference_data = Vec::new();
        let mut reference_stream = std::pin::pin!(reference_stream);
        while let Some(chunk_result) = reference_stream.next().await {
            let chunk = chunk_result.unwrap();
            reference_data.extend_from_slice(&chunk);
        }

        assert_eq!(downloaded_data, reference_data);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_with_adapter_failure() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter = DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(1024).with_failure(true));
        let range = 0u64..500u64;
        let runner_id = 2;
        let (_control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter,
            wg.clone(),
            cancel_token,
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range, runner_id, control_rx);
        let result = receiver.await.expect("receiver closed");
        wg.wait().await;

        // Connection should fail because range_stream fails
        assert!(result.is_err());
        match result {
            Ok(_) => panic!("Expected Connection error, got channels"),
            Err(PendingRunnerError::Connection { .. }) => {}
            Err(e) => panic!("Expected Connection error, got: {:?}", e),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_reports_connector_spawn_error() {
        let rt = ThreadedRuntimeImpl::new_other_rt(FailOnSpawnRuntime::new(0));
        let adapter =
            DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(1024).with_range_support(true));
        let range = 0u64..500u64;
        let runner_id = 8;
        let (_control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();
        let wg = WaitGroup::new();

        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter,
            wg.clone(),
            cancel_token,
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range, runner_id, control_rx);
        let result = receiver.await.expect("receiver closed");

        match result {
            Err(PendingRunnerError::Spawn { message }) => assert!(!message.is_empty()),
            other => panic!("expected Spawn error, got: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_spawns_once() {
        // A second spawn would fail; connecting and running share one task.
        let runtime = FailOnSpawnRuntime::new(1);
        let spawns = runtime.calls.clone();
        let rt = ThreadedRuntimeImpl::new_other_rt(runtime);
        let adapter =
            DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(1024).with_range_support(true));
        let (_control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let wg = WaitGroup::new();

        let receiver = RunnerSpawner::new(
            rt,
            adapter,
            wg.clone(),
            CancellationToken::new(),
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(0..500, 9, control_rx);
        let output = tokio::time::timeout(Duration::from_secs(1), receiver)
            .await
            .expect("timed out waiting for runner channels")
            .expect("receiver closed")
            .expect("runner should start");

        let mut data_rx = pin!(output.data_rx);
        let mut lifecycle_rx = pin!(output.lifecycle_rx);
        let mut received = 0;
        while let Some(frame) = data_rx.next().await {
            received += frame.data.len();
        }
        let mut finished = false;
        while let Some(event) = lifecycle_rx.next().await {
            finished |= event.is_finished();
        }
        wg.wait().await;

        assert!(finished);
        assert_eq!(received, 500);
        assert_eq!(spawns.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_with_cancellation() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter =
            DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(10240).with_range_support(true));
        let range = 0u64..5000u64;
        let runner_id = 3;
        let (_control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter,
            wg.clone(),
            cancel_token.clone(),
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range, runner_id, control_rx);
        let result = receiver.await.expect("receiver closed");
        assert!(result.is_ok());
        let output = result.unwrap();
        let _data_rx = output.data_rx;
        let lifecycle_rx = output.lifecycle_rx;

        // Wait for start message
        let mut lifecycle_rx = pin!(lifecycle_rx);
        let msg = lifecycle_rx.next().await.unwrap();
        assert!(matches!(msg, LifecycleEvent::Started));

        // Cancel the task
        cancel_token.cancel();

        // Wait for cancellation message
        while let Some(msg) = lifecycle_rx.next().await {
            if let LifecycleEvent::Stopped(StoppedReason::Failed(e)) = msg
                && e.is_cancelled()
            {
                break;
            }
        }

        wg.wait().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_small_range_runner_with_control_messages() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter =
            DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(10240).with_range_support(true));
        let old_total = 10240u64;
        let range = 0u64..old_total;
        let runner_id = 4;
        let (control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        // Send control message before creating the runner
        let new_total = 1000u64;
        control_tx
            .send(ControlEvent::LimitTotal(new_total))
            .await
            .unwrap();

        let wg = WaitGroup::new();
        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter,
            wg.clone(),
            cancel_token,
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range.clone(), runner_id, control_rx);
        let result = receiver.await.expect("receiver closed");
        assert!(result.is_ok());
        let output = result.unwrap();
        let data_rx = output.data_rx;
        let lifecycle_rx = output.lifecycle_rx;

        // Wait for start message
        let mut lifecycle_rx = pin!(lifecycle_rx);
        let msg = lifecycle_rx.next().await.unwrap();
        assert!(matches!(msg, LifecycleEvent::Started));

        // Collect downloaded data
        let mut total_downloaded = 0;
        let mut data_rx = pin!(data_rx);
        loop {
            tokio::select! {
                frame = data_rx.next() => {
                    match frame {
                        Some(frame) => {
                            total_downloaded += frame.data.len();
                        }
                        None => break,
                    }
                }
                lifecycle = lifecycle_rx.next() => {
                    if let Some(LifecycleEvent::Stopped(StoppedReason::Finished)) = lifecycle {
                        break;
                    }
                }
            }
        }

        wg.wait().await;

        // Verify that downloaded amount is the adjusted size
        if total_downloaded != new_total as usize {
            error!("total_downloaded: {total_downloaded}, new_total: {new_total}");
        }
        assert!(total_downloaded >= new_total as usize);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_edge_ranges() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let content_size = 1000;
        let adapter = DynBoltLoadAdapter::new_arc(
            SimpleTestAdapter::new(content_size).with_range_support(true),
        );

        // Test boundary range: start from end of file
        let range = 900u64..1000u64;
        let runner_id = 5;
        let (_control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter.clone(),
            wg.clone(),
            cancel_token,
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range.clone(), runner_id, control_rx);
        let result = receiver.await.expect("receiver closed");
        assert!(result.is_ok());
        let output = result.unwrap();
        let data_rx = output.data_rx;
        let lifecycle_rx = output.lifecycle_rx;

        // Wait for start
        let mut lifecycle_rx = pin!(lifecycle_rx);
        let msg = lifecycle_rx.next().await.unwrap();
        assert!(matches!(msg, LifecycleEvent::Started));

        // Collect all data - drain data_rx first, then check lifecycle
        let mut downloaded_data = Vec::new();
        let mut data_rx = pin!(data_rx);
        let mut finished = false;
        let mut data_done = false;

        loop {
            tokio::select! {
                biased;
                // Prefer data frames over lifecycle events to avoid losing data
                frame = data_rx.next(), if !data_done => {
                    match frame {
                        Some(frame) => {
                            downloaded_data.extend_from_slice(&frame.data);
                        }
                        None => {
                            data_done = true;
                            if finished {
                                break;
                            }
                        }
                    }
                }
                lifecycle = lifecycle_rx.next() => {
                    match lifecycle {
                        Some(LifecycleEvent::Stopped(StoppedReason::Finished)) => {
                            finished = true;
                            if data_done {
                                break;
                            }
                        }
                        None => break,
                        _ => {}
                    }
                }
            }
        }

        wg.wait().await;
        assert_eq!(downloaded_data.len(), (range.end - range.start) as usize);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_zero_length_range() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter =
            DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(1000).with_range_support(true));

        // Test zero-length range
        let range = 500u64..500u64;
        let runner_id = 6;
        let (_control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter,
            wg.clone(),
            cancel_token,
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range, runner_id, control_rx);
        let result = receiver.await.expect("receiver closed");
        assert!(result.is_ok());
        let output = result.unwrap();
        let _data_rx = output.data_rx;
        let lifecycle_rx = output.lifecycle_rx;

        wg.wait().await;

        // Wait for start
        let mut lifecycle_rx = pin!(lifecycle_rx);
        let msg = lifecycle_rx.next().await.unwrap();
        assert!(matches!(msg, LifecycleEvent::Started));

        // Should finish immediately without downloading any data
        // Zero-length range may result in Empty error
        let msg = lifecycle_rx.next().await;
        match msg {
            Some(LifecycleEvent::Stopped(StoppedReason::Finished)) => {}
            Some(LifecycleEvent::Stopped(StoppedReason::Failed(e))) if e.is_empty() => {}
            other => panic!("Expected immediate finish for zero-length range, got: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_multiple_runners() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter =
            DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(4000).with_range_support(true));

        // Create multiple concurrent range runners
        let ranges = vec![
            (1, 0u64..1000u64),
            (2, 1000u64..2000u64),
            (3, 2000u64..3000u64),
            (4, 3000u64..4000u64),
        ];

        let mut handles = Vec::new();

        for (runner_id, range) in ranges {
            let rt_clone = rt.clone();
            let adapter_clone = adapter.clone();
            let (control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
            let cancel_token = CancellationToken::new();

            let handle = tokio::spawn(async move {
                let wg = WaitGroup::new();
                let receiver = RunnerSpawner::new(
                    rt_clone.clone(),
                    adapter_clone,
                    wg.clone(),
                    cancel_token,
                    crate::runner::DEFAULT_IDLE_TIMEOUT,
                )
                .spawn(range.clone(), runner_id, control_rx);
                let result = receiver.await.expect("receiver closed");
                let output = result.expect("connection failed");
                let data_rx = output.data_rx;
                let lifecycle_rx = output.lifecycle_rx;

                // Keep the control channel sender alive
                let _control_tx = control_tx;

                // Wait for start
                let mut lifecycle_rx = pin!(lifecycle_rx);
                let msg = lifecycle_rx.next().await.unwrap();
                assert!(matches!(msg, LifecycleEvent::Started));

                // Collect all data
                let mut downloaded_size = 0;
                let mut finished = false;
                let mut data_done = false;

                let mut data_rx = pin!(data_rx);
                loop {
                    tokio::select! {
                        biased;
                        // Prefer data frames to avoid losing data
                        frame = data_rx.next(), if !data_done => {
                            match frame {
                                Some(frame) => {
                                    downloaded_size += frame.data.len();
                                }
                                None => {
                                    data_done = true;
                                    if finished {
                                        break;
                                    }
                                }
                            }
                        }
                        lifecycle = lifecycle_rx.next() => {
                            match lifecycle {
                                Some(LifecycleEvent::Stopped(StoppedReason::Finished)) => {
                                    finished = true;
                                    if data_done {
                                        break;
                                    }
                                }
                                Some(LifecycleEvent::Stopped(StoppedReason::Failed(kind))) => {
                                    panic!("Runner {runner_id} failed with error: {kind:?}");
                                }
                                None => break,
                                _ => {}
                            }
                        }
                    }
                }

                wg.wait().await;

                // Ensure the runner completed correctly
                if !finished {
                    panic!("Runner {runner_id} did not finish properly");
                }

                (runner_id, downloaded_size, range.end - range.start)
            });

            handles.push(handle);
        }

        // Wait for all tasks to complete
        let results = futures::future::join_all(handles).await;

        for result in results {
            let (runner_id, downloaded_size, expected_size) = result.unwrap();
            info!(
                "Runner {runner_id}: downloaded {downloaded_size} bytes, expected {expected_size} \
                 bytes"
            );
            assert_eq!(downloaded_size, expected_size as usize);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_hash_verification() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter =
            DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(5000).with_range_support(true));
        let range = 1500u64..3500u64;
        let runner_id = 7;
        let (_control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let receiver = RunnerSpawner::new(
            rt.clone(),
            adapter.clone(),
            wg.clone(),
            cancel_token,
            crate::runner::DEFAULT_IDLE_TIMEOUT,
        )
        .spawn(range.clone(), runner_id, control_rx);
        let result = receiver.await.expect("receiver closed");
        assert!(result.is_ok());
        let output = result.unwrap();
        let data_rx = output.data_rx;
        let lifecycle_rx = output.lifecycle_rx;

        // Wait for start message
        let mut lifecycle_rx = pin!(lifecycle_rx);
        let msg = lifecycle_rx.next().await.unwrap();
        assert!(matches!(msg, LifecycleEvent::Started));

        // Collect all downloaded data
        let mut downloaded_data = Vec::new();
        let mut data_rx = pin!(data_rx);
        loop {
            tokio::select! {
                frame = data_rx.next() => {
                    match frame {
                        Some(frame) => {
                            downloaded_data.extend_from_slice(&frame.data);
                        }
                        None => break,
                    }
                }
                lifecycle = lifecycle_rx.next() => {
                    if let Some(LifecycleEvent::Stopped(StoppedReason::Finished)) = lifecycle {
                        break;
                    }
                }
            }
        }

        wg.wait().await;

        // Use the same adapter to directly get data from the same range for comparison
        let reference_adapter = SimpleTestAdapter::new(5000).with_range_support(true);
        let reference_stream =
            BoltLoadAdapter::range_stream(&reference_adapter, range.start, range.end)
                .await
                .unwrap();
        let mut reference_data = Vec::new();
        let mut reference_stream = std::pin::pin!(reference_stream);
        while let Some(chunk_result) = reference_stream.next().await {
            let chunk = chunk_result.unwrap();
            reference_data.extend_from_slice(&chunk);
        }

        // Verify data integrity
        assert_eq!(downloaded_data.len(), (range.end - range.start) as usize);
        assert_eq!(downloaded_data, reference_data);

        // Calculate and compare hash
        let downloaded_hash = calculate_blake3(&downloaded_data);
        let reference_hash = calculate_blake3(&reference_data);
        assert_eq!(downloaded_hash, reference_hash);
    }

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
                TimerKind::Tokio => {
                    TimerImpl::Tokio(crate::runtime::TokioDelayedTimer::new(duration))
                }
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
        match futures::future::select(async_io::Timer::after(Duration::from_secs(10)), pin!(fut))
            .await
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

        /// Run the loop until it returns on its own.
        async fn run_to_end(&mut self) -> Result<()> {
            with_watchdog(self.download.run(&self.cancel)).await
        }

        async fn teardown(self) {
            drop(self.download);
            self.file_writer.finalize().await.unwrap();
        }
    }

    /// Drain the progress events published so far.
    fn progress_events(events: &async_channel::Receiver<TaskEvent>) -> usize {
        std::iter::from_fn(|| events.try_recv().ok())
            .filter(|e| matches!(e, TaskEvent::Downloading(_)))
            .count()
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
        let cancel = harness.cancel.clone();
        let result = {
            let mut run = pin!(harness.download.run(&cancel));
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
                let mut run = pin!(harness.download.run(&cancel));
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
            assert_eq!(progress_events(&harness.events), 1);
            harness.teardown().await;
        });
    }

    #[test]
    fn test_timer_rearms_after_consecutive_ticks() {
        // Scripted: the loop parks between ticks and every permit is one more tick.
        on_each_executor(|| async {
            let mut harness = LoopHarness::scripted(false).await;
            let script = harness.rt.script.clone();
            let events = harness.events.clone();
            let cancel = harness.cancel.clone();
            let wakes = Arc::new(WakeCount::default());
            let waker = futures::task::waker(wakes.clone());
            let mut cx = Context::from_waker(&waker);
            let result = {
                let mut run = pin!(harness.download.run(&cancel));
                assert!(run.as_mut().poll(&mut cx).is_pending());
                for tick in 1..=3 {
                    let seen = wakes.get();
                    script.permit("throughput");
                    wakes.wait_past(seen);
                    assert!(run.as_mut().poll(&mut cx).is_pending());
                    assert_eq!(progress_events(&events), 1, "tick {tick}");
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
            let mut harness = LoopHarness::new(timers, TaskOptions::default()).await;
            let events = harness.events.clone();
            let cancel = harness.cancel.clone();
            drop(std::thread::spawn(move || {
                let mut ticks = 0;
                while ticks < 3 {
                    match events.recv_blocking() {
                        Ok(TaskEvent::Downloading(_)) => ticks += 1,
                        Ok(_) => {}
                        Err(_) => return,
                    }
                }
                cancel.cancel();
            }));
            let started = Instant::now();
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
            harness.download.policy.on_bytes_accepted(1, Instant::now());
            std::thread::sleep(TIMEOUT / 4);
            let last_progress = Instant::now();
            harness.download.policy.on_bytes_accepted(2, last_progress);
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

    #[test]
    fn test_cancel_after_registration_wakes_loop() {
        on_each_executor(|| async {
            let mut harness = LoopHarness::scripted(false).await;
            let cancel = harness.cancel.clone();
            let wakes = Arc::new(WakeCount::default());
            let waker = futures::task::waker(wakes.clone());
            let mut cx = Context::from_waker(&waker);
            let result = {
                let mut run = pin!(harness.download.run(&cancel));
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
}
