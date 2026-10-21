use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_waitgroup::WaitGroup;
use bolt_load_core::adapter::AdapterError;
use smol_cancellation_token::CancellationToken;

use super::{
    Result, TaskInstance,
    drive::{self, Transfer},
    output_file,
};
use crate::{
    adapter::{AnyAdapter, UnretryableError},
    runner::TaskError,
    runtime::{LocalRuntimeBuilderImpl, ThreadedRuntimeImpl},
    task::{
        Progress, TaskOptions,
        instance::{TaskControl, TaskEvent, TaskInstanceError},
        meta,
    },
};

mod chunk_planner;
mod download_loop;
/// File writer implementations for concurrent downloads.
///
/// This module is public for benchmarking purposes.
pub mod file_writer;
mod runner_manager;
mod runner_spawner;
mod strategy;
#[cfg(test)]
mod test_support;

use chunk_planner::*;
use download_loop::DownloadLoop;
use file_writer::*;
use runner_manager::RunnerManager;
use runner_spawner::RunnerSpawner;

static DEFAULT_MAX_CONCURRENCY: usize = 4;

#[expect(
    dead_code,
    reason = "the initial progress is kept for resumable downloads"
)]
pub struct ConcurrentTask {
    threaded_rt: ThreadedRuntimeImpl,
    local_runtime_builder: Option<LocalRuntimeBuilderImpl>,
    task: Option<TaskControl>,
    progress: Progress,
    options: TaskOptions,
}

impl ConcurrentTask {
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
            progress: Progress::default(),
            options,
        }
    }
}

/// One download attempt, run on a local runtime by [`drive::spawn_on_local`].
pub(super) fn concurrent_attempt(
    adapter: Arc<AnyAdapter>,
    path: PathBuf,
    event_tx: async_channel::Sender<TaskEvent>,
    cancel_token: CancellationToken,
    rt: ThreadedRuntimeImpl,
    options: TaskOptions,
) -> impl Future<Output = ()> + 'static {
    let inner = ConcurrentTaskInner::new(adapter, path, event_tx.clone(), rt, options);
    let task = drive::drive(inner, event_tx, cancel_token);
    #[cfg(feature = "tracing")]
    let task = tracing::Instrument::instrument(
        task,
        tracing::trace_span!(parent: None, "ConcurrentTask::background_task"),
    );
    task
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
        let options = self.options.clone();
        let task = drive::spawn_on_local(
            self.threaded_rt.clone(),
            self.local_runtime_builder.clone(),
            cancel_token.clone(),
            // Built on the local runtime's thread, so it does not need to be `Send`.
            move |rt| concurrent_attempt(adapter, path, event_tx, cancel_token, rt, options),
        );
        self.task = Some(task);
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(task) = self.task.as_mut() {
            task.stop().await;
        }
        Ok(())
    }

    async fn wait(&mut self) -> Result<()> {
        if let Some(task) = self.task.as_mut() {
            task.wait().await;
        }
        Ok(())
    }
}

pub struct ConcurrentTaskInner {
    rt: ThreadedRuntimeImpl,
    path: PathBuf,
    adapter: Arc<AnyAdapter>,
    /// The initial progress of the task, for task resume
    progress: Progress,
    event_tx: async_channel::Sender<TaskEvent>,
    options: TaskOptions,
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[expect(dead_code, reason = "reserved for persisting runner states")]
enum RunnerStatus {
    Running,
    Finished,
}

impl ConcurrentTaskInner {
    fn new(
        adapter: Arc<AnyAdapter>,
        path: PathBuf,
        event_tx: async_channel::Sender<TaskEvent>,
        rt: ThreadedRuntimeImpl,
        options: TaskOptions,
    ) -> Self {
        Self {
            rt,
            path,
            adapter,
            progress: Progress::default(),
            event_tx,
            options,
        }
    }
    /// Retrieve the meta data of the file
    async fn retrieve_meta(&mut self) -> Result<()> {
        let meta = meta::retrieve_meta(&self.adapter, &self.options.meta_retry, Some(&self.rt))
            .await
            .map_err(TaskInstanceError::new_retrieve_meta_failed)?;
        let total = if meta.content_size == 0 {
            return Err(TaskInstanceError::RetrieveMetaFailed {
                source: AdapterError::Unretryable {
                    source: UnretryableError::Whatever {
                        message: "content size is 0; concurrent task does not support 0-size file"
                            .to_string(),
                        source: None,
                    },
                },
            });
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
    async fn create_file_writer(&self, tmp_path: &Path) -> Result<FileWriter> {
        let total_size = self.progress.total.ok_or_else(|| {
            TaskInstanceError::new_failed(TaskError::Other {
                message: "File writer creation called before meta retrieval".to_string(),
            })
        })?;
        FileWriter::new(tmp_path, total_size).await.map_err(|e| {
            TaskInstanceError::new_failed(TaskError::Other {
                message: e.to_string(),
            })
        })
    }

    /// Download into the temporary file, then publish it or discard it.
    ///
    /// Returns only after every owned worker has exited.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    async fn download(&mut self, cancel_token: &CancellationToken) -> Result<()> {
        let tmp_path = output_file::tmp_path_for(&self.path);
        let transfer = self.download_to(&tmp_path, cancel_token).await;
        output_file::commit(transfer, &tmp_path, &self.path, cancel_token).await
    }

    /// Run the transfer and tear it down on every path: success, failure and
    /// cancellation. Teardown never short-circuits, so all spawned work is joined.
    async fn download_to(
        &mut self,
        tmp_path: &Path,
        cancel_token: &CancellationToken,
    ) -> Result<()> {
        let total = self
            .progress
            .total
            .expect("A concurrent task should have a total size");
        let file_writer = self.create_file_writer(tmp_path).await?;

        let runners_cancel_token = cancel_token.child_token();
        // Fallback cancellation signal on unwinding; `DownloadLoop::finish` cancels explicitly.
        let _runner_cancel_guard = runners_cancel_token.clone().drop_guard();
        let spawner = RunnerSpawner::new(
            self.rt.clone(),
            self.adapter.clone(),
            WaitGroup::new(),
            runners_cancel_token,
            self.options.stream_idle_timeout,
        );

        let mut download = DownloadLoop::new(
            total,
            spawner,
            &file_writer,
            self.event_tx.clone(),
            &self.options,
        );
        let transfer_result = download.run(cancel_token).await;
        let transfer_result = download.finish(transfer_result, &mut self.progress).await;
        // Always join the writer workers, so the file handle is released before
        // the temporary file is renamed or removed.
        let finalize_result = file_writer.finalize().await.map_err(|e| {
            TaskInstanceError::new_failed(TaskError::Other {
                message: e.to_string(),
            })
        });

        // The first error takes precedence.
        transfer_result.and(finalize_result)
    }
}

impl Transfer for ConcurrentTaskInner {
    async fn prepare(&mut self) -> Result<()> {
        self.retrieve_meta().await
    }

    fn progress(&self) -> Progress {
        self.progress.clone()
    }

    fn final_progress(&self) -> Progress {
        self.progress.clone()
    }

    async fn download(&mut self, cancel_token: &CancellationToken) -> Result<()> {
        ConcurrentTaskInner::download(self, cancel_token).await
    }
}
