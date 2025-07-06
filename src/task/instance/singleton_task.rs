use std::{collections::VecDeque, path::PathBuf, sync::Arc, time::Duration};

use async_fs::OpenOptions;
use async_io::Timer;
use futures::{AsyncWriteExt, FutureExt, StreamExt, task::SpawnExt};
use smol_cancellation_token::CancellationToken;
use statig::prelude::*;

use super::{
    Result, TaskInstance, TaskInstanceError,
    sampler::{DEFAULT_SAMPLE_INTERVAL, SpeedSampler},
};
use crate::{
    adapter::AnyAdapter,
    runner::{RunnerMessage, RunnerMessageKind, StoppedReason, TaskRunner, TaskRunnerGuard},
    runtime::{LocalRuntimeBuilderImpl, ThreadedRuntimeExt, ThreadedRuntimeImpl},
    task::{
        Progress, RunnerId,
        instance::{ProgressWithSpeed, RunningPayload, TaskControl, TaskEvent},
    },
    utils::{ShutdownGuardExt, logging::*},
};

/// Singleton task only have one runner, so we use a static id for the runner
const STATIC_RUNNER_ID: RunnerId = 0;

pub struct SingletonTask {
    threaded_rt: ThreadedRuntimeImpl,
    local_runtime_builder: Option<LocalRuntimeBuilderImpl>,
    task: Option<TaskControl>,
    progress: Progress,
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
            progress: Progress::default(),
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
            local_rt.block_on(task)
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

        let (control_tx, control_rx) = async_channel::unbounded();
        let guard = TaskRunnerGuard::new(STATIC_RUNNER_ID, cancel_token.clone(), control_tx);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let _shutdown_guard = shutdown_tx.shutdown_guard();
        let (mut runner, runner_rx) = TaskRunner::new(
            self.total,
            stream,
            STATIC_RUNNER_ID,
            control_rx,
            cancel_token.clone(),
        );
        let _cancel_guard = cancel_token.clone().drop_guard();
        self.instance = Some((guard, shutdown_rx));

        let mut meter = 0;
        let mut speed = 0.0;
        let mut sampler = SpeedSampler::new();
        let mut timer = Timer::interval(Duration::from_millis(DEFAULT_SAMPLE_INTERVAL));

        let fut = runner.run().fuse();
        futures::pin_mut!(fut);
        loop {
            futures::select_biased! {
                _ = timer.next().fuse() => {
                    let (_, ema_speed) = sampler.sample(&mut meter);
                    speed = ema_speed;
                }
                _ = fut => (),
                msg = runner_rx.recv().fuse() => {
                    match msg {
                        Ok(RunnerMessage(_, kind)) => {
                            match kind {
                                RunnerMessageKind::Stopped(reason) => {
                                    match reason {
                                        StoppedReason::Finished => {
                                            if let Err(e) = file.flush().await {
                                                error!("failed to flush file: {e:?}");
                                            }
                                            break;
                                        }
                                        StoppedReason::Failed(kind) => {
                                            return Err(TaskInstanceError::Failed(kind))
                                        }
                                    }
                                }
                                RunnerMessageKind::Downloaded(chunk) => {
                                    self.downloaded += chunk.len() as u64;
                                    file.write_all(&chunk)
                                        .await
                                        .map_err(TaskInstanceError::new_write_chunk_failed)?;
                                    let progress = Progress {
                                        total: self.total,
                                        downloaded: self.downloaded,
                                        downloaded_chunks: vec![0..self.downloaded],
                                    };
                                    let tx = self.event_tx.clone();
                                    let _ = self.rt.spawn(async move {
                                        let _ = tx.send(TaskEvent::Downloading(ProgressWithSpeed::new(
                                            progress, speed,
                                        )))
                                        .await;
                                    });
                                }
                                _ => {}
                            }
                        }
                        Err(e) => {
                            error!("runner message error: {e:?}");
                        }
                    }
                }
            }
        }

        if let Err(e) = file.flush().await {
            warn!("failed to flush file: {e:?}");
        }

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
                        progress, 0.0,
                    )))
                    .await;
            }
        }
    }
}

enum Event {
    Run(RunningPayload),
    Stop(StoppedReason),
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
                Event::Stop(_) => "Stop",
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
                        Transition(State::stopped(Some(Err(TaskInstanceError::Failed(
                            crate::runner::TaskFailedKind::Cancelled,
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
                Event::Stop(_) => "Stop",
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
