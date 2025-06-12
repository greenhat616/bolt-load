use std::{collections::VecDeque, path::PathBuf, sync::Arc, time::Duration};

use super::{
    Result, TaskInstance, TaskInstanceError,
    sampler::{SAMPLE_INTERVAL, Sampler},
};
use crate::{
    adapter::AnyAdapter,
    runner::{RunnerMessage, RunnerMessageKind, StoppedReason, TaskRunner, TaskRunnerGuard},
    runtime::ThreadedRuntimeImpl,
    task::{
        Progress, RunnerId, Task,
        instance::{ProgressWithSpeed, RunningPayload, TaskControl, TaskEvent},
    },
    utils::ShutdownGuardExt,
};

use async_fs::OpenOptions;
use async_io::Timer;
use futures::{AsyncWriteExt, FutureExt, StreamExt, task::SpawnExt};
use smol_cancellation_token::CancellationToken;
use statig::prelude::*;

/// Singleton task only have one runner, so we use a static id for the runner
const STATIC_RUNNER_ID: RunnerId = 0;

pub struct SingletonTask {
    rt: ThreadedRuntimeImpl,
    task: Option<TaskControl>,
    progress: Progress,
}

impl TaskInstance for SingletonTask {
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
                let mut state_machine = SingletonTaskInner::new(path, event_tx, rt)
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
        let stream = self
            .adapter
            .as_ref()
            .unwrap()
            .full_stream()
            .await
            .map_err(TaskInstanceError::StreamFailed)?;

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(&self.path)
            .await
            .map_err(|e| TaskInstanceError::new_write_chunk_failed(e))?;

        if let Some(total) = self.total {
            file.set_len(total)
                .await
                .map_err(|e| TaskInstanceError::new_write_chunk_failed(e))?;
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
        let sampler = Sampler::default();
        let mut timer = Timer::interval(Duration::from_secs(SAMPLE_INTERVAL));

        let fut = runner.run().fuse();
        futures::pin_mut!(fut);
        loop {
            futures::select_biased! {
                _ = timer.next().fuse() => {
                    speed = sampler.sample(&mut meter);
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
                                                log::error!("failed to flush file: {:?}", e);
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
                                        .map_err(|e| TaskInstanceError::new_write_chunk_failed(e))?;
                                    let progress = Progress {
                                        total: self.total,
                                        downloaded: self.downloaded,
                                        downloaded_chunks: vec![0..self.downloaded],
                                    };
                                    let tx = self.event_tx.clone();
                                    self.rt.spawn(async move {
                                        tx.send(TaskEvent::Downloading(ProgressWithSpeed::new(
                                            progress, speed,
                                        )))
                                        .await;
                                    });
                                }
                                _ => {}
                            }
                        }
                        Err(e) => {
                            log::error!("runner message error: {:?}", e);
                        }
                    }
                }
            }
        }

        if let Err(e) = file.flush().await {
            log::warn!("failed to flush file: {:?}", e);
        }

        Ok(())
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
    on_transition = "Self::on_transition"
)]
#[allow(unused_variables)]
impl SingletonTaskInner {
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
    fn running(event: &Event) -> Response<State> {
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
                        Ok(()) => {
                            context.poll.push_back(());
                            Transition(State::downloading(cancel_token.clone()))
                        }
                        Err(e) => Transition(State::stopped(Some(Err(e)))),
                    }
                }
                .fuse();
                futures::pin_mut!(task);

                futures::select_biased! {
                    _ = cancel_token.cancelled().fuse() => {
                        Transition(State::stopped(Some(Err(TaskInstanceError::Failed(
                            crate::runner::TaskFailedKind::Cancelled,
                        )))))
                    }
                    res = task => { res },
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
    ) -> Response<State> {
        match event {
            Event::Step => match self.download(cancel_token).await {
                Ok(()) => Transition(State::stopped(Some(Ok(())))),
                Err(e) => Transition(State::stopped(Some(Err(e)))),
            },
            _ => Super,
        }
    }

    fn on_transition(&mut self, _source: &State, target: &State) {
        match target {
            State::Stopped { reason } => match reason.clone() {
                Some(Ok(())) => {
                    let progress = Progress {
                        total: Some(self.total.unwrap_or(self.downloaded)),
                        downloaded: self.downloaded,
                        downloaded_chunks: vec![0..self.downloaded],
                    };
                    let tx = self.event_tx.clone();
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
                let progress = Progress {
                    total: self.total,
                    downloaded: self.downloaded,
                    downloaded_chunks: vec![0..self.downloaded],
                };
                let tx = self.event_tx.clone();
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
