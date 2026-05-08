use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
    ops::Range,
    path::PathBuf,
    sync::Arc,
};

use async_waitgroup::WaitGroup;
use bolt_load_core::adapter::AdapterError;
use bolt_load_utils::telemetry::*;
use futures::{FutureExt, future::Either, task::SpawnExt};
use runner_manager::TaskState;
use smol_cancellation_token::CancellationToken;
use statig::prelude::*;

use super::{Result, TaskInstance};
use crate::{
    DOWNLOADING_TMP_EXTENSION,
    adapter::{AnyAdapter, UnretryableError},
    runner::{RunnerConnector, RunnerConnectorError, TaskError, TaskRunner},
    runtime::{
        LocalRuntimeBuilderImpl, ThreadedRuntimeExt, ThreadedRuntimeImpl, Timer, TimerBuilder,
    },
    task::{
        Progress, RunnerId,
        instance::{
            ProgressWithSpeed, RunningPayload, TaskControl, TaskEvent, TaskInstanceError,
            sampler::{DEFAULT_SAMPLE_INTERVAL, SpeedSampler},
        },
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
const DEFAULT_CONTROL_CHANNEL_CAPACITY: usize = 8;

pub struct ConcurrentTask {
    threaded_rt: ThreadedRuntimeImpl,
    local_runtime_builder: Option<LocalRuntimeBuilderImpl>,
    task: Option<TaskControl>,
    progress: Progress,
}

impl ConcurrentTask {
    pub fn new(
        threaded_rt: ThreadedRuntimeImpl,
        local_runtime_builder: Option<LocalRuntimeBuilderImpl>,
    ) -> Self {
        Self {
            threaded_rt,
            local_runtime_builder,
            task: None,
            progress: Progress::default(),
        }
    }
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
        let token = cancel_token.clone();
        let threaded_rt = self.threaded_rt.clone();
        let local_rt_builder = self.local_runtime_builder.clone();
        // TODO: replace the local task executor with global runtime executor while `-Zhigher-ranked-assumptions` is stable
        // Ref:  https://github.com/rust-lang/rust/issues/100013.
        let task = ::blocking::unblock(move || {
            // TODO: handle the error
            let local_rt = threaded_rt
                .downcast_local(local_rt_builder)
                .expect("failed to downcast the runtime");
            let task = async move {
                let mut context = Context::default();
                let mut state_machine = ConcurrentTaskInner::new(path, event_tx, threaded_rt)
                    .uninitialized_state_machine()
                    .init_with_context(&mut context)
                    .await;

                state_machine
                    .handle_with_context(
                        &Event::Run(RunningPayload {
                            adapter,
                            cancel_token,
                        }),
                        &mut context,
                    )
                    .await;

                while context.poll.pop_front().is_some() {
                    state_machine
                        .handle_with_context(&Event::Step, &mut context)
                        .await;
                }

                debug_assert!(
                    matches!(state_machine.state(), State::Stopped { .. }),
                    "the task should be stopped after the state machine is finished"
                );
            };
            #[cfg(feature = "tracing")]
            let task = tracing::Instrument::instrument(
                task,
                tracing::trace_span!(
                    parent: None,
                    "ConcurrentTask::background_task",
                ),
            );
            local_rt.block_on(Box::pin(task))
        });
        self.task = Some(TaskControl::new(token, task));
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(mut task) = self.task.take() {
            task.stop().await;
        }
        Ok(())
    }

    async fn wait(&mut self) -> Result<()> {
        if let Some(mut task) = self.task.take() {
            task.wait().await;
        }
        Ok(())
    }
}

pub struct ConcurrentTaskInner {
    rt: ThreadedRuntimeImpl,
    path: PathBuf,
    adapter: Option<Arc<AnyAdapter>>,
    /// The initial progress of the task, for task resume
    progress: Progress,
    event_tx: async_channel::Sender<TaskEvent>,
}

#[derive(Default)]
pub struct Context {
    poll: VecDeque<()>,
}

pub enum Event {
    Run(RunningPayload),
    Step,
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
enum RunnerStatus {
    Running,
    Finished,
}

impl ConcurrentTaskInner {
    fn new(
        path: PathBuf,
        event_tx: async_channel::Sender<TaskEvent>,
        rt: ThreadedRuntimeImpl,
    ) -> Self {
        Self {
            rt,
            path,
            adapter: None,
            progress: Progress::default(),
            event_tx,
        }
    }
    /// Retrieve the meta data of the file
    async fn retrieve_meta(&mut self) -> Result<()> {
        // TODO: use backon to retry
        let adapter = self.adapter.as_ref().ok_or_else(|| {
            TaskInstanceError::new_failed(TaskError::Other {
                message: "Adapter not set before meta retrieval".to_string(),
            })
        })?;
        let meta = adapter
            .retrieve_meta()
            .await
            .map_err(TaskInstanceError::RetrieveMetaFailed)?;
        let total = if meta.content_size == 0 {
            return Err(TaskInstanceError::RetrieveMetaFailed(
                AdapterError::Unretryable {
                    source: UnretryableError::Whatever {
                        message: "content size is 0; concurrent task does not support 0-size file"
                            .to_string(),
                        source: None,
                    },
                },
            ));
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
    async fn create_file_writer(&self) -> Result<(PathBuf, FileWriter)> {
        let tmp_path = if self.path.ends_with(DOWNLOADING_TMP_EXTENSION) {
            Cow::Borrowed(&self.path)
        } else {
            let parent_dir = self.path.parent().unwrap();
            let file_name = self.path.file_name().unwrap();
            Cow::Owned(parent_dir.join(format!(
                "{}{}",
                file_name.to_string_lossy(),
                DOWNLOADING_TMP_EXTENSION
            )))
        };
        let total_size = self.progress.total.ok_or_else(|| {
            TaskInstanceError::new_failed(TaskError::Other {
                message: "File writer creation called before meta retrieval".to_string(),
            })
        })?;
        let file_writer = FileWriter::new(&tmp_path, total_size).await.map_err(|e| {
            TaskInstanceError::new_failed(TaskError::Other {
                message: e.to_string(),
            })
        })?;
        Ok((tmp_path.into_owned(), file_writer))
    }

    /// Split the given task into two separate tasks
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, fields(runner_id, range))
    )]
    #[allow(clippy::too_many_arguments)]
    fn split_task(
        runner_manager: &mut RunnerManager,

        threaded_rt: &ThreadedRuntimeImpl,
        wg: &WaitGroup,
        runners_cancel_token: &CancellationToken,
        incomplete_range: Range<u64>,
        adapter: Arc<AnyAdapter>,
        runner_id: RunnerId,
    ) {
        let half_size = (incomplete_range.end - incomplete_range.start) / 2;

        runner_manager.resize_runner(runner_id, half_size);

        // Create a new runner for the remaining range

        let next_range = incomplete_range.start + half_size..incomplete_range.end;
        runner_manager
            .allocate_pending_runner_with_chunk(next_range.clone(), |runner_id, control_rx| {
                Self::create_background_range_runner(
                    threaded_rt,
                    wg,
                    next_range,
                    adapter.clone(),
                    control_rx,
                    runner_id,
                    runners_cancel_token.clone(),
                )
            })
            .expect("failed to allocate runner with chunk");
    }

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, fields(runner_id, range))
    )]
    fn create_background_range_runner(
        threaded_rt: &ThreadedRuntimeImpl,
        wg: &WaitGroup,
        range: Range<u64>,
        adapter: Arc<AnyAdapter>,
        control_rx: ControlReceiver,
        runner_id: RunnerId,
        cancel_token: CancellationToken,
    ) -> oneshot::Receiver<Result<runner_manager::RunnerBuilderOutput, RunnerConnectorError>> {
        let (tx, rx) = oneshot::channel();
        let (start, end) = (range.start, range.end);
        let wg = wg.clone();
        let rt = threaded_rt.clone();
        let connector_task = async move {
            let wg = wg;
            trace!(
                "[TASK] create background range runner connector: id: {runner_id}, range: \
                 {range:?}"
            );
            let builder = TaskRunner::builder()
                .total(end - start)
                .runner_id(runner_id)
                .control_signal(control_rx)
                .cancel_token(cancel_token.clone());
            let connector = RunnerConnector::new(
                async move { adapter.range_stream(start, end).await }.boxed(),
                builder,
            );
            match connector.connect().await {
                Ok((mut runner, lifecycle_rx, data_rx)) => {
                    if let Err(e) = tx.send(Ok(runner_manager::RunnerBuilderOutput {
                        lifecycle_rx,
                        data_rx,
                    })) {
                        error!("failed to send runner channels: {e:?}");
                        return;
                    }
                    let wg = wg.clone();
                    let runner_task = async move {
                        let _wg = wg;
                        runner.run().await;
                    };

                    #[cfg(feature = "tracing")]
                    let runner_task = tracing::Instrument::instrument(
                        runner_task,
                        tracing::trace_span!(
                            parent: tracing::Span::current(),
                            "background_task::runner",
                        ),
                    );
                    rt.spawn(runner_task).expect("should never spawn failed");
                }
                Err(e) => {
                    error!("failed to create background range runner connector: {e:?}");
                    if let Err(e) = tx.send(Err(e)) {
                        error!("failed to send error to channel: {e:?}");
                    }
                }
            }
        };
        #[cfg(feature = "tracing")]
        let connector_task = tracing::Instrument::instrument(
            connector_task,
            tracing::trace_span!(
                parent: None,
                "background_task::runner_connector",
                runner_id = runner_id,
                range = ?start..end,
            ),
        );
        threaded_rt
            .spawn(connector_task)
            .expect("should never spawn failed");
        rx
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    #[allow(clippy::too_many_arguments)]
    fn throughout_meter_tick(
        rt: &ThreadedRuntimeImpl,
        wg: &WaitGroup,
        runner_manager: &RunnerManager,

        meters: &mut HashMap<RunnerId, usize>,
        sampler: &mut SpeedSampler,
        current_speed: &mut f64,
        per_runner_avg_speed: &mut f64,
        write_bytes_meter: &mut usize,
        write_sampler: &mut SpeedSampler,

        event_tx: &async_channel::Sender<TaskEvent>,
    ) {
        let count = meters.len();
        let mut total_bytes = meters.values_mut().map(std::mem::take).sum::<usize>();
        let (_, ema_speed) = sampler.sample(&mut total_bytes);
        let (_, write_ema_speed) = write_sampler.sample(write_bytes_meter);
        // debug!("ema speed {ema_speed} count {count}");
        *current_speed = ema_speed;
        *per_runner_avg_speed = if count == 0 {
            0.0
        } else {
            ema_speed / count as f64
        };

        let event_tx = event_tx.clone();
        let total = runner_manager.total();
        let downloaded_chunks = runner_manager.get_downloaded_ranges();
        let downloaded = downloaded_chunks
            .iter()
            .map(|r| r.end - r.start)
            .sum::<u64>();
        let wg = wg.clone();
        let _ = rt.spawn(async move {
            let _wg = wg;
            if let Err(e) = event_tx
                .send(TaskEvent::Downloading(ProgressWithSpeed::new(
                    Progress {
                        total: Some(total),
                        downloaded,
                        downloaded_chunks,
                    },
                    ema_speed,
                    write_ema_speed,
                )))
                .await
            {
                error!("failed to send downloading event: {e:?}");
            }
        });
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    #[inline]
    fn handle_strategy_action(
        runner_manager: &mut RunnerManager,
        max_concurrency: &mut usize,

        action: StrategyAction,

        threaded_rt: &ThreadedRuntimeImpl,
        wg: &WaitGroup,
        runners_cancel_token: &CancellationToken,
        adapter: &Arc<AnyAdapter>,
    ) -> Result<()> {
        match action {
            // Split current all task into two separate tasks
            StrategyAction::SplitAllTask(planned_chunk_size) => {
                trace!("[TASK] Strategy: split all task");
                let states = runner_manager.get_incomplete_states(Some(planned_chunk_size));
                for (runner_id, incomplete_range) in states {
                    trace!("split task: {runner_id}, {incomplete_range:?}");
                    Self::split_task(
                        runner_manager,
                        threaded_rt,
                        wg,
                        runners_cancel_token,
                        incomplete_range,
                        adapter.clone(),
                        runner_id,
                    );
                }
            }
            // Split the given task into two separate tasks
            StrategyAction::SplitGivenTask(task_id) => {
                trace!("[TASK] Strategy: split given task: {task_id}");
                let incomplete_range = runner_manager
                    .get_runner_state(task_id)
                    .map(|s| s.incomplete_range())
                    .expect("task id not found");
                Self::split_task(
                    runner_manager,
                    threaded_rt,
                    wg,
                    runners_cancel_token,
                    incomplete_range,
                    adapter.clone(),
                    task_id,
                );
            }
            // Change the max concurrency
            StrategyAction::ChangeMaxConcurrency(new_max_concurrency) => {
                trace!("[TASK] Strategy: change max concurrency: {new_max_concurrency}");
                *max_concurrency = new_max_concurrency;
            }
            // Create background runners for specific range
            StrategyAction::CreateTask(range) => {
                trace!("[TASK] Strategy: create task: {range:?}");
                runner_manager
                    .allocate_pending_runner_with_chunk(range.clone(), |runner_id, control_rx| {
                        Self::create_background_range_runner(
                            threaded_rt,
                            wg,
                            range,
                            adapter.clone(),
                            control_rx,
                            runner_id,
                            runners_cancel_token.clone(),
                        )
                    })
                    .expect("failed to allocate runner with chunk");
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    async fn strategy_control_timer_tick(
        runner_manager: &mut RunnerManager,
        strategy_control: &mut StrategyControl,
        max_concurrency: &mut usize,

        threaded_rt: &ThreadedRuntimeImpl,
        adapter: &Arc<AnyAdapter>,
        runners_cancel_token: &CancellationToken,
        wg: &WaitGroup,

        current_speed: f64,
        per_runner_avg_speed: f64,
    ) -> Result<()> {
        if let Some((strategy_name, actions)) = strategy_control.execute(
            *max_concurrency,
            current_speed,
            per_runner_avg_speed,
            runner_manager,
        ) {
            for action in actions {
                trace!("[TASK] applying strategy: {strategy_name}, action: {action:?}");
                Self::handle_strategy_action(
                    runner_manager,
                    max_concurrency,
                    action,
                    threaded_rt,
                    wg,
                    runners_cancel_token,
                    adapter,
                )?;
            }
        }
        Ok(())
    }

    /// Sync the progress of the task
    fn sync_progress(&mut self, runner_manager: &RunnerManager) {
        self.progress.downloaded = runner_manager.get_total_downloaded();
        self.progress.downloaded_chunks = runner_manager.get_downloaded_ranges();
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    async fn download(&mut self, cancel_token: &CancellationToken) -> Result<()> {
        use file_writer::{PendingWriter, WriteCompletion, WriterFullError};
        use runner_manager::RunnerTick;

        let total = self
            .progress
            .total
            .expect("A concurrent task should have a total size");
        let adapter = self.adapter.clone().unwrap();
        let rt = self.rt.clone();
        let event_tx = self.event_tx.clone();
        let wg = WaitGroup::new();
        let runners_cancel_token = cancel_token.child_token();
        let _runner_cancel_guard = runners_cancel_token.clone().drop_guard();

        // The initial max concurrency is the number of available threads
        let initial_max_concurrency = std::thread::available_parallelism()
            .map(|t| t.get())
            .unwrap_or(DEFAULT_MAX_CONCURRENCY);
        // The max concurrency should follows 1 <= max_concurrency <= initial_max_concurrency
        let mut max_concurrency = initial_max_concurrency;
        let mut runner_manager = RunnerManager::new(total, initial_max_concurrency);
        let mut strategy_control = StrategyControl::new(initial_max_concurrency);
        // TODO: make meters removal more efficient
        let mut meters: HashMap<RunnerId, usize> = HashMap::with_capacity(initial_max_concurrency);
        let mut sampler = SpeedSampler::new();
        let mut current_speed = 0.0;
        let mut per_runner_avg_speed = 0.0;
        let mut write_bytes_meter = 0usize;
        let mut write_sampler = SpeedSampler::new();

        let (tmp_path, file_writer) = self.create_file_writer().await?;
        let mut pending_writer = PendingWriter::new(
            Arc::new(file_writer.clone()),
            rt.clone(),
            initial_max_concurrency,
        );

        let mut throughout_meter_timer = rt.create_delayed_timer(DEFAULT_SAMPLE_INTERVAL);
        let mut strategy_timer = rt.create_delayed_timer(DEFAULT_STRATEGY_TICK_INTERVAL);

        fn check_completions(completions: Vec<WriteCompletion>) -> Result<usize> {
            let mut written = 0usize;
            for completion in completions {
                match completion.result {
                    Ok(()) => {
                        written += (completion.range.end - completion.range.start) as usize;
                    }
                    Err(error) => {
                        return Err(TaskInstanceError::new_failed(TaskError::Other {
                            message: format!("write failed at {:?}: {error}", completion.range),
                        }));
                    }
                }
            }
            Ok(written)
        }

        enum LoopEvent {
            WriterCompleted(Option<Vec<WriteCompletion>>),
            StrategyTimer,
            ThroughputTimer,
            Runner(RunnerTick),
        }

        let event_loop_result: Result<()> = async {
            loop {
                write_bytes_meter += check_completions(pending_writer.try_tick())
                    .inspect_err(|_| runners_cancel_token.cancel())?;

                let can_write = pending_writer.can_write();

                let event = {
                    let writer_fut = if pending_writer.is_idle() {
                        Either::Left(futures::future::pending::<Option<Vec<WriteCompletion>>>())
                    } else {
                        Either::Right(pending_writer.tick())
                    }
                    .fuse();

                    let runner_fut = if can_write {
                        Either::Left(runner_manager.tick(&mut meters))
                    } else {
                        Either::Right(runner_manager.tick_lifecycle_only(&mut meters))
                    }
                    .fuse();

                    let strategy_tick = strategy_timer.tick().fuse();
                    let throughput_tick = throughout_meter_timer.tick().fuse();

                    futures::pin_mut!(writer_fut, runner_fut, strategy_tick, throughput_tick);

                    futures::select_biased! {
                        completions = writer_fut => LoopEvent::WriterCompleted(completions),
                        _ = strategy_tick => LoopEvent::StrategyTimer,
                        _ = throughput_tick => LoopEvent::ThroughputTimer,
                        tick = runner_fut => LoopEvent::Runner(tick),
                    }
                };

                match event {
                    LoopEvent::WriterCompleted(Some(completions)) => {
                        write_bytes_meter += check_completions(completions)
                            .inspect_err(|_| runners_cancel_token.cancel())?;
                    }
                    LoopEvent::WriterCompleted(None) => {
                        runners_cancel_token.cancel();
                        return Err(TaskInstanceError::new_failed(TaskError::Other {
                            message: "writer completion channel closed unexpectedly".to_string(),
                        }));
                    }
                    LoopEvent::StrategyTimer => {
                        Self::strategy_control_timer_tick(
                            &mut runner_manager,
                            &mut strategy_control,
                            &mut max_concurrency,
                            &rt,
                            &adapter,
                            &runners_cancel_token,
                            &wg,
                            current_speed,
                            per_runner_avg_speed,
                        )
                        .await
                        .inspect_err(|e| {
                            error!("failed to download strategy timer tick: {e:?}");
                            runners_cancel_token.cancel();
                            self.sync_progress(&runner_manager);
                        })?;
                    }
                    LoopEvent::ThroughputTimer => {
                        Self::throughout_meter_tick(
                            &rt,
                            &wg,
                            &runner_manager,
                            &mut meters,
                            &mut sampler,
                            &mut current_speed,
                            &mut per_runner_avg_speed,
                            &mut write_bytes_meter,
                            &mut write_sampler,
                            &event_tx,
                        );
                    }
                    LoopEvent::Runner(tick) => {
                        if let Some(chunk) = tick.downloaded {
                            pending_writer
                                .write_range(chunk.range, chunk.bytes)
                                .map_err(|error: WriterFullError| {
                                    TaskInstanceError::new_failed(TaskError::Other {
                                        message: format!(
                                            "writer rejected write (invariant violation): {:?}",
                                            error.0.range
                                        ),
                                    })
                                })
                                .inspect_err(|_| runners_cancel_token.cancel())?;
                        }
                        if tick.state == TaskState::Finished {
                            break;
                        }
                    }
                }
            }
            Ok(())
        }
        .await;

        if event_loop_result.is_err() {
            runners_cancel_token.cancel();
        }
        self.sync_progress(&runner_manager);

        wg.wait().await;

        let flush_result = check_completions(pending_writer.flush().await);
        drop(pending_writer);

        event_loop_result?;
        flush_result.map(drop)?;

        // TODO: add a finalizing state?
        file_writer.finalize().await.map_err(|e| {
            TaskInstanceError::new_failed(TaskError::Other {
                message: e.to_string(),
            })
        })?;

        async_fs::rename(&tmp_path, &self.path).await.map_err(|e| {
            TaskInstanceError::new_failed(TaskError::Other {
                message: e.to_string(),
            })
        })?;

        Ok(())
    }

    async fn before_transition(&mut self, _source: &State, target: &State, _context: &mut Context) {
        match target {
            State::Stopped { reason } => {
                trace!("on_transition: enter stopped state, reason: {reason:?}");
                match reason.clone() {
                    Some(Ok(())) => {
                        let _ = self
                            .event_tx
                            .send(TaskEvent::Finished(self.progress.clone()))
                            .await;
                    }
                    Some(Err(e)) => {
                        let _ = self.event_tx.send(TaskEvent::Failed(e)).await;
                    }
                    None => unreachable!(),
                }
            }
            State::Initializing { .. } => {
                trace!("on_transition: enter initializing state");
                let _ = self.event_tx.send(TaskEvent::Initializing).await;
            }
            State::Downloading { .. } => {
                trace!("on_transition: enter downloading state");
                let _ = self
                    .event_tx
                    .send(TaskEvent::Downloading(ProgressWithSpeed::new(
                        self.progress.clone(),
                        0.0,
                        0.0,
                    )))
                    .await;
            }
        }
    }
}

#[state_machine(
    initial = "State::stopped(None)",
    before_transition = "Self::before_transition"
)]
impl ConcurrentTaskInner {
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    #[state]
    #[allow(unused_variables)]
    fn stopped(
        &mut self,
        context: &mut Context,
        reason: &mut Option<Result<()>>,
        event: &Event,
    ) -> Outcome<State> {
        match event {
            Event::Run(payload) => {
                context.poll.push_back(());
                self.adapter = Some(payload.adapter.clone());
                Transition(State::initializing(payload.cancel_token.clone()))
            }
            _ => Super,
        }
    }

    #[superstate]
    #[allow(unused_variables)]
    async fn running(event: &Event) -> Outcome<State> {
        Super
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    #[state(superstate = "running")]
    async fn initializing(
        &mut self,
        cancel_token: &mut CancellationToken,
        context: &mut Context,
        event: &Event,
    ) -> Outcome<State> {
        match event {
            Event::Step => {
                let task = async {
                    match self.retrieve_meta().await {
                        Ok(_) => {
                            context.poll.push_back(());
                            Transition(State::downloading(cancel_token.clone()))
                        }
                        Err(e) => Transition(State::stopped(Some(Err(e)))),
                    }
                }
                .fuse();
                let cancel = cancel_token.cancelled().fuse();
                futures::pin_mut!(task, cancel);
                futures::select_biased! {
                    _ = cancel => {
                        Transition(State::stopped(Some(Err(
                            TaskInstanceError::new_failed(TaskError::Cancelled),
                        ))))
                    }
                    res = task => { res }
                }
            }
            _ => Super,
        }
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    #[state(superstate = "running")]
    async fn downloading(
        &mut self,
        cancel_token: &mut CancellationToken,
        event: &Event,
    ) -> Outcome<State> {
        match event {
            Event::Step => {
                let task = async {
                    match self.download(cancel_token).await {
                        Ok(_) => Transition(State::stopped(Some(Ok(())))),
                        Err(e) => Transition(State::stopped(Some(Err(e)))),
                    }
                }
                .fuse();
                let cancel = cancel_token.cancelled().fuse();
                futures::pin_mut!(task, cancel);
                futures::select_biased! {
                    _ = cancel => {
                        Transition(State::stopped(Some(Err(
                            TaskInstanceError::new_failed(TaskError::Cancelled),
                        ))))
                    }
                    res = task => { res }
                }
            }
            _ => Super,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{pin::pin, sync::Arc, time::Duration};

    use bolt_load_tests::adapter::simple::{SimpleTestAdapter, calculate_blake3};
    use bolt_load_utils::telemetry::*;
    use futures::StreamExt;
    use pretty_assertions::assert_eq;
    use smol_cancellation_token::CancellationToken;

    use super::*;
    use crate::{
        adapter::BoltLoadAdapter,
        runner::{DataFrameReceiver, LifecycleEvent, LifecycleReceiver, StoppedReason, TaskError},
        runtime::ThreadedRuntimeImpl,
        task::ControlEvent,
    };

    async fn collect_runner_channels(
        lifecycle_rx: LifecycleReceiver,
        data_rx: DataFrameReceiver,
    ) -> (bool, bool, Option<TaskError>, Vec<u8>) {
        let mut lifecycle_rx = pin!(lifecycle_rx);
        let mut data_rx = pin!(data_rx);
        let mut started = false;
        let mut finished = false;
        let mut failed = None;
        let mut data_done = false;
        let mut downloaded_data = Vec::new();

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    event = lifecycle_rx.next(), if !finished && failed.is_none() => {
                        match event {
                            Some(LifecycleEvent::Started) => started = true,
                            Some(LifecycleEvent::Stopped(StoppedReason::Finished)) => finished = true,
                            Some(LifecycleEvent::Stopped(StoppedReason::Failed(error))) => failed = Some(error),
                            None => break,
                        }
                    }
                    frame = data_rx.next(), if !data_done => {
                        match frame {
                            Some(frame) => downloaded_data.extend_from_slice(&frame.data),
                            None => data_done = true,
                        }
                    }
                }

                if (finished || failed.is_some()) && data_done {
                    break;
                }
            }
        })
        .await
        .expect("timed out collecting runner channels");

        (started, finished, failed, downloaded_data)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn throughput_tick_reports_download_and_write_speed() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let wg = WaitGroup::new();
        let runner_manager = RunnerManager::new(1024, 2);
        let mut meters = std::collections::HashMap::new();
        meters.insert(0, 512usize);
        let mut sampler = SpeedSampler::new();
        let mut write_sampler = SpeedSampler::new();
        let mut current_speed = 0.0;
        let mut per_runner_avg_speed = 0.0;
        let mut write_bytes_meter = 768usize;
        let (event_tx, event_rx) = async_channel::bounded(1);

        tokio::time::sleep(Duration::from_millis(1)).await;

        ConcurrentTaskInner::throughout_meter_tick(
            &rt,
            &wg,
            &runner_manager,
            &mut meters,
            &mut sampler,
            &mut current_speed,
            &mut per_runner_avg_speed,
            &mut write_bytes_meter,
            &mut write_sampler,
            &event_tx,
        );

        let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("timed out waiting for progress event")
            .expect("progress event channel closed");

        match event {
            TaskEvent::Downloading(progress) => {
                assert!(progress.speed() > 0.0);
                assert!(progress.write_speed() > 0.0);
            }
            other => panic!("expected downloading progress event, got {other:?}"),
        }
        assert_eq!(meters.get(&0), Some(&0));
        assert_eq!(write_bytes_meter, 0);
        assert!(current_speed > 0.0);
        assert!(per_runner_avg_speed > 0.0);

        wg.wait().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_success() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter = Arc::new(
            Box::new(SimpleTestAdapter::new(10240).with_range_support(true))
                as Box<dyn crate::adapter::BoltLoadAdapter + Send>,
        );
        let range = 1000u64..3000u64;
        let runner_id = 1;
        let (_control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();
        let wg = WaitGroup::new();
        let receiver = ConcurrentTaskInner::create_background_range_runner(
            &rt,
            &wg,
            range.clone(),
            adapter.clone(),
            control_rx,
            runner_id,
            cancel_token,
        );
        let result = receiver.await.expect("receiver closed");
        assert!(result.is_ok());
        let output = result.unwrap();
        let (started, finished, failed, downloaded_data) =
            collect_runner_channels(output.lifecycle_rx, output.data_rx).await;
        wg.wait().await;

        // Verify the amount of downloaded data
        assert!(started);
        assert!(finished);
        assert!(failed.is_none());
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
        use crate::runner::RunnerConnectorError;

        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter = Arc::new(Box::new(SimpleTestAdapter::new(1024).with_failure(true))
            as Box<dyn crate::adapter::BoltLoadAdapter + Send>);
        let range = 0u64..500u64;
        let runner_id = 2;
        let (_control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let receiver = ConcurrentTaskInner::create_background_range_runner(
            &rt,
            &wg,
            range,
            adapter,
            control_rx,
            runner_id,
            cancel_token,
        );
        let result = receiver.await.expect("receiver closed");
        wg.wait().await;

        // Connection should fail because range_stream fails
        assert!(result.is_err());
        match result {
            Ok(_) => panic!("Expected Connection error, got cons rx"),
            Err(RunnerConnectorError::Connection { .. }) => {}
            Err(e) => panic!("Expected Connection error, got: {:?}", e),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_with_cancellation() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter = Arc::new(
            Box::new(SimpleTestAdapter::new(10240).with_range_support(true))
                as Box<dyn crate::adapter::BoltLoadAdapter + Send>,
        );
        let range = 0u64..5000u64;
        let runner_id = 3;
        let (_control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let receiver = ConcurrentTaskInner::create_background_range_runner(
            &rt,
            &wg,
            range,
            adapter,
            control_rx,
            runner_id,
            cancel_token.clone(),
        );
        let result = receiver.await.expect("receiver closed");
        assert!(result.is_ok());
        let output = result.unwrap();

        // Cancel the task
        cancel_token.cancel();

        let (started, finished, failed, _) =
            collect_runner_channels(output.lifecycle_rx, output.data_rx).await;
        wg.wait().await;

        assert!(started);
        assert!(!finished);
        assert!(failed.is_some_and(|e| e.is_cancelled()));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_small_range_runner_with_control_messages() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter = Arc::new(
            Box::new(SimpleTestAdapter::new(10240).with_range_support(true))
                as Box<dyn crate::adapter::BoltLoadAdapter + Send>,
        );
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
        let receiver = ConcurrentTaskInner::create_background_range_runner(
            &rt,
            &wg,
            range.clone(),
            adapter,
            control_rx,
            runner_id,
            cancel_token,
        );
        let result = receiver.await.expect("receiver closed");
        assert!(result.is_ok());
        let output = result.unwrap();
        let (started, finished, failed, downloaded_data) =
            collect_runner_channels(output.lifecycle_rx, output.data_rx).await;
        wg.wait().await;

        // Verify that downloaded amount is the adjusted size
        let total_downloaded = downloaded_data.len();
        assert!(started);
        assert!(finished);
        assert!(failed.is_none());
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
        let adapter = Arc::new(Box::new(
            SimpleTestAdapter::new(content_size).with_range_support(true),
        ) as Box<dyn crate::adapter::BoltLoadAdapter + Send>);

        // Test boundary range: start from end of file
        let range = 900u64..1000u64;
        let runner_id = 5;
        let (_control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let receiver = ConcurrentTaskInner::create_background_range_runner(
            &rt,
            &wg,
            range.clone(),
            adapter.clone(),
            control_rx,
            runner_id,
            cancel_token,
        );
        let result = receiver.await.expect("receiver closed");
        assert!(result.is_ok());
        let output = result.unwrap();
        let (started, finished, failed, downloaded_data) =
            collect_runner_channels(output.lifecycle_rx, output.data_rx).await;
        wg.wait().await;

        assert!(started);
        assert!(finished);
        assert!(failed.is_none());
        assert_eq!(downloaded_data.len(), (range.end - range.start) as usize);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_zero_length_range() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter = Arc::new(
            Box::new(SimpleTestAdapter::new(1000).with_range_support(true))
                as Box<dyn crate::adapter::BoltLoadAdapter + Send>,
        );

        // Test zero-length range
        let range = 500u64..500u64;
        let runner_id = 6;
        let (_control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let receiver = ConcurrentTaskInner::create_background_range_runner(
            &rt,
            &wg,
            range,
            adapter,
            control_rx,
            runner_id,
            cancel_token,
        );
        let result = receiver.await.expect("receiver closed");
        assert!(result.is_ok());
        let output = result.unwrap();
        let (started, finished, failed, downloaded_data) =
            collect_runner_channels(output.lifecycle_rx, output.data_rx).await;
        wg.wait().await;

        assert!(started);
        assert!(finished);
        assert!(failed.is_none());
        assert!(downloaded_data.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_create_background_range_runner_multiple_runners() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter = Arc::new(
            Box::new(SimpleTestAdapter::new(4000).with_range_support(true))
                as Box<dyn crate::adapter::BoltLoadAdapter + Send>,
        );

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
                let receiver = ConcurrentTaskInner::create_background_range_runner(
                    &rt_clone,
                    &wg,
                    range.clone(),
                    adapter_clone,
                    control_rx,
                    runner_id,
                    cancel_token,
                );
                let result = receiver.await.expect("receiver closed");
                let output = result.expect("connection failed");

                // Keep the control channel sender alive
                let _control_tx = control_tx;

                let (started, finished, failed, downloaded_data) =
                    collect_runner_channels(output.lifecycle_rx, output.data_rx).await;
                wg.wait().await;

                // Ensure the runner completed correctly
                if !started || !finished {
                    panic!("Runner {runner_id} did not finish properly");
                }
                if let Some(kind) = failed {
                    panic!("Runner {runner_id} failed with error: {kind:?}");
                }

                (runner_id, downloaded_data.len(), range.end - range.start)
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
        let adapter = Arc::new(
            Box::new(SimpleTestAdapter::new(5000).with_range_support(true))
                as Box<dyn crate::adapter::BoltLoadAdapter + Send>,
        );
        let range = 1500u64..3500u64;
        let runner_id = 7;
        let (_control_tx, control_rx) = async_channel::bounded(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let receiver = ConcurrentTaskInner::create_background_range_runner(
            &rt,
            &wg,
            range.clone(),
            adapter.clone(),
            control_rx,
            runner_id,
            cancel_token,
        );
        let result = receiver.await.expect("receiver closed");
        assert!(result.is_ok());
        let output = result.unwrap();
        let (started, finished, failed, downloaded_data) =
            collect_runner_channels(output.lifecycle_rx, output.data_rx).await;
        wg.wait().await;

        assert!(started);
        assert!(finished);
        assert!(failed.is_none());

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
}
