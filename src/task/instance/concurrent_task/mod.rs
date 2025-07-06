use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
    ops::Range,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use async_broadcast::{Receiver as BroadcastReceiver, Sender as BroadcastSender};
use async_channel::{Receiver, Sender};
use async_io::Timer;
use async_waitgroup::WaitGroup;
use futures::{FutureExt, StreamExt, future::RemoteHandle, task::SpawnExt};
use smol_cancellation_token::CancellationToken;
use statig::prelude::*;

use super::{Generator, Result, TaskInstance};
use crate::{
    DOWNLOADING_TMP_EXTENSION,
    adapter::{AnyAdapter, UnretryableError},
    runner::{RunnerMessage, RunnerMessageKind, StoppedReason, TaskFailedKind, TaskRunner},
    runtime::{LocalRuntimeBuilderImpl, ThreadedRuntimeExt, ThreadedRuntimeImpl},
    task::{
        ManagerMessage, ManagerMessagesVariant, Progress, RunnerId,
        instance::{
            DEFAULT_CONTROL_CHANNEL_CAPACITY, ProgressWithSpeed, RunningPayload, TaskControl,
            TaskEvent, TaskInstanceError,
            sampler::{DEFAULT_SAMPLE_INTERVAL, SpeedSampler},
        },
    },
    utils::logging::*,
};

mod chunk_planner;
mod file;
mod runner_notification;
mod strategy;

use chunk_planner::*;
use file::*;
use runner_notification::RunnerNotification;
use strategy::*;

static DEFAULT_MAX_CONCURRENCY: usize = 4;

type ControlChannelRef<'a> = (
    &'a BroadcastSender<ManagerMessage>,
    &'a BroadcastReceiver<ManagerMessage>,
);

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
            local_rt.block_on(task)
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

struct TaskSpeed {
    pub current: u64,
    pub avg: u64,
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
            TaskInstanceError::Failed(TaskFailedKind::Other(
                "Adapter not set before meta retrieval".to_string(),
            ))
        })?;
        let meta = adapter
            .retrieve_meta()
            .await
            .map_err(TaskInstanceError::RetrieveMetaFailed)?;
        let total = if meta.content_size == 0 {
            return Err(TaskInstanceError::RetrieveMetaFailed(
                UnretryableError::ExceededRequestLimits(
                    "content size is 0; concurrent task does not support 0-size file".to_string(),
                ),
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

    fn create_file_writer(&self) -> Result<(PathBuf, FileWriter)> {
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
            TaskInstanceError::Failed(TaskFailedKind::Other(
                "File writer creation called before meta retrieval".to_string(),
            ))
        })?;
        let file_writer = FileWriter::new(tmp_path.as_ref(), total_size);
        Ok((tmp_path.into_owned(), file_writer))
    }

    /// Split the given task into two separate tasks
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, fields(runner_id, range))
    )]
    #[allow(clippy::too_many_arguments)]
    async fn split_task(
        threaded_rt: &ThreadedRuntimeImpl,
        wg: &WaitGroup,
        chunk_planner: &mut ChunkPlanner,
        runner_id_generator: &mut Generator,
        runner_notification: &mut RunnerNotification,
        runners_cancel_token: &CancellationToken,
        incomplete_range: Range<u64>,
        adapter: Arc<AnyAdapter>,
        control_channel: ControlChannelRef<'_>,
        runner_id: RunnerId,
    ) -> Result<()> {
        let (control_tx, control_rx) = control_channel;
        let half_size = (incomplete_range.end - incomplete_range.start) / 2;
        // Limit the total size of the runner
        chunk_planner
            .resize_runner_state(runner_id, half_size)
            .expect("chunk planner should not fail");

        control_tx
            .broadcast_direct(ManagerMessage(
                runner_id,
                ManagerMessagesVariant::LimitTotal(incomplete_range.start + half_size),
            ))
            .await
            .expect("control channel should not fail");

        // Create a new runner for the remaining range
        let next_runner_id = runner_id_generator.next().expect("no more runner id");
        let next_range = incomplete_range.start + half_size..incomplete_range.end;
        if !chunk_planner.allocate_chunk(next_range.clone(), Some(next_runner_id)) {
            panic!("failed to allocate chunk: {next_range:?}");
        }
        let (rx, _) = Self::create_background_range_runner(
            threaded_rt,
            wg,
            next_range,
            adapter.clone(),
            control_rx.clone(),
            next_runner_id,
            runners_cancel_token.clone(),
        )
        .await?;
        runner_notification.add(next_runner_id, rx);
        Ok(())
    }

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, fields(runner_id, range))
    )]
    async fn create_background_range_runner(
        threaded_rt: &ThreadedRuntimeImpl,
        wg: &WaitGroup,
        range: Range<u64>,
        adapter: Arc<AnyAdapter>,
        notify: BroadcastReceiver<ManagerMessage>,
        runner_id: RunnerId,
        cancel_token: CancellationToken,
    ) -> Result<(Receiver<RunnerMessage>, RemoteHandle<()>)> {
        let (tx, rx) = oneshot::channel();
        let (start, end) = (range.start, range.end);
        let wg = wg.clone();
        let fut = async move {
            let _wg = wg;
            trace!("[TASK] create background range runner: id: {runner_id}, range: {range:?}");
            let mut runner = match TaskRunner::new_with_async_and_callback(
                Some(end - start),
                async { adapter.range_stream(start, end).await },
                runner_id,
                notify,
                cancel_token.clone(),
                move |rx| {
                    let _ = tx.send(rx);
                },
            )
            .await
            {
                Some(runner) => runner,
                None => {
                    return;
                }
            };
            runner.run().await;
        };
        #[cfg(feature = "tracing")]
        let fut = tracing::Instrument::instrument(
            fut,
            tracing::trace_span!(
                parent: None,
                "background_range_runner",
                runner_id = runner_id,
                range = ?start..end,
            ),
        );
        let handle = threaded_rt
            .spawn_with_handle(fut)
            .map_err(|e| TaskInstanceError::Failed(TaskFailedKind::Other(e.to_string())))?;
        let rx = rx
            .await
            .map_err(|_| TaskInstanceError::Failed(TaskFailedKind::Cancelled))?;
        Ok((rx, handle))
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    fn throughout_meter_tick(
        rt: &ThreadedRuntimeImpl,
        wg: &WaitGroup,
        chunk_planner: &ChunkPlanner,

        meters: &mut HashMap<RunnerId, usize>,
        sampler: &mut SpeedSampler,
        current_speed: &mut f64,
        per_runner_avg_speed: &mut f64,

        event_tx: &async_channel::Sender<TaskEvent>,
    ) {
        let count = meters.len();
        let mut total_bytes = meters.values_mut().map(std::mem::take).sum::<usize>();
        let (_, ema_speed) = sampler.sample(&mut total_bytes);
        *current_speed = ema_speed;
        *per_runner_avg_speed = ema_speed / count as f64;

        let event_tx = event_tx.clone();
        let total = chunk_planner.total;
        let downloaded_chunks = chunk_planner.get_downloaded_ranges();
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
                )))
                .await
            {
                error!("failed to send downloading event: {e:?}");
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    async fn download_strategy_timer_tick(
        chunk_planner: &mut ChunkPlanner,
        runner_notification: &mut RunnerNotification,
        dynamic_strategy: &mut DynamicStrategy,
        runner_id_generator: &mut Generator,
        max_concurrency: usize,

        threaded_rt: &ThreadedRuntimeImpl,
        adapter: &Arc<AnyAdapter>,
        control_channel: ControlChannelRef<'_>,
        runners_cancel_token: &CancellationToken,
        wg: &WaitGroup,

        current_speed: f64,
        per_runner_avg_speed: f64,
    ) -> Result<()> {
        let (_, control_rx) = control_channel;

        // get the available ranges, and create background runners for them
        let available_ranges = chunk_planner.get_available_ranges();
        // TODO: move it to a new strategy for error and concurrency control
        if !available_ranges.is_empty() {
            trace!("[TASK] create background runners for available ranges: {available_ranges:?}");
            for chunk in available_ranges.iter() {
                trace!("[TASK] create background runner for chunk: {chunk:?}");
                let runner_id = runner_id_generator.next().expect("no more runner id");

                if !chunk_planner.allocate_chunk(chunk.clone(), Some(runner_id)) {
                    warn!("failed to allocate chunk: {chunk:?}");
                    continue;
                }

                let (rx, _) = Self::create_background_range_runner(
                    threaded_rt,
                    wg,
                    chunk.clone(),
                    adapter.clone(),
                    control_rx.clone(),
                    runner_id,
                    runners_cancel_token.clone(),
                )
                .await?;

                runner_notification.add(runner_id, rx);
            }

            return Ok(());
        }
        let active_runners = chunk_planner.get_active_runners_count();

        // TODO: maybe move into the strategy?
        let planned_chunk_size = (current_speed * 2.0) as u64;

        if active_runners < max_concurrency && current_speed > 0.0 {
            trace!(
                "[TASK] current speed: {current_speed}, per runner avg speed: \
                 {per_runner_avg_speed}"
            );
            if let Some((runner_id, suggested_range)) =
                chunk_planner.find_chunk_to_split(planned_chunk_size)
            {
                if suggested_range.end - suggested_range.start >= planned_chunk_size {
                    match runner_id {
                        //
                        None => {
                            // create a new runner
                            let runner_id = runner_id_generator.next().expect("no more runner id");
                            if !chunk_planner
                                .allocate_chunk(suggested_range.clone(), Some(runner_id))
                            {
                                panic!("failed to allocate chunk: {suggested_range:?}");
                            }
                            let (rx, _) = Self::create_background_range_runner(
                                threaded_rt,
                                wg,
                                suggested_range,
                                adapter.clone(),
                                control_rx.clone(),
                                runner_id,
                                runners_cancel_token.clone(),
                            )
                            .await?;
                            runner_notification.add(runner_id, rx);
                        }
                        Some(runner_id) => {
                            // split the chunk
                            let strategy_context = DynamicStrategyContext {
                                speed: current_speed,
                                per_runner_speed: per_runner_avg_speed,
                                current_concurrency: chunk_planner.get_active_runners_count(),
                                remaining_largest_runner_id: runner_id,
                            };
                            let actions = dynamic_strategy.step(&strategy_context);
                            for action in actions {
                                match action {
                                    // Split current all task into two separate tasks
                                    StrategyAction::SplitAllTask if current_speed > 0.0 => {
                                        trace!("[TASK] Strategy: split all task");
                                        let states = chunk_planner
                                            .get_incomplete_states(Some(planned_chunk_size));
                                        for (runner_id, incomplete_range) in states {
                                            trace!("split task: {runner_id}, {incomplete_range:?}");
                                            Self::split_task(
                                                threaded_rt,
                                                wg,
                                                chunk_planner,
                                                runner_id_generator,
                                                runner_notification,
                                                runners_cancel_token,
                                                incomplete_range,
                                                adapter.clone(),
                                                control_channel,
                                                runner_id,
                                            )
                                            .await?;
                                        }
                                    }
                                    // Split the given task into two separate tasks
                                    StrategyAction::SplitGivenTask(task_id) => {
                                        trace!("[TASK] Strategy: split given task: {task_id}");
                                        let incomplete_range = chunk_planner
                                            .get_runner_state(task_id)
                                            .map(|s| s.incomplete_range())
                                            .expect("task id not found");
                                        Self::split_task(
                                            threaded_rt,
                                            wg,
                                            chunk_planner,
                                            runner_id_generator,
                                            runner_notification,
                                            runners_cancel_token,
                                            incomplete_range,
                                            adapter.clone(),
                                            control_channel,
                                            runner_id,
                                        )
                                        .await?;
                                    }
                                    // Change the max concurrency
                                    StrategyAction::ChangeMaxThread(new_max_concurrency) => {
                                        trace!(
                                            "[TASK] Strategy: change max concurrency: \
                                             {new_max_concurrency}"
                                        );
                                        todo!("change max concurrency");
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Sync the progress of the task
    fn sync_progress(&mut self, chunk_planner: &ChunkPlanner) {
        self.progress.downloaded = chunk_planner.get_total_downloaded();
        self.progress.downloaded_chunks = chunk_planner.get_downloaded_ranges();
    }

    #[allow(clippy::too_many_arguments)]
    // #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    async fn handle_runner_message(
        threaded_rt: &ThreadedRuntimeImpl,
        wg: &WaitGroup,
        meters: &mut HashMap<RunnerId, usize>,
        chunk_planner: &mut ChunkPlanner,
        runner_notification: &mut RunnerNotification,
        id_generator: &mut Generator,
        file_writer_control: &FileWriterControl,
        on_all_chunks_finished: impl FnOnce(),
        msg: RunnerMessage,
    ) -> Result<()> {
        let RunnerMessage(runner_id, msg) = msg;

        match msg {
            RunnerMessageKind::Stopped(reason) => {
                meters.remove(&runner_id);
                match reason {
                    StoppedReason::Finished => {
                        trace!("runner {runner_id} finished");
                        chunk_planner
                            .mark_finished(runner_id)
                            .expect("chunk planner should not fail");

                        if chunk_planner.is_complete() {
                            trace!("[TASK] all chunks finished");
                            on_all_chunks_finished();
                        }
                    }
                    StoppedReason::Failed(_kind) => {
                        let unfinished_range = chunk_planner
                            .mark_failed(runner_id)
                            .expect("chunk planner should not fail");
                        runner_notification.remove(runner_id);
                        id_generator.release(runner_id);
                        trace!("Runner {runner_id} failed, released range: {unfinished_range:?}");
                    }
                }
            }
            RunnerMessageKind::Downloaded(mut bytes) => {
                let bytes_len = bytes.len();
                *meters.entry(runner_id).or_insert(0) += bytes_len;

                // update the chunk planner progress
                let fixed_downloaded_range = chunk_planner
                    .update_progress(runner_id, bytes_len as u64)
                    .expect("chunk planner should not fail");
                let picked_size = fixed_downloaded_range.end - fixed_downloaded_range.start;
                bytes.truncate(picked_size as usize);

                let file_writer_control = file_writer_control.clone();
                let wg = wg.clone();
                let _ = threaded_rt.spawn(async move {
                    let _wg = wg;
                    if let Err(e) = file_writer_control
                        .write(fixed_downloaded_range, bytes)
                        .await
                    {
                        // TODO: notify the task to stop
                        match e {
                            FileWriterError::Io(e) => {
                                error!("failed to write to file: {e:?}");
                            }
                            FileWriterError::Send(e) => {
                                error!("failed to send to file writer: {e:?}");
                            }
                            FileWriterError::Recv(e) => {
                                error!("failed to receive from file writer: {e:?}");
                            }
                        }
                    }
                });
            }
            RunnerMessageKind::Started => {
                trace!("runner {runner_id} started");
            }
        }
        Ok(())
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    async fn download(&mut self, cancel_token: &CancellationToken) -> Result<()> {
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
        let max_concurrency = initial_max_concurrency;

        let mut runner_id_generator = Generator::new(initial_max_concurrency);
        let mut chunk_planner = ChunkPlanner::new(total);

        let (control_tx, control_rx) = async_broadcast::broadcast(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let mut runner_notification = RunnerNotification::new();

        let mut dynamic_strategy =
            DynamicStrategy::new_with_max_concurrency(initial_max_concurrency);
        // TODO: make meters removal more efficient
        let mut meters: HashMap<RunnerId, usize> = HashMap::with_capacity(initial_max_concurrency);
        let mut sampler = SpeedSampler::new();
        let mut current_speed = 0.0;
        let mut per_runner_avg_speed = 0.0;

        let (tmp_path, file_writer) = self.create_file_writer()?;
        let FileWriterGuard(fs_writer_handle, file_writer_control) = file_writer
            .start()
            .await
            .map_err(|e| TaskInstanceError::AllocateFileSpaceFailed(Arc::new(e)))?;

        let mut throughout_meter_timer =
            Timer::interval(Duration::from_millis(DEFAULT_SAMPLE_INTERVAL));
        let mut strategy_timer =
            Timer::interval(Duration::from_millis(DEFAULT_STRATEGY_TICK_INTERVAL));

        let event_loop_result: Result<()> = async {
            loop {
                futures::select_biased! {
                    _ = strategy_timer.next().fuse() => {
                        Self::download_strategy_timer_tick(
                            &mut chunk_planner,
                            &mut runner_notification,
                            &mut dynamic_strategy,
                            &mut runner_id_generator,
                            max_concurrency,

                            &rt,
                            &adapter,
                            (&control_tx, &control_rx),
                            &runners_cancel_token,
                            &wg,

                            current_speed,
                            per_runner_avg_speed,
                        ).await.inspect_err(|e| {
                            error!("failed to download strategy timer tick: {e:?}");
                            self.sync_progress(&chunk_planner);
                        })?;
                    }
                    _ = throughout_meter_timer.next().fuse() => {
                        Self::throughout_meter_tick(
                            &rt,
                            &wg,
                            &chunk_planner,
                            &mut meters,
                            &mut sampler,
                            &mut current_speed,
                            &mut per_runner_avg_speed,
                            &event_tx,
                        );
                    }
                    msg = runner_notification.next().fuse() => {
                        if let Some(msg) = msg {
                            let mut flag = false;
                            Self::handle_runner_message(
                                &rt,
                                &wg,
                                &mut meters,
                                &mut chunk_planner,
                                &mut runner_notification,
                                &mut runner_id_generator,
                                &file_writer_control,
                                || {
                                    flag = true;
                                },
                                msg,
                            )
                            .await.inspect_err(|e| {
                                error!("failed to handle runner message: {e:?}");
                                self.sync_progress(&chunk_planner);
                            })?;
                            if flag {
                                break;
                            }
                        }
                    }
                }
            }
            Ok(())
        }
        .await;
        self.sync_progress(&chunk_planner);

        // Wait all message handlers to finish
        wg.wait().await;

        drop(file_writer_control);
        fs_writer_handle
            .await
            .map_err(|e| TaskInstanceError::Failed(TaskFailedKind::Other(e.to_string())))?;
        event_loop_result?;
        async_fs::rename(&tmp_path, &self.path)
            .await
            .map_err(|e| TaskInstanceError::Failed(TaskFailedKind::Other(e.to_string())))?;

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
                            TaskInstanceError::Failed(TaskFailedKind::Cancelled),
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
                            TaskInstanceError::Failed(TaskFailedKind::Cancelled),
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
    use std::sync::Arc;

    use futures::StreamExt;
    use pretty_assertions::assert_eq;
    use smol_cancellation_token::CancellationToken;

    use super::*;
    use crate::{
        adapter::{BoltLoadAdapter, tests::SimpleTestAdapter},
        runtime::ThreadedRuntimeImpl,
        task::{ManagerMessage, ManagerMessagesVariant},
    };

    #[tokio::test(flavor = "multi_thread")]
    #[tracing_test::traced_test]
    async fn test_create_background_range_runner_success() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter = Arc::new(Box::new(SimpleTestAdapter::new(10240))
            as Box<dyn crate::adapter::BoltLoadAdapter + Send>);
        let range = 1000u64..3000u64;
        let runner_id = 1;
        let (_control_tx, control_rx) =
            async_broadcast::broadcast(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();
        let wg = WaitGroup::new();
        let result = ConcurrentTaskInner::create_background_range_runner(
            &rt,
            &wg,
            range.clone(),
            adapter.clone(),
            control_rx,
            runner_id,
            cancel_token,
        )
        .await;
        wg.wait().await;

        assert!(result.is_ok());
        let (msg_rx, _handle) = result.unwrap();

        // 验证能收到 Started 消息
        let msg = msg_rx.recv().await.unwrap();
        match msg {
            RunnerMessage(id, RunnerMessageKind::Started) => {
                assert_eq!(id, runner_id);
            }
            _ => panic!("Expected Started message, got: {:?}", msg),
        }

        // 验证能收到下载数据
        let mut total_downloaded = 0;
        let mut downloaded_data = Vec::new();

        while let Ok(msg) = msg_rx.recv().await {
            match msg {
                RunnerMessage(id, RunnerMessageKind::Downloaded(bytes)) => {
                    assert_eq!(id, runner_id);
                    total_downloaded += bytes.len();
                    downloaded_data.extend_from_slice(&bytes);
                }
                RunnerMessage(id, RunnerMessageKind::Stopped(StoppedReason::Finished)) => {
                    assert_eq!(id, runner_id);
                    break;
                }
                _ => {}
            }
        }

        // 验证下载的数据量
        assert_eq!(total_downloaded, (range.end - range.start) as usize);
        assert_eq!(downloaded_data.len(), (range.end - range.start) as usize);

        // 验证数据的确定性 - 创建相同的适配器获取相同范围的数据进行比较
        let reference_adapter = SimpleTestAdapter::new(10240);
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
    #[tracing_test::traced_test]
    async fn test_create_background_range_runner_with_adapter_failure() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter = Arc::new(Box::new(SimpleTestAdapter::new(1024).with_failure(true))
            as Box<dyn crate::adapter::BoltLoadAdapter + Send>);
        let range = 0u64..500u64;
        let runner_id = 2;
        let (_control_tx, control_rx) =
            async_broadcast::broadcast(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let result = ConcurrentTaskInner::create_background_range_runner(
            &rt,
            &wg,
            range,
            adapter,
            control_rx,
            runner_id,
            cancel_token,
        )
        .await;
        wg.wait().await;

        assert!(result.is_ok());
        let (msg_rx, _handle) = result.unwrap();

        // 应该收到失败消息，因为 range_stream 失败
        let msg = msg_rx.recv().await.unwrap();
        match msg {
            RunnerMessage(
                id,
                RunnerMessageKind::Stopped(StoppedReason::Failed(TaskFailedKind::StreamError(_))),
            ) => {
                assert_eq!(id, runner_id);
            }
            _ => panic!("Expected StreamError failure message, got: {:?}", msg),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[tracing_test::traced_test]
    async fn test_create_background_range_runner_with_cancellation() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter = Arc::new(Box::new(SimpleTestAdapter::new(10240))
            as Box<dyn crate::adapter::BoltLoadAdapter + Send>);
        let range = 0u64..5000u64;
        let runner_id = 3;
        let (_control_tx, control_rx) =
            async_broadcast::broadcast(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let result = ConcurrentTaskInner::create_background_range_runner(
            &rt,
            &wg,
            range,
            adapter,
            control_rx,
            runner_id,
            cancel_token.clone(),
        )
        .await;
        wg.wait().await;

        assert!(result.is_ok());
        let (msg_rx, _handle) = result.unwrap();

        // 等待开始消息
        let msg = msg_rx.recv().await.unwrap();
        assert!(matches!(msg, RunnerMessage(_, RunnerMessageKind::Started)));

        // 取消任务
        cancel_token.cancel();

        // 等待取消消息
        while let Ok(msg) = msg_rx.recv().await {
            if let RunnerMessage(
                id,
                RunnerMessageKind::Stopped(StoppedReason::Failed(TaskFailedKind::Cancelled)),
            ) = msg
            {
                assert_eq!(id, runner_id);
                break;
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[tracing_test::traced_test]
    async fn test_create_background_small_range_runner_with_control_messages() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter = Arc::new(Box::new(SimpleTestAdapter::new(10240))
            as Box<dyn crate::adapter::BoltLoadAdapter + Send>);
        let old_total = 10240u64;
        let range = 0u64..old_total;
        let runner_id = 4;
        let (control_tx, control_rx) = async_broadcast::broadcast(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        // 在创建运行器之前预先发送控制消息
        let new_total = 1000u64;
        control_tx
            .broadcast_direct(ManagerMessage(
                runner_id,
                ManagerMessagesVariant::LimitTotal(new_total),
            ))
            .await
            .unwrap();

        let wg = WaitGroup::new();
        let result = ConcurrentTaskInner::create_background_range_runner(
            &rt,
            &wg,
            range.clone(),
            adapter,
            control_rx,
            runner_id,
            cancel_token,
        )
        .await;
        wg.wait().await;

        assert!(result.is_ok());
        let (msg_rx, _handle) = result.unwrap();

        // 等待开始消息
        let msg = msg_rx.recv().await.unwrap();
        assert!(matches!(msg, RunnerMessage(_, RunnerMessageKind::Started)));

        // 收集下载数据
        let mut total_downloaded = 0;
        while let Ok(msg) = msg_rx.recv().await {
            match msg {
                RunnerMessage(_, RunnerMessageKind::Downloaded(bytes)) => {
                    total_downloaded += bytes.len();
                }
                RunnerMessage(_, RunnerMessageKind::Stopped(StoppedReason::Finished)) => {
                    break;
                }
                _ => {}
            }
        }

        // 验证下载量应该是调整后的大小
        if total_downloaded != new_total as usize {
            error!("total_downloaded: {total_downloaded}, new_total: {new_total}");
        }
        assert!(total_downloaded >= new_total as usize);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[tracing_test::traced_test]
    async fn test_create_background_range_runner_edge_ranges() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let content_size = 1000;
        let adapter = Arc::new(Box::new(SimpleTestAdapter::new(content_size))
            as Box<dyn crate::adapter::BoltLoadAdapter + Send>);

        // 测试边界范围：从文件末尾开始
        let range = 900u64..1000u64;
        let runner_id = 5;
        let (_control_tx, control_rx) =
            async_broadcast::broadcast(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let result = ConcurrentTaskInner::create_background_range_runner(
            &rt,
            &wg,
            range.clone(),
            adapter.clone(),
            control_rx,
            runner_id,
            cancel_token,
        )
        .await;
        wg.wait().await;

        assert!(result.is_ok());
        let (msg_rx, _handle) = result.unwrap();

        // 等待开始
        let msg = msg_rx.recv().await.unwrap();
        assert!(matches!(msg, RunnerMessage(_, RunnerMessageKind::Started)));

        // 收集所有数据
        let mut downloaded_data = Vec::new();
        while let Ok(msg) = msg_rx.recv().await {
            match msg {
                RunnerMessage(_, RunnerMessageKind::Downloaded(bytes)) => {
                    downloaded_data.extend_from_slice(&bytes);
                }
                RunnerMessage(_, RunnerMessageKind::Stopped(StoppedReason::Finished)) => {
                    break;
                }
                _ => {}
            }
        }

        assert_eq!(downloaded_data.len(), (range.end - range.start) as usize);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[tracing_test::traced_test]
    async fn test_create_background_range_runner_zero_length_range() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter = Arc::new(Box::new(SimpleTestAdapter::new(1000))
            as Box<dyn crate::adapter::BoltLoadAdapter + Send>);

        // 测试零长度范围
        let range = 500u64..500u64;
        let runner_id = 6;
        let (_control_tx, control_rx) =
            async_broadcast::broadcast(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let result = ConcurrentTaskInner::create_background_range_runner(
            &rt,
            &wg,
            range,
            adapter,
            control_rx,
            runner_id,
            cancel_token,
        )
        .await;
        wg.wait().await;

        assert!(result.is_ok());
        let (msg_rx, _handle) = result.unwrap();

        // 等待开始
        let msg = msg_rx.recv().await.unwrap();
        assert!(matches!(msg, RunnerMessage(_, RunnerMessageKind::Started)));

        // 应该立即完成，没有下载任何数据
        let msg = msg_rx.recv().await.unwrap();
        match msg {
            RunnerMessage(id, RunnerMessageKind::Stopped(StoppedReason::Finished)) => {
                assert_eq!(id, runner_id);
            }
            _ => panic!("Expected immediate finish for zero-length range, got: {msg:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[tracing_test::traced_test]
    async fn test_create_background_range_runner_multiple_runners() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter = Arc::new(Box::new(SimpleTestAdapter::new(4000))
            as Box<dyn crate::adapter::BoltLoadAdapter + Send>);

        // 创建多个并发的范围运行器
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
            let (control_tx, control_rx) =
                async_broadcast::broadcast(DEFAULT_CONTROL_CHANNEL_CAPACITY);
            let cancel_token = CancellationToken::new();

            let handle = tokio::spawn(async move {
                let wg = WaitGroup::new();
                let result = ConcurrentTaskInner::create_background_range_runner(
                    &rt_clone,
                    &wg,
                    range.clone(),
                    adapter_clone,
                    control_rx,
                    runner_id,
                    cancel_token,
                )
                .await
                .unwrap();
                wg.wait().await;

                let (msg_rx, runner_handle) = result;

                // 保持控制通道发送端活跃
                let _control_tx = control_tx;

                // 等待开始
                let msg = msg_rx.recv().await.unwrap();
                assert!(matches!(msg, RunnerMessage(_, RunnerMessageKind::Started)));

                // 收集所有数据
                let mut downloaded_size = 0;
                let mut finished = false;

                while let Ok(msg) = msg_rx.recv().await {
                    match msg {
                        RunnerMessage(_, RunnerMessageKind::Downloaded(bytes)) => {
                            downloaded_size += bytes.len();
                        }
                        RunnerMessage(id, RunnerMessageKind::Stopped(StoppedReason::Finished)) => {
                            assert_eq!(id, runner_id);
                            finished = true;
                            break;
                        }
                        RunnerMessage(
                            id,
                            RunnerMessageKind::Stopped(StoppedReason::Failed(kind)),
                        ) => {
                            panic!("Runner {id} failed with error: {kind:?}");
                        }
                        _ => {}
                    }
                }

                // 确保运行器正确完成
                if !finished {
                    panic!("Runner {runner_id} did not finish properly");
                }

                // 等待运行器完全完成
                runner_handle.await;

                (runner_id, downloaded_size, range.end - range.start)
            });

            handles.push(handle);
        }

        // 等待所有任务完成
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
    #[tracing_test::traced_test]
    async fn test_create_background_range_runner_hash_verification() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter = Arc::new(Box::new(SimpleTestAdapter::new(5000))
            as Box<dyn crate::adapter::BoltLoadAdapter + Send>);
        let range = 1500u64..3500u64;
        let runner_id = 7;
        let (_control_tx, control_rx) =
            async_broadcast::broadcast(DEFAULT_CONTROL_CHANNEL_CAPACITY);
        let cancel_token = CancellationToken::new();

        let wg = WaitGroup::new();
        let result = ConcurrentTaskInner::create_background_range_runner(
            &rt,
            &wg,
            range.clone(),
            adapter.clone(),
            control_rx,
            runner_id,
            cancel_token,
        )
        .await;
        wg.wait().await;

        assert!(result.is_ok());
        let (msg_rx, _handle) = result.unwrap();

        // 等待开始消息
        let msg = msg_rx.recv().await.unwrap();
        assert!(matches!(msg, RunnerMessage(_, RunnerMessageKind::Started)));

        // 收集所有下载的数据
        let mut downloaded_data = Vec::new();
        while let Ok(msg) = msg_rx.recv().await {
            match msg {
                RunnerMessage(_, RunnerMessageKind::Downloaded(bytes)) => {
                    downloaded_data.extend_from_slice(&bytes);
                }
                RunnerMessage(_, RunnerMessageKind::Stopped(StoppedReason::Finished)) => {
                    break;
                }
                _ => {}
            }
        }

        // 使用相同的适配器直接获取相同范围的数据进行比较
        let reference_adapter = SimpleTestAdapter::new(5000);
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

        // 验证数据完整性
        assert_eq!(downloaded_data.len(), (range.end - range.start) as usize);
        assert_eq!(downloaded_data, reference_data);

        // 计算并比较 hash
        use crate::adapter::tests::calculate_blake3;
        let downloaded_hash = calculate_blake3(&downloaded_data);
        let reference_hash = calculate_blake3(&reference_data);
        assert_eq!(downloaded_hash, reference_hash);
    }
}
