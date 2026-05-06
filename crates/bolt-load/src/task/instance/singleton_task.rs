use std::{collections::VecDeque, path::PathBuf, sync::Arc};

use async_fs::{File, OpenOptions};
use async_io::Timer;
use async_waitgroup::WaitGroup;
use bolt_load_utils::telemetry::*;
use futures::{AsyncWriteExt, FutureExt, StreamExt, task::SpawnExt};
use smol_cancellation_token::CancellationToken;
use statig::prelude::*;

use super::{
    Result, TaskInstance, TaskInstanceError,
    sampler::{DEFAULT_SAMPLE_INTERVAL, SpeedSampler},
};
use crate::{
    adapter::AnyAdapter,
    runner::{LifecycleEvent, StoppedReason, TaskRunner, TaskRunnerGuard},
    runtime::{LocalRuntimeBuilderImpl, ThreadedRuntimeExt, ThreadedRuntimeImpl},
    task::{
        Progress, RunnerId,
        instance::{ProgressWithSpeed, RunningPayload, TaskControl, TaskEvent},
    },
    utils::ShutdownGuardExt,
};

/// Singleton task only have one runner, so we use a static id for the runner
const STATIC_RUNNER_ID: RunnerId = 0;

pub struct SingletonTask {
    threaded_rt: ThreadedRuntimeImpl,
    local_runtime_builder: Option<LocalRuntimeBuilderImpl>,
    task: Option<TaskControl>,
}

impl SingletonTask {
    pub fn new(
        threaded_rt: ThreadedRuntimeImpl,
        local_runtime_builder: Option<LocalRuntimeBuilderImpl>,
    ) -> Self {
        Self {
            threaded_rt,
            local_runtime_builder,
            task: None,
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
        trace!("[SINGLETON TASK] Attempting to spawn state machine task");
        let task = ::blocking::unblock(move || {
            let local_rt = threaded_rt
                .downcast_local(local_rt_builder)
                .expect("failed to downcast the runtime");
            let task = async move {
                trace!("[SINGLETON TASK] State machine starting");
                let mut context = Context::default();
                let mut state_machine = SingletonTaskInner::new(path, event_tx, threaded_rt)
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
        if let Some(mut task) = self.task.take() {
            task.stop().await;
        }
        Ok(())
    }

    async fn wait(&mut self) -> Result<()> {
        trace!(
            "[SINGLETON TASK] wait() called, task present: {}",
            self.task.is_some()
        );
        if let Some(mut task) = self.task.take() {
            trace!("[SINGLETON TASK] waiting for task to complete");
            task.wait().await;
            trace!("[SINGLETON TASK] task completed");
        } else {
            warn!("[SINGLETON TASK] WARNING: No task to wait for!");
        }
        Ok(())
    }
}

struct SingletonTaskInner {
    total: Option<u64>,
    downloaded: u64,
    instance: Option<(TaskRunnerGuard, oneshot::Receiver<()>)>,
    path: PathBuf,
    adapter: Option<Arc<AnyAdapter>>,
    event_tx: async_channel::Sender<TaskEvent>,
    rt: ThreadedRuntimeImpl,
}

impl SingletonTaskInner {
    fn new(
        path: PathBuf,
        event_tx: async_channel::Sender<TaskEvent>,
        rt: ThreadedRuntimeImpl,
    ) -> Self {
        Self {
            total: None,
            downloaded: 0,
            instance: None,
            path,
            adapter: None,
            event_tx,
            rt,
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
            .map_err(TaskInstanceError::RetrieveMetaFailed)?;
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
            _ => {}
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

    #[allow(clippy::single_range_in_vec_init)]
    fn handle_timer_tick(
        &self,
        wg: &WaitGroup,
        speed: &mut f64,
        sampler: &mut SpeedSampler,
        meter: &mut usize,
    ) {
        let (_, ema_speed) = sampler.sample(meter);
        *speed = ema_speed;
        let progress = Progress {
            total: self.total,
            downloaded: self.downloaded,
            downloaded_chunks: [0..self.downloaded].to_vec(),
        };

        let event_tx = self.event_tx.clone();
        let speed = *speed;
        let wg = wg.clone();
        let _ = self.rt.spawn(async move {
            let _wg = wg;
            let _ = event_tx
                .send(TaskEvent::Downloading(ProgressWithSpeed::new(
                    progress, speed, 0.0,
                )))
                .await;
        });
    }

    /// Download the file
    async fn download(&mut self, cancel_token: &mut CancellationToken) -> Result<()> {
        trace!(
            "[SINGLETON TASK] download() method called, path: {:?}",
            self.path
        );
        let stream = self
            .adapter
            .as_ref()
            .unwrap()
            .full_stream()
            .await
            .map_err(TaskInstanceError::StreamFailed)?;
        trace!("[SINGLETON TASK] stream obtained successfully");

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(&self.path)
            .await
            .map_err(TaskInstanceError::new_write_chunk_failed)?;

        if let Some(total) = self.total {
            file.set_len(total)
                .await
                .map_err(TaskInstanceError::new_write_chunk_failed)?;
        }

        let wg = WaitGroup::new();
        let (control_tx, control_rx) = TaskRunnerGuard::create_control_channel();
        let guard = TaskRunnerGuard::new(STATIC_RUNNER_ID, cancel_token.clone(), control_tx);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let _shutdown_guard = shutdown_tx.shutdown_guard();

        // Use builder pattern to create runner with split channels
        let mut builder = TaskRunner::builder()
            .stream(stream)
            .runner_id(STATIC_RUNNER_ID)
            .control_signal(control_rx)
            .cancel_token(cancel_token.clone());

        if let Some(total) = self.total {
            builder = builder.total(total);
        }

        let (mut runner, lifecycle_rx, data_rx) =
            builder.build().expect("failed to build task runner");

        let _cancel_guard = cancel_token.clone().drop_guard();
        self.instance = Some((guard, shutdown_rx));

        let mut meter = 0;
        let mut speed = 0.0;
        let mut sampler = SpeedSampler::new();
        let mut timer = Timer::interval(DEFAULT_SAMPLE_INTERVAL);

        let result = async {
            let mut is_finished = false;
            let mut runner_stopped = false;
            let mut data_done = false;
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
                futures::pin_mut!(data_next);

                futures::select_biased! {
                    _ = timer.next().fuse() => {
                        self.handle_timer_tick(&wg, &mut speed, &mut sampler, &mut meter);
                    }
                    _ = fut => (),
                    // Data FIRST — higher priority than lifecycle to avoid losing frames
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
                    event = lifecycle_rx.next().fuse() => {
                        match event {
                            Some(event) => {
                                match self.handle_lifecycle_event(event, &mut is_finished).await {
                                    Ok(()) => {
                                        if is_finished {
                                            runner_stopped = true;
                                            // Don't break — drain data_rx first
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
                            }
                        }
                    }
                }
            }
            Ok(())
        }
        .await;

        if let Err(e) = file.flush().await {
            warn!("failed to flush file: {e:?}");
        }

        wg.wait().await;

        result?;

        Ok(())
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
