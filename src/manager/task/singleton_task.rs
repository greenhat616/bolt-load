use std::{io::SeekFrom, path::PathBuf};

use crate::{
    adapter::AnyAdapter,
    manager::{Progress, RunnerId},
    runner::{TaskRunner, TaskRunnerGuard},
    utils::ShutdownGuardExt,
};
use async_fs::OpenOptions;
use futures::{AsyncSeekExt, AsyncWriteExt, FutureExt};
use smol_cancellation_token::CancellationToken;

use super::{Result, Task, TaskError};
use crate::runner::{RunnerMessage, RunnerMessageKind, StoppedReason};

/// Singleton task should only have one runner, so we use a static id for the runner
const STATIC_RUNNER_ID: RunnerId = 0;

pub struct SingletonTask {
    total: Option<u64>,
    downloaded: u64,
    instance: Option<(TaskRunnerGuard, oneshot::Receiver<()>)>,
    tmp_path: PathBuf,
}

impl Task for SingletonTask {
    async fn start(&mut self, adapter: &AnyAdapter, cancel_token: CancellationToken) -> Result<()> {
        // TODO: use backon to retry
        let meta = adapter
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
        let stream = adapter
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
            total,
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
                                        StoppedReason::Finished => return Ok(()),
                                        StoppedReason::Failed(kind) => {
                                            return Err(TaskError::Failed(kind))
                                        }
                                    }
                                }
                                RunnerMessageKind::Downloaded(chunk) => {
                                    self.downloaded += chunk.len() as u64;
                                    file.seek(SeekFrom::Start(self.downloaded))
                                        .await
                                        .map_err(TaskError::WriteChunkFailed)?;
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
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some((guard, cancel_rx)) = self.instance.take() {
            guard.cancel();
            let _ = cancel_rx.await;
        }
        Ok(())
    }

    fn inspect_progress(&self) -> Progress {
        let total = self.total;

        Progress {
            total,
            downloaded: self.downloaded,
            downloaded_chunks: vec![0..self.downloaded],
        }
    }
}
