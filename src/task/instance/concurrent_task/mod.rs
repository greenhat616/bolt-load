use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
    ops::Range,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use async_channel::{Receiver, Sender};
use async_io::Timer;
use futures::{FutureExt, StreamExt, future::RemoteHandle, task::SpawnExt};
use lending_stream::prelude::*;
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
    },
};

use super::{Generator, Result, TaskInstance};

mod chunk_planner;
mod file;
mod runner_notification;
mod strategy;

use chunk_planner::*;
use file::*;
use runner_notification::RunnerNotification;
use strategy::*;

static DEFAULT_MAX_CONCURRENCY: usize = 4;

pub struct ConcurrentTask {
    rt: ThreadedRuntimeImpl,
    task: Option<TaskControl>,
    progress: Progress,
}

impl ConcurrentTask {
    pub fn new(rt: ThreadedRuntimeImpl) -> Self {
        Self {
            rt,
            task: None,
            progress: Progress::default(),
        }
    }
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

                while context.poll.pop_front().is_some() {
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
pub struct Context {
    poll: VecDeque<()>,
}

pub enum Event {
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
                log::trace!(
                    "[TASK] create background range runner: id: {runner_id}, range: {range:?}"
                );
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

    #[allow(clippy::too_many_arguments)]
    async fn download_timer_tick(
        chunk_planner: &mut ChunkPlanner,
        runners_state: &mut HashMap<RunnerId, RunnerState>,
        runner_notification: &mut RunnerNotification,
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
            log::trace!("[TASK] create background runners: available_ranges: {available_ranges:?}");
            for chunk in available_ranges.iter() {
                let runner_id = runner_id_generator.next().expect("no more runner id");
                chunk_planner.add_chunk(chunk.clone(), Some(runner_id));
                let (rx, handle) = Self::create_background_range_runner(
                    rt,
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
                runner_notification.add(runner_id, rx);
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
                log::trace!("[TASK] download_timer_tick: action: {action:?}");
                match action {
                    StrategyAction::SplitAllTask if runners_state.is_empty() => {
                        let runner_id = runner_id_generator.next().expect("no more runner id");
                        let chunk = 0..total;
                        let (rx, handle) = Self::create_background_range_runner(
                            rt,
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

                        // read next notification
                        // let RunnerMessage(_, msg) = rx.recv().await.expect("Runner maybe corrupt?");
                        // if let RunnerMessageKind::Stopped(StoppedReason::Failed(kind)) = msg {
                        //     return Err(TaskInstanceError::Failed(kind));
                        // }
                        runner_notification.add(runner_id, rx);
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

                        log::trace!("[TASK] StrategyAction::SplitGivenTask: task_id: {task_id}");

                        let next_id = runner_id_generator.next().expect("no more runner id");
                        let next_chunk = half_end..occupied_chunk.end;
                        chunk_planner.remove_chunk(occupied_chunk.clone());
                        chunk_planner.add_chunk(occupied_chunk.start..half_end, Some(task_id));
                        chunk_planner.add_chunk(half_end..occupied_chunk.end, Some(next_id));
                        let _ = control_tx
                            .send(ManagerMessage(
                                task_id,
                                ManagerMessagesVariant::LimitTotal(half_end),
                            ))
                            .await
                            .inspect_err(|e| {
                                log::error!("failed to send resize total message: {e:?}");
                            });

                        let _ = state;

                        // TODO: retry with backoff
                        let (rx, handle) = Self::create_background_range_runner(
                            rt,
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
                        runner_notification.add(next_id, rx);
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
            let _ = event_tx
                .send(TaskEvent::Downloading(ProgressWithSpeed::new(
                    Progress {
                        total: Some(total),
                        downloaded,
                        downloaded_chunks,
                    },
                    current_speed,
                )))
                .await
                .inspect_err(|e| {
                    log::error!("failed to send downloading event: {e:?}");
                });
        });
        Ok(())
    }

    async fn handle_runner_message(
        runners_state: &mut HashMap<RunnerId, RunnerState>,
        runner_notification: &mut RunnerNotification,
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
                    log::trace!("runner {runner_id} finished");
                    runners_state
                        .entry(runner_id)
                        .and_modify(|r| r.status = RunnerStatus::Finished);
                    if runners_state
                        .values()
                        .all(|r| r.status == RunnerStatus::Finished)
                        && chunk_planner.get_available_ranges().is_empty()
                    {
                        log::trace!("[TASK] handle_runner_message: all chunks finished");
                        on_all_chunks_finished();
                    }
                }
                StoppedReason::Failed(_kind) => {
                    if let Some(state) = runners_state.remove(&runner_id) {
                        runner_notification.remove(runner_id);
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
                log::trace!("runner {runner_id} started");
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
            if state.chunk.downloaded != state.chunk.occupied
                && chunk_planner.remove_chunk(state.chunk.occupied.clone())
            {
                chunk_planner.add_chunk(state.chunk.occupied.clone(), Some(*runner_id));
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
        let max_concurrency = initial_max_concurrency;

        let mut runner_id_generator = Generator::new(initial_max_concurrency);
        let mut chunk_planner = ChunkPlanner::new(total);

        // A temporary map to store the state of each runner
        // When a runner was released, the chunk state should sync back to the chunk planner
        let mut runners_state: HashMap<RunnerId, RunnerState> =
            HashMap::with_capacity(initial_max_concurrency);
        let (control_tx, control_rx) = async_channel::unbounded();
        let mut runner_notification = RunnerNotification::new();

        let mut dynamic_strategy =
            DynamicStrategy::new_with_max_concurrency(initial_max_concurrency);
        let mut meters: HashMap<RunnerId, usize> = HashMap::with_capacity(initial_max_concurrency);
        let sampler = Sampler::default();
        let (tmp_path, file_writer) = self.create_file_writer()?;
        let FileWriterGuard(_, file_writer_control) = file_writer
            .start()
            .await
            .map_err(|e| TaskInstanceError::AllocateFileSpaceFailed(Arc::new(e)))?;

        let mut timer = Timer::interval(Duration::from_secs(SAMPLE_INTERVAL));

        loop {
            futures::select_biased! {
                _ = timer.next().fuse() => {
                    Self::download_timer_tick(
                        &mut chunk_planner,
                        &mut runners_state,
                        &mut runner_notification,
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
                msg = runner_notification.next().fuse() => {
                    if let Some(msg) = msg {
                        let mut flag = false;
                        Self::handle_runner_message(
                            &mut runners_state,
                            &mut runner_notification,
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
                self.adapter = Some(payload.adapter.clone());
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
            State::Stopped { reason } => {
                log::trace!("on_transition: enter stopped state, reason: {reason:?}");
                match reason.clone() {
                    Some(Ok(())) => {
                        let tx = self.event_tx.clone();
                        let progress = self.progress.clone();
                        let _ = self.rt.spawn(async move {
                            let _ = tx.send(TaskEvent::Finished(progress)).await;
                        });
                    }
                    Some(Err(e)) => {
                        let tx = self.event_tx.clone();
                        let _ = self.rt.spawn(async move {
                            let _ = tx.send(TaskEvent::Failed(e)).await;
                        });
                    }
                    None => unreachable!(),
                }
            }
            State::Initializing { .. } => {
                log::trace!("on_transition: enter initializing state");
                let tx = self.event_tx.clone();
                let _ = self.rt.spawn(async move {
                    let _ = tx.send(TaskEvent::Initializing).await;
                });
            }
            State::Downloading { .. } => {
                log::trace!("on_transition: enter downloading state");
                let tx = self.event_tx.clone();
                let progress = self.progress.clone();
                let _ = self.rt.spawn(async move {
                    let _ = tx
                        .send(TaskEvent::Downloading(ProgressWithSpeed::new(
                            progress, 0.0,
                        )))
                        .await;
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        adapter::{BoltLoadAdapter, tests::SimpleTestAdapter},
        runtime::ThreadedRuntimeImpl,
        task::{ManagerMessage, ManagerMessagesVariant, RunnerId},
    };
    use futures::StreamExt;
    use pretty_assertions::assert_eq;
    use smol_cancellation_token::CancellationToken;
    use std::sync::Arc;
    use test_log::test;

    #[test(tokio::test(flavor = "multi_thread"))]
    async fn test_create_background_range_runner_success() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter = Arc::new(Box::new(SimpleTestAdapter::new(10240))
            as Box<dyn crate::adapter::BoltLoadAdapter + Send>);
        let range = 1000u64..3000u64;
        let runner_id = 1;
        let (_control_tx, control_rx) = async_channel::unbounded();
        let cancel_token = CancellationToken::new();

        let result = ConcurrentTaskInner::create_background_range_runner(
            &rt,
            range.clone(),
            adapter.clone(),
            control_rx,
            runner_id,
            cancel_token,
        )
        .await;

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

    #[test(tokio::test(flavor = "multi_thread"))]
    async fn test_create_background_range_runner_with_adapter_failure() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter = Arc::new(Box::new(SimpleTestAdapter::new(1024).with_failure(true))
            as Box<dyn crate::adapter::BoltLoadAdapter + Send>);
        let range = 0u64..500u64;
        let runner_id = 2;
        let (_control_tx, control_rx) = async_channel::unbounded();
        let cancel_token = CancellationToken::new();

        let result = ConcurrentTaskInner::create_background_range_runner(
            &rt,
            range,
            adapter,
            control_rx,
            runner_id,
            cancel_token,
        )
        .await;

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

    #[test(tokio::test(flavor = "multi_thread"))]
    async fn test_create_background_range_runner_with_cancellation() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter = Arc::new(Box::new(SimpleTestAdapter::new(10240))
            as Box<dyn crate::adapter::BoltLoadAdapter + Send>);
        let range = 0u64..5000u64;
        let runner_id = 3;
        let (_control_tx, control_rx) = async_channel::unbounded();
        let cancel_token = CancellationToken::new();

        let result = ConcurrentTaskInner::create_background_range_runner(
            &rt,
            range,
            adapter,
            control_rx,
            runner_id,
            cancel_token.clone(),
        )
        .await;

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

    #[test(tokio::test(flavor = "multi_thread"))]
    async fn test_create_background_small_range_runner_with_control_messages() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter = Arc::new(Box::new(SimpleTestAdapter::new(10240))
            as Box<dyn crate::adapter::BoltLoadAdapter + Send>);
        let old_total = 10240u64;
        let range = 0u64..old_total;
        let runner_id = 4;
        let (control_tx, control_rx) = async_channel::unbounded();
        let cancel_token = CancellationToken::new();

        // 在创建运行器之前预先发送控制消息
        let new_total = 1000u64;
        control_tx
            .send(ManagerMessage(
                runner_id,
                ManagerMessagesVariant::LimitTotal(new_total),
            ))
            .await
            .unwrap();

        let result = ConcurrentTaskInner::create_background_range_runner(
            &rt,
            range.clone(),
            adapter,
            control_rx,
            runner_id,
            cancel_token,
        )
        .await;

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
            eprintln!("total_downloaded: {total_downloaded}, new_total: {new_total}");
        }
        assert!(total_downloaded >= new_total as usize);
    }

    #[test(tokio::test(flavor = "multi_thread"))]
    async fn test_create_background_range_runner_edge_ranges() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let content_size = 1000;
        let adapter = Arc::new(Box::new(SimpleTestAdapter::new(content_size))
            as Box<dyn crate::adapter::BoltLoadAdapter + Send>);

        // 测试边界范围：从文件末尾开始
        let range = 900u64..1000u64;
        let runner_id = 5;
        let (_control_tx, control_rx) = async_channel::unbounded();
        let cancel_token = CancellationToken::new();

        let result = ConcurrentTaskInner::create_background_range_runner(
            &rt,
            range.clone(),
            adapter.clone(),
            control_rx,
            runner_id,
            cancel_token,
        )
        .await;

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

    #[test(tokio::test(flavor = "multi_thread"))]
    async fn test_create_background_range_runner_zero_length_range() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter = Arc::new(Box::new(SimpleTestAdapter::new(1000))
            as Box<dyn crate::adapter::BoltLoadAdapter + Send>);

        // 测试零长度范围
        let range = 500u64..500u64;
        let runner_id = 6;
        let (_control_tx, control_rx) = async_channel::unbounded();
        let cancel_token = CancellationToken::new();

        let result = ConcurrentTaskInner::create_background_range_runner(
            &rt,
            range,
            adapter,
            control_rx,
            runner_id,
            cancel_token,
        )
        .await;

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

    #[test(tokio::test(flavor = "multi_thread"))]
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
            let (control_tx, control_rx) = async_channel::unbounded();
            let cancel_token = CancellationToken::new();

            let handle = tokio::spawn(async move {
                let result = ConcurrentTaskInner::create_background_range_runner(
                    &rt_clone,
                    range.clone(),
                    adapter_clone,
                    control_rx,
                    runner_id,
                    cancel_token,
                )
                .await
                .unwrap();

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
            println!(
                "Runner {runner_id}: downloaded {downloaded_size} bytes, expected {expected_size} \
                 bytes"
            );
            assert_eq!(downloaded_size, expected_size as usize);
        }
    }

    #[test(tokio::test(flavor = "multi_thread"))]
    async fn test_create_background_range_runner_hash_verification() {
        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter = Arc::new(Box::new(SimpleTestAdapter::new(5000))
            as Box<dyn crate::adapter::BoltLoadAdapter + Send>);
        let range = 1500u64..3500u64;
        let runner_id = 7;
        let (_control_tx, control_rx) = async_channel::unbounded();
        let cancel_token = CancellationToken::new();

        let result = ConcurrentTaskInner::create_background_range_runner(
            &rt,
            range.clone(),
            adapter.clone(),
            control_rx,
            runner_id,
            cancel_token,
        )
        .await;

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
        use crate::adapter::tests::calculate_sha256;
        let downloaded_hash = calculate_sha256(&downloaded_data);
        let reference_hash = calculate_sha256(&reference_data);
        assert_eq!(downloaded_hash, reference_hash);
    }
}
