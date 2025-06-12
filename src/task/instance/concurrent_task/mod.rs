use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
    ops::Range,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use async_channel::{Receiver, Sender};
use async_io::Timer;
use futures::{FutureExt, StreamExt, future::RemoteHandle, task::SpawnExt};
use smol_cancellation_token::CancellationToken;
use statig::{Response::*, prelude::*};

use crate::{
    DOWNLOADING_TMP_EXTENSION,
    adapter::{AnyAdapter, UnretryableError},
    runner::{RunnerMessage, RunnerMessageKind, StoppedReason, TaskFailedKind, TaskRunner},
    runtime::ThreadedRuntimeImpl,
    task::{
        ManagerMessage, ManagerMessagesVariant, Progress, RunnerId,
        instance::{
            ProgressWithSpeed, RunningPayload, TaskControl, TaskEvent, TaskInstanceError,
            sampler::{SAMPLE_INTERVAL, Sampler},
        },
        runner_notification::RunnerNotification,
    },
};

use super::{Generator, Result, TaskInstance};

mod chunk_planner;
mod file;
mod strategy;

use chunk_planner::*;
use file::*;
use strategy::*;

static DEFAULT_MAX_CONCURRENCY: usize = 4;

/// Starts with 1 task, and then split the task by dynamic strategy
const INITIAL_TASK_NUM: usize = 1;

pub struct ConcurrentTask {
    rt: ThreadedRuntimeImpl,
    task: Option<TaskControl>,
    progress: Progress,
}

impl TaskInstance for ConcurrentTask {
    fn run(
        &mut self,
        adapter: Arc<AnyAdapter>,
        path: PathBuf,
        event_tx: async_channel::Sender<TaskEvent>,
        cancel_token: CancellationToken,
    ) -> Result<()> {
        let token = cancel_token.clone();
        let rt = self.rt.clone();
        let handle = self
            .rt
            .spawn_with_handle(async move {
                let mut context = Context::default();
                let mut state_machine = ConcurrentTaskInner::new(path, event_tx, rt)
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

                while let Some(_) = context.poll.pop_front() {
                    state_machine
                        .handle_with_context(&Event::Step, &mut context)
                        .await;
                }

                debug_assert!(
                    matches!(state_machine.state(), State::Stopped { .. }),
                    "the task should be stopped after the state machine is finished"
                );
            })
            .expect("Runtime is dropped");
        self.task = Some(TaskControl::new(token, handle));
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
    /// the chunk planner of this task
    chunk_planner: ChunkPlanner,
    progress: Progress,
    event_tx: async_channel::Sender<TaskEvent>,
}

#[derive(Default)]
struct Context {
    poll: VecDeque<()>,
}

enum Event {
    Run(RunningPayload),
    Step,
}

struct RunnerChunk {
    occupied: Range<u64>,
    downloaded: Range<u64>,
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
enum RunnerStatus {
    Running,
    Finished,
}

struct RunnerState {
    status: RunnerStatus,
    chunk: RunnerChunk,
    handle: RemoteHandle<()>,
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
            chunk_planner: ChunkPlanner::new(0),
            progress: Progress::default(),
            event_tx,
        }
    }
    /// Retrieve the meta data of the file
    async fn retrieve_meta(&mut self) -> Result<()> {
        // TODO: use backon to retry
        let meta = self
            .adapter
            .as_ref()
            .unwrap()
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
        }
        Ok(())
    }

    fn create_file_writer(&self) -> (PathBuf, FileWriter) {
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
        let file_writer = FileWriter::new(tmp_path.as_ref(), self.progress.total.unwrap());
        (tmp_path.into_owned(), file_writer)
    }

    async fn create_background_range_runner(
        rt: &ThreadedRuntimeImpl,
        range: Range<u64>,
        adapter: Arc<AnyAdapter>,
        notify: Receiver<ManagerMessage>,
        runner_id: RunnerId,
        cancel_token: CancellationToken,
    ) -> Result<(Receiver<RunnerMessage>, RemoteHandle<()>)> {
        let (tx, rx) = oneshot::channel();
        let handle = rt
            .spawn_with_handle(async move {
                let mut runner = match TaskRunner::new_with_async_and_callback(
                    Some(range.end - range.start),
                    async { adapter.range_stream(range.start, range.end).await },
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
            })
            .map_err(|e| TaskInstanceError::Failed(TaskFailedKind::Other(e.to_string())))?;
        let rx = rx
            .await
            .map_err(|_| TaskInstanceError::Failed(TaskFailedKind::Cancelled))?;
        Ok((rx, handle))
    }

    async fn download_timer_tick(
        chunk_planner: &mut ChunkPlanner,
        runners_state: &mut HashMap<RunnerId, RunnerState>,
        notification_subscriber: &mut RunnerNotification<'_, RunnerMessage>,
        dynamic_strategy: &mut DynamicStrategy,
        runner_id_generator: &mut Generator,
        max_concurrency: usize,
        total: u64,
        rt: &ThreadedRuntimeImpl,
        adapter: &Arc<AnyAdapter>,
        event_tx: &async_channel::Sender<TaskEvent>,
        control_tx: &Sender<ManagerMessage>,
        control_rx: &Receiver<ManagerMessage>,
        runners_cancel_token: &CancellationToken,
        meters: &mut HashMap<RunnerId, usize>,
        sampler: &Sampler,
    ) -> Result<()> {
        let runners_speed = meters
            .values_mut()
            .map(|r| sampler.sample(r))
            .collect::<Vec<_>>();
        let current_speed = runners_speed.iter().sum::<f64>();
        let avg_runner_speed = current_speed / runners_speed.len() as f64;

        let available_ranges = chunk_planner.get_available_ranges();
        // TODO: move it to a new strategy for error and concurrency control
        if !available_ranges.is_empty() {
            for chunk in available_ranges.iter() {
                let runner_id = runner_id_generator.next().unwrap();
                let (rx, handle) = Self::create_background_range_runner(
                    &rt,
                    chunk.clone(),
                    adapter.clone(),
                    control_rx.clone(),
                    runner_id,
                    runners_cancel_token.clone(),
                )
                .await?;
                runners_state.insert(
                    runner_id,
                    RunnerState {
                        status: RunnerStatus::Running,
                        chunk: RunnerChunk {
                            occupied: chunk.clone(),
                            downloaded: chunk.start..chunk.start,
                        },
                        handle,
                    },
                );
                notification_subscriber.add(runner_id, rx);
            }
        }
        let running_runners = runners_state
            .iter()
            .filter(|(_, r)| r.status == RunnerStatus::Running)
            .collect::<Vec<_>>();
        let (remaining_largest_runner, _) = running_runners
            .iter()
            .max_by_key(|(_, r)| r.chunk.occupied.end - r.chunk.downloaded.end)
            .expect("no runner is running");
        if running_runners.len() < max_concurrency
            && (running_runners.is_empty() || available_ranges.is_empty())
        {
            let strategy_context = DynamicStrategyContext {
                speed: current_speed,
                per_runner_speed: avg_runner_speed,
                current_concurrency: runners_speed.len(),
                remaining_largest_runner: **remaining_largest_runner,
            };
            let actions = dynamic_strategy.step(&strategy_context);
            for action in actions {
                match action {
                    StrategyAction::SplitAllTask if runners_state.len() == 0 => {
                        let runner_id = runner_id_generator.next().unwrap();
                        let chunk = 0..total;
                        let (rx, handle) = Self::create_background_range_runner(
                            &rt,
                            chunk.clone(),
                            adapter.clone(),
                            control_rx.clone(),
                            runner_id,
                            runners_cancel_token.clone(),
                        )
                        .await?;

                        runners_state.insert(
                            runner_id,
                            RunnerState {
                                status: RunnerStatus::Running,
                                chunk: RunnerChunk {
                                    occupied: chunk,
                                    downloaded: 0..0,
                                },
                                handle,
                            },
                        );
                        notification_subscriber.add(runner_id, rx);

                        // read next notification
                        let RunnerMessage(_, msg) = notification_subscriber
                            .next()
                            .await
                            .expect("Runner maybe corrupt?");
                        if let RunnerMessageKind::Stopped(StoppedReason::Failed(kind)) = msg {
                            return Err(TaskInstanceError::Failed(kind));
                        }
                    }
                    StrategyAction::SplitGivenTask(task_id) => {
                        let state = (*runners_state)
                            .get_mut(&task_id)
                            .expect("task id not found");
                        let occupied_chunk = state.chunk.occupied.clone();
                        let half_end = state.chunk.downloaded.end.max(
                            occupied_chunk.start + (occupied_chunk.end - occupied_chunk.start) / 2,
                        );
                        let chunk_length = half_end - occupied_chunk.start;
                        if chunk_length <= chunk_planner.min_chunk_size {
                            continue;
                        }

                        let next_id = runner_id_generator.next().unwrap();
                        let next_chunk = half_end..occupied_chunk.end;
                        chunk_planner.remove_chunk(occupied_chunk.clone());
                        chunk_planner.add_chunk(occupied_chunk.start..half_end, Some(task_id));
                        chunk_planner.add_chunk(half_end..occupied_chunk.end, Some(next_id));
                        control_tx.send(ManagerMessage(
                            task_id,
                            ManagerMessagesVariant::ResizeTotal(half_end),
                        ));

                        drop(state);

                        // TODO: retry with backoff
                        let (rx, handle) = Self::create_background_range_runner(
                            &rt,
                            next_chunk.clone(),
                            adapter.clone(),
                            control_rx.clone(),
                            next_id,
                            runners_cancel_token.clone(),
                        )
                        .await
                        .expect("Should not drop oneshot here");
                        runners_state.insert(
                            next_id,
                            RunnerState {
                                status: RunnerStatus::Running,
                                chunk: RunnerChunk {
                                    occupied: next_chunk.clone(),
                                    downloaded: next_chunk.start..next_chunk.start,
                                },
                                handle,
                            },
                        );
                        notification_subscriber.add(next_id, rx);
                    }
                    _ => {}
                }
            }
        }

        let mut downloaded = 0;
        let downloaded_chunks = chunk_planner
            .iter_chunks()
            .map(|(r, task_id)| {
                let range = match task_id {
                    Some(task_id) => runners_state
                        .get(&task_id)
                        .map(|r| r.chunk.downloaded.clone())
                        .unwrap_or(r.clone()),
                    None => r.clone(),
                };
                downloaded += range.end - range.start;
                range
            })
            .collect::<Vec<_>>();
        let event_tx = event_tx.clone();
        let _ = rt.spawn(async move {
            event_tx
                .send(TaskEvent::Downloading(ProgressWithSpeed::new(
                    Progress {
                        total: Some(total),
                        downloaded,
                        downloaded_chunks,
                    },
                    current_speed,
                )))
                .await;
        });
        Ok(())
    }

    async fn handle_runner_message(
        runners_state: &mut HashMap<RunnerId, RunnerState>,
        notification_subscriber: &mut RunnerNotification<'_, RunnerMessage>,
        chunk_planner: &mut ChunkPlanner,
        id_generator: &mut Generator,
        file_writer_control: &FileWriterControl,
        on_all_chunks_finished: impl FnOnce(),
        msg: RunnerMessage,
    ) -> Result<()> {
        let RunnerMessage(runner_id, msg) = msg;
        match msg {
            RunnerMessageKind::Stopped(reason) => match reason {
                StoppedReason::Finished => {
                    log::trace!("runner {} finished", runner_id);
                    runners_state
                        .entry(runner_id)
                        .and_modify(|r| r.status = RunnerStatus::Finished);
                    if runners_state
                        .values()
                        .all(|r| r.status == RunnerStatus::Finished)
                        && chunk_planner.get_available_ranges().is_empty()
                    {
                        on_all_chunks_finished();
                    }
                }
                StoppedReason::Failed(_kind) => {
                    if let Some(state) = runners_state.remove(&runner_id) {
                        notification_subscriber.remove(runner_id);
                        chunk_planner.remove_chunk(state.chunk.occupied.clone());
                        chunk_planner.add_chunk(state.chunk.occupied, None);
                        id_generator.release(runner_id);
                        // TODO: if unretryable, decrease the max concurrency
                    }
                }
            },
            RunnerMessageKind::Downloaded(bytes) => {
                if let Some(state) = runners_state.get_mut(&runner_id) {
                    let previous_downloaded_pos = state.chunk.downloaded.end;
                    state.chunk.downloaded.end += bytes.len() as u64;
                    debug_assert!(state.chunk.downloaded.end <= state.chunk.occupied.end);

                    // TODO: background write to file, avoid blocking the main thread
                    file_writer_control
                        .write(previous_downloaded_pos..state.chunk.downloaded.end, bytes)
                        .await
                        .map_err(|e| {
                            TaskInstanceError::Failed(TaskFailedKind::Other(e.to_string()))
                        })?;
                }
            }
            RunnerMessageKind::Started => {
                log::trace!("runner {} started", runner_id);
            }
        }
        Ok(())
    }

    /// Sync the runner chunks to the chunk planner
    ///
    /// It should called before the downloading returned
    fn sync_runner_chunks_and_chunk_planner(
        &mut self,
        runners_state: &HashMap<RunnerId, RunnerState>,
        chunk_planner: &mut ChunkPlanner,
    ) {
        for (runner_id, state) in runners_state.iter() {
            if state.chunk.downloaded != state.chunk.occupied {
                if chunk_planner.remove_chunk(state.chunk.occupied.clone()) {
                    chunk_planner.add_chunk(state.chunk.occupied.clone(), Some(*runner_id));
                }
            }
        }
        self.progress.downloaded_chunks = chunk_planner.get_occupied_ranges();
        self.progress.downloaded = self
            .progress
            .downloaded_chunks
            .iter()
            .map(|r| r.end - r.start)
            .sum();
    }

    async fn download(&mut self, cancel_token: &CancellationToken) -> Result<()> {
        let total = self
            .progress
            .total
            .expect("A concurrent task should have a total size");
        let adapter = self.adapter.clone().unwrap();
        let rt = self.rt.clone();
        let event_tx = self.event_tx.clone();
        let runners_cancel_token = cancel_token.child_token();
        let _runner_cancel_guard = runners_cancel_token.clone().drop_guard();

        // The initial max concurrency is the number of available threads
        let initial_max_concurrency = std::thread::available_parallelism()
            .map(|t| t.get())
            .unwrap_or(DEFAULT_MAX_CONCURRENCY);
        // The max concurrency should follows 1 <= max_concurrency <= initial_max_concurrency
        let mut max_concurrency = initial_max_concurrency;

        let mut runner_id_generator = Generator::new(initial_max_concurrency);
        let mut chunk_planner = ChunkPlanner::new(total);

        // A temporary map to store the state of each runner
        // When a runner was released, the chunk state should sync back to the chunk planner
        let mut runners_state: HashMap<RunnerId, RunnerState> =
            HashMap::with_capacity(initial_max_concurrency);
        let (control_tx, control_rx) = async_channel::unbounded();
        let mut notification_subscriber = RunnerNotification::default();
        // futures::pin_mut!(notification_subscriber, runners_state, chunk_planner);

        let mut dynamic_strategy =
            DynamicStrategy::new_with_max_concurrency(initial_max_concurrency);
        let mut meters: HashMap<RunnerId, usize> = HashMap::with_capacity(initial_max_concurrency);
        let sampler = Sampler::default();
        let (tmp_path, file_writer) = self.create_file_writer();
        let FileWriterGuard(_, file_writer_control) = file_writer.start();

        let mut timer = Timer::interval(Duration::from_secs(SAMPLE_INTERVAL));

        loop {
            let mut notification = notification_subscriber.next().fuse();

            futures::select_biased! {
                _ = timer.next().fuse() => {
                    Self::download_timer_tick(
                        &mut chunk_planner,
                        &mut runners_state,
                        &mut notification_subscriber,
                        &mut dynamic_strategy,
                        &mut runner_id_generator,
                        max_concurrency,
                        total,
                        &rt,
                        &adapter,
                        &event_tx,
                        &control_tx,
                        &control_rx,
                        &runners_cancel_token,
                        &mut meters,
                        &sampler,
                    ).await.inspect_err(|_| {
                        self.sync_runner_chunks_and_chunk_planner(&runners_state, &mut chunk_planner);
                    })?;
                }
                msg = notification => {
                    if let Some(msg) = msg {
                        let mut flag = false;
                        Self::handle_runner_message(
                            &mut runners_state,
                            &mut notification_subscriber,
                            &mut chunk_planner,
                            &mut runner_id_generator,
                            &file_writer_control,
                            || {
                                flag = true;
                            },
                            msg,
                        )
                        .await
                        .inspect_err(|_| {
                            self.sync_runner_chunks_and_chunk_planner(&runners_state, &mut chunk_planner);
                        })?;
                        if flag {
                            break;
                        }
                    }
                }
            }
        }
        self.sync_runner_chunks_and_chunk_planner(&runners_state, &mut chunk_planner);
        drop(file_writer_control);
        // TODO: send a flush all message to file writer
        async_fs::rename(&tmp_path, &self.path)
            .await
            .map_err(|e| TaskInstanceError::Failed(TaskFailedKind::Other(e.to_string())))?;
        Ok(())
    }
}

#[state_machine(
    initial = "State::stopped(None)",
    on_transition = "Self::on_transition"
)]
impl ConcurrentTaskInner {
    #[state]
    fn stopped(
        &mut self,
        context: &mut Context,
        reason: &mut Option<Result<()>>,
        event: &Event,
    ) -> Response<State> {
        match event {
            Event::Run(payload) => {
                context.poll.push_back(());
                Transition(State::initializing(payload.cancel_token.clone()))
            }
            _ => Super,
        }
    }

    #[superstate]
    async fn running(event: &Event) -> Response<State> {
        Super
    }

    #[state(superstate = "running")]
    async fn initializing(
        &mut self,
        cancel_token: &mut CancellationToken,
        context: &mut Context,
        event: &Event,
    ) -> Response<State> {
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

    #[state(superstate = "running")]
    async fn downloading(
        &mut self,
        cancel_token: &mut CancellationToken,
        event: &Event,
    ) -> Response<State> {
        match event {
            Event::Step => {
                let task = async {
                    match self.download(cancel_token).await {
                        Ok(_) => Transition(State::stopped(None)),
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

    fn on_transition(&mut self, _source: &State, target: &State) {
        match target {
            State::Stopped { reason } => match reason.clone() {
                Some(Ok(())) => {
                    let tx = self.event_tx.clone();
                    let progress = self.progress.clone();
                    self.rt.spawn(async move {
                        tx.send(TaskEvent::Finished(progress)).await;
                    });
                }
                Some(Err(e)) => {
                    let tx = self.event_tx.clone();
                    self.rt.spawn(async move {
                        tx.send(TaskEvent::Failed(e)).await;
                    });
                }
                None => unreachable!(),
            },
            State::Initializing { .. } => {
                let tx = self.event_tx.clone();
                self.rt.spawn(async move {
                    tx.send(TaskEvent::Initializing).await;
                });
            }
            State::Downloading { .. } => {
                let tx = self.event_tx.clone();
                let progress = self.progress.clone();
                self.rt.spawn(async move {
                    tx.send(TaskEvent::Downloading(ProgressWithSpeed::new(
                        progress, 0.0,
                    )))
                    .await;
                });
            }
        }
    }
}
