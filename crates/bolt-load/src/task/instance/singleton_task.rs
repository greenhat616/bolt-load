use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_fs::{File, OpenOptions};
use async_io::Timer;
use bolt_load_utils::telemetry::*;
use futures::{AsyncWriteExt, FutureExt, StreamExt};
use smol_cancellation_token::CancellationToken;
use statig::prelude::*;

use super::{
    Result, TaskInstance, TaskInstanceError, output_file,
    sampler::{DEFAULT_SAMPLE_INTERVAL, SpeedSampler},
};
use crate::{
    adapter::AnyAdapter,
    runner::{LifecycleEvent, StoppedReason, TaskError, TaskRunner},
    runtime::{LocalRuntimeBuilderImpl, ThreadedRuntimeExt, ThreadedRuntimeImpl},
    task::{
        Progress, RunnerId, TaskOptions,
        instance::{ProgressWithSpeed, RunningPayload, TaskControl, TaskEvent},
    },
};

/// Singleton task only have one runner, so we use a static id for the runner
const STATIC_RUNNER_ID: RunnerId = 0;

const CONTROL_CHANNEL_CAPACITY: usize = 8;

pub struct SingletonTask {
    threaded_rt: ThreadedRuntimeImpl,
    local_runtime_builder: Option<LocalRuntimeBuilderImpl>,
    task: Option<TaskControl>,
    options: TaskOptions,
}

impl SingletonTask {
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
            options,
        }
    }
}

impl TaskInstance for SingletonTask {
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, name = "SingletonTask::run")
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
        let options = self.options.clone();
        trace!("[SINGLETON TASK] Attempting to spawn state machine task");
        let task = ::blocking::unblock(move || {
            let local_rt = threaded_rt
                .downcast_local(local_rt_builder)
                .expect("failed to downcast the runtime");
            let task = async move {
                trace!("[SINGLETON TASK] State machine starting");
                let mut context = Context::default();
                let mut state_machine = SingletonTaskInner::new(path, event_tx, options)
                    .uninitialized_state_machine()
                    .init_with_context(&mut context)
                    .await;

                trace!("[SINGLETON TASK] State machine initialized, handling Run event");
                state_machine
                    .handle_with_context(
                        &Event::Run(RunningPayload {
                            adapter,
                            cancel_token,
                        }),
                        &mut context,
                    )
                    .await;

                trace!(
                    "[SINGLETON TASK] Run event handled, starting event loop with {} items in poll",
                    context.poll.len()
                );
                while context.poll.pop_front().is_some() {
                    trace!("[SINGLETON TASK] Processing Step event");
                    state_machine
                        .handle_with_context(&Event::Step, &mut context)
                        .await;
                    trace!(
                        "[SINGLETON TASK] Step event handled, {} items remaining in poll",
                        context.poll.len()
                    );
                }
                trace!("[SINGLETON TASK] Event loop completed");

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
                    "SingletonTask::background_task",
                ),
            );
            local_rt.block_on(Box::pin(task))
        });
        trace!("[SINGLETON TASK] State machine task spawned successfully");
        self.task = Some(TaskControl::new(token, task));
        trace!("[SINGLETON TASK] TaskControl created and stored");
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(task) = self.task.as_mut() {
            task.stop().await;
        }
        Ok(())
    }

    async fn wait(&mut self) -> Result<()> {
        trace!(
            "[SINGLETON TASK] wait() called, task present: {}",
            self.task.is_some()
        );
        if let Some(task) = self.task.as_mut() {
            trace!("[SINGLETON TASK] waiting for task to complete");
            task.wait().await;
            trace!("[SINGLETON TASK] task completed");
        }
        Ok(())
    }
}

struct SingletonTaskInner {
    total: Option<u64>,
    downloaded: u64,
    path: PathBuf,
    adapter: Option<Arc<AnyAdapter>>,
    event_tx: async_channel::Sender<TaskEvent>,
    options: TaskOptions,
}

impl SingletonTaskInner {
    fn new(
        path: PathBuf,
        event_tx: async_channel::Sender<TaskEvent>,
        options: TaskOptions,
    ) -> Self {
        Self {
            total: None,
            downloaded: 0,
            path,
            adapter: None,
            event_tx,
            options,
        }
    }
}

impl SingletonTaskInner {
    /// Retrieve the meta data of the file
    async fn retrieve_meta(&mut self) -> Result<()> {
        // TODO: use backon to retry
        let meta = self
            .adapter
            .as_ref()
            .unwrap()
            .retrieve_meta()
            .await
            .map_err(TaskInstanceError::new_retrieve_meta_failed)?;
        let total = if meta.content_size == 0 {
            None
        } else {
            Some(meta.content_size)
        };
        self.total = total;
        self.downloaded = 0;
        Ok(())
    }

    async fn handle_lifecycle_event(
        &mut self,
        event: LifecycleEvent,
        is_finished: &mut bool,
    ) -> Result<()> {
        match event {
            LifecycleEvent::Stopped(reason) => match reason {
                StoppedReason::Finished => {
                    *is_finished = true;
                    return Ok(());
                }
                StoppedReason::Failed(kind) => return Err(TaskInstanceError::new_failed(kind)),
            },
            LifecycleEvent::Started => {}
        }
        Ok(())
    }

    async fn handle_data_frame(
        &mut self,
        data: bytes::Bytes,
        file: &mut File,
        meter: &mut usize,
    ) -> Result<()> {
        trace!("[SINGLETON TASK] downloaded chunk: {:?}", data.len());
        *meter += data.len();
        self.downloaded += data.len() as u64;
        file.write_all(&data)
            .await
            .map_err(TaskInstanceError::new_write_chunk_failed)?;
        Ok(())
    }

    /// Publish a periodic progress sample; it is dropped when the channel is full.
    #[allow(clippy::single_range_in_vec_init)]
    fn handle_timer_tick(&self, sampler: &mut SpeedSampler, meter: &mut usize) {
        let (_, ema_speed) = sampler.sample(meter);
        let progress = Progress {
            total: self.total,
            downloaded: self.downloaded,
            downloaded_chunks: [0..self.downloaded].to_vec(),
        };
        let _ = self
            .event_tx
            .try_send(TaskEvent::Downloading(ProgressWithSpeed::new(
                progress, ema_speed, 0.0,
            )));
    }

    /// Download into the temporary file, then publish it or discard it.
    async fn download(&mut self, cancel_token: &CancellationToken) -> Result<()> {
        trace!(
            "[SINGLETON TASK] download() method called, path: {:?}",
            self.path
        );
        let tmp_path = output_file::tmp_path_for(&self.path);
        let transfer = self.download_to(&tmp_path, cancel_token).await;
        output_file::commit(transfer, &tmp_path, &self.path, cancel_token).await
    }

    /// Run the transfer; returns only after the runner has exited.
    async fn download_to(
        &mut self,
        tmp_path: &Path,
        cancel_token: &CancellationToken,
    ) -> Result<()> {
        // A child token: finishing or failing must never cancel the caller's token.
        let runner_token = cancel_token.child_token();
        let _runner_cancel_guard = runner_token.clone().drop_guard();

        let stream = {
            let adapter = self.adapter.clone().unwrap();
            let connect = async move { adapter.full_stream().await }.fuse();
            let cancelled = runner_token.cancelled().fuse();
            let timeout = FutureExt::fuse(Timer::after(self.options.no_progress_timeout));
            futures::pin_mut!(connect, cancelled, timeout);
            futures::select_biased! {
                _ = cancelled => {
                    return Err(TaskInstanceError::new_failed(TaskError::Cancelled));
                }
                _ = timeout => {
                    return Err(TaskInstanceError::new_failed(TaskError::NoProgress {
                        timeout: self.options.no_progress_timeout,
                    }));
                }
                res = connect => res.map_err(TaskInstanceError::new_stream_failed)?,
            }
        };
        trace!("[SINGLETON TASK] stream obtained successfully");

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(tmp_path)
            .await
            .map_err(TaskInstanceError::new_write_chunk_failed)?;

        if let Some(total) = self.total {
            file.set_len(total)
                .await
                .map_err(TaskInstanceError::new_write_chunk_failed)?;
        }

        // The runner fails with `ChannelClosed` once every control sender is
        // gone, so the sender must outlive it.
        let (control_tx, control_rx) = async_channel::bounded(CONTROL_CHANNEL_CAPACITY);

        // Use builder pattern to create runner with split channels
        let mut builder = TaskRunner::builder()
            .stream(stream)
            .runner_id(STATIC_RUNNER_ID)
            .control_signal(control_rx)
            .cancel_token(runner_token.clone())
            .idle_timeout(self.options.stream_idle_timeout);

        if let Some(total) = self.total {
            builder = builder.total(total);
        }

        let (mut runner, lifecycle_rx, data_rx) =
            builder.build().expect("failed to build task runner");

        let mut meter = 0;
        let mut sampler = SpeedSampler::new();
        let mut timer = Timer::interval(DEFAULT_SAMPLE_INTERVAL);

        // The runner is driven inline; leaving this block drops it, which stops
        // it and releases the upstream stream.
        let result = async {
            let mut is_finished = false;
            let mut runner_stopped = false;
            let mut data_done = false;
            let mut lifecycle_done = false;
            let fut = runner.run().fuse();
            futures::pin_mut!(fut);
            futures::pin_mut!(lifecycle_rx);
            futures::pin_mut!(data_rx);
            loop {
                let data_next = if data_done {
                    futures::future::Either::Left(futures::future::pending())
                } else {
                    futures::future::Either::Right(data_rx.next())
                }
                .fuse();
                let lifecycle_next = if lifecycle_done {
                    futures::future::Either::Left(futures::future::pending())
                } else {
                    futures::future::Either::Right(lifecycle_rx.next())
                }
                .fuse();
                futures::pin_mut!(data_next, lifecycle_next);

                futures::select_biased! {
                    _ = timer.next().fuse() => self.handle_timer_tick(&mut sampler, &mut meter),
                    _ = fut => (),
                    frame = data_next => {
                        match frame {
                            Some(frame) => {
                                self.handle_data_frame(frame.data, &mut file, &mut meter).await?;
                            }
                            None => {
                                data_done = true;
                                if runner_stopped {
                                    break;
                                }
                            }
                        }
                    }
                    event = lifecycle_next => {
                        match event {
                            Some(event) => {
                                match self.handle_lifecycle_event(event, &mut is_finished).await {
                                    Ok(()) => {
                                        if is_finished {
                                            runner_stopped = true;
                                            if data_done {
                                                break;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        return Err(e);
                                    }
                                }
                            }
                            None => {
                                warn!("lifecycle stream ended unexpectedly");
                                lifecycle_done = true;
                                if !runner_stopped {
                                    return Err(TaskInstanceError::new_failed(
                                        TaskError::ChannelClosed,
                                    ));
                                }
                                if data_done {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            Ok(())
        }
        .await;

        // ---- teardown: never short-circuit ----
        runner_token.cancel();
        drop(runner);
        drop(control_tx);
        let flush = file
            .flush()
            .await
            .map_err(TaskInstanceError::new_write_chunk_failed);
        drop(file);

        result.and(flush)
    }

    async fn before_transition(&mut self, _source: &State, target: &State, _context: &mut Context) {
        match target {
            State::Stopped { reason } => match reason.clone() {
                Some(Ok(())) => {
                    let progress = Progress {
                        total: Some(self.total.unwrap_or(self.downloaded)),
                        downloaded: self.downloaded,
                        #[allow(clippy::single_range_in_vec_init)]
                        downloaded_chunks: vec![0..self.downloaded],
                    };
                    let _ = self.event_tx.send(TaskEvent::Finished(progress)).await;
                }
                Some(Err(e)) => {
                    let _ = self.event_tx.send(TaskEvent::Failed(e)).await;
                }
                None => unreachable!(),
            },
            State::Initializing { .. } => {
                let _ = self.event_tx.send(TaskEvent::Initializing).await;
            }
            State::Downloading { .. } => {
                let progress = Progress {
                    total: self.total,
                    downloaded: self.downloaded,
                    #[allow(clippy::single_range_in_vec_init)]
                    downloaded_chunks: vec![0..self.downloaded],
                };
                let _ = self
                    .event_tx
                    .send(TaskEvent::Downloading(ProgressWithSpeed::new(
                        progress, 0.0, 0.0,
                    )))
                    .await;
            }
        }
    }
}

enum Event {
    Run(RunningPayload),
    /// Step the state machine to get the next state
    Step,
}

#[derive(Default)]
struct Context {
    poll: VecDeque<()>,
}

#[state_machine(
    initial = "State::stopped(None)",
    state(derive(Debug)),
    superstate(derive(Debug)),
    before_transition = "Self::before_transition"
)]
#[allow(unused_variables)]
impl SingletonTaskInner {
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
    fn running(event: &Event) -> Outcome<State> {
        Super
    }

    #[state(superstate = "running")]
    async fn initializing(
        &mut self,
        cancel_token: &mut CancellationToken,
        context: &mut Context,
        event: &Event,
    ) -> Outcome<State> {
        trace!(
            "[STATE MACHINE] initializing state entered, event: {:?}",
            match event {
                Event::Step => "Step",
                Event::Run(_) => "Run",
            }
        );
        match event {
            Event::Step => {
                trace!("[STATE MACHINE] processing Step event in initializing");
                let task = async {
                    match self.retrieve_meta().await {
                        Ok(()) => {
                            trace!(
                                "[STATE MACHINE] retrieve_meta successful, transitioning to \
                                 downloading"
                            );
                            context.poll.push_back(());
                            Transition(State::downloading(cancel_token.clone()))
                        }
                        Err(e) => {
                            trace!("[STATE MACHINE] retrieve_meta failed: {:?}", e);
                            Transition(State::stopped(Some(Err(e))))
                        }
                    }
                }
                .fuse();
                futures::pin_mut!(task);

                futures::select_biased! {
                    _ = cancel_token.cancelled().fuse() => {
                        trace!("[STATE MACHINE] initializing cancelled");
                        Transition(State::stopped(Some(Err(TaskInstanceError::new_failed(
                            crate::runner::TaskError::Cancelled,
                        )))))
                    }
                    res = task => {
                        trace!("[STATE MACHINE] initializing task completed: {:?}", match &res {
                            Transition(State::Downloading { .. }) => "Downloading transition",
                            Transition(State::Stopped { .. }) => "Stopped transition",
                            _ => "Other transition",
                        });
                        res
                    },
                }
            }
            _ => Super,
        }
    }

    #[state(superstate = "running")]
    async fn downloading(
        &mut self,
        cancel_token: &mut CancellationToken,
        context: &mut Context,
        event: &Event,
    ) -> Outcome<State> {
        trace!(
            "[STATE MACHINE] downloading state entered, event: {:?}",
            match event {
                Event::Step => "Step",
                Event::Run(_) => "Run",
            }
        );
        match event {
            Event::Step => {
                trace!("[STATE MACHINE] calling download method");
                match self.download(cancel_token).await {
                    Ok(()) => {
                        trace!("[STATE MACHINE] download completed successfully");
                        Transition(State::stopped(Some(Ok(()))))
                    }
                    Err(e) => {
                        trace!("[STATE MACHINE] download failed: {:?}", e);
                        Transition(State::stopped(Some(Err(e))))
                    }
                }
            }
            _ => Super,
        }
    }
}
