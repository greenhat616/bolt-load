use std::{collections::VecDeque, path::PathBuf, sync::Arc};

use crate::{
    adapter::AnyAdapter,
    manager::RunnerId,
    runner::{TaskRunner, TaskRunnerGuard},
    utils::ShutdownGuardExt,
};
use async_fs::OpenOptions;
use futures::{AsyncWriteExt, FutureExt};
use smol_cancellation_token::CancellationToken;
use statig::prelude::*;

use super::{Result, TaskError};
use crate::runner::{RunnerMessage, RunnerMessageKind, StoppedReason};

/// Singleton task should only have one runner, so we use a static id for the runner
const STATIC_RUNNER_ID: RunnerId = 0;

struct SingletonTaskInner {
    total: Option<u64>,
    downloaded: u64,
    instance: Option<(TaskRunnerGuard, oneshot::Receiver<()>)>,
    tmp_path: PathBuf,
    adapter: Option<Arc<AnyAdapter>>,
}

#[derive(Clone)]
struct RunningPayload {
    adapter: Arc<AnyAdapter>,
    cancel_token: CancellationToken,
}

enum Event {
    Run(RunningPayload),
    Stop(StoppedReason),
    /// Step the state machine to get the next state
    Step,
}

struct Context {
    poll: VecDeque<()>,
}

#[state_machine(initial = "State::stopped(None)")]
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

    async fn retrieve_meta(&mut self) -> Result<()> {
        // TODO: use backon to retry
        let meta = self
            .adapter
            .as_ref()
            .unwrap()
            .retrieve_meta()
            .await
            .map_err(TaskError::RetrieveMetaFailed)?;
        let total = if meta.content_size == 0 {
            None
        } else {
            Some(meta.content_size)
        };
        self.total = total;
        self.downloaded = 0;
        Ok(())
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
                        Ok(()) => Transition(State::downloading(cancel_token.clone())),
                        Err(e) => Transition(State::stopped(Some(Err(e)))),
                    }
                }
                .fuse();
                futures::pin_mut!(task);

                futures::select_biased! {
                    res = task => { res },
                    _ = cancel_token.cancelled().fuse() => {
                        Transition(State::stopped(Some(Err(TaskError::Failed(
                            crate::runner::TaskFailedKind::Cancelled,
                        )))))
                    }
                }
            }
            _ => Super,
        }
    }

    async fn download(&mut self, cancel_token: &mut CancellationToken) -> Result<()> {
        let stream = self
            .adapter
            .as_ref()
            .unwrap()
            .full_stream()
            .await
            .map_err(TaskError::StreamFailed)?;

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(&self.tmp_path)
            .await
            .map_err(TaskError::WriteChunkFailed)?;

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

        let fut = runner.run().fuse();
        let runner_msg = runner_rx.recv().fuse();
        futures::pin_mut!(fut, runner_msg);
        loop {
            futures::select! {
                _ = fut => (),
                msg = runner_msg => {
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
                                            return Err(TaskError::Failed(kind))
                                        }
                                    }
                                }
                                RunnerMessageKind::Downloaded(chunk) => {
                                    self.downloaded += chunk.len() as u64;
                                    file.write_all(&chunk)
                                        .await
                                        .map_err(TaskError::WriteChunkFailed)?;
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
}