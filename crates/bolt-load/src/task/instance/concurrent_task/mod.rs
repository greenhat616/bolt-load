use std::{path::Path, sync::Arc};

use async_waitgroup::WaitGroup;
use smol_cancellation_token::CancellationToken;

use super::{
    Result,
    drive::Transfer,
    output_file::{OutputFile, Teardown},
};
use crate::{
    adapter::AnyAdapter,
    runtime::ThreadedRuntimeImpl,
    task::{DownloadError, Progress, TaskOptions, status::ProgressReporter},
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

/// The transfer of the concurrent mode: runners download ranges in parallel.
pub(crate) struct ConcurrentTaskInner {
    rt: ThreadedRuntimeImpl,
    output: OutputFile,
    adapter: Arc<AnyAdapter>,
    /// The progress of the task, kept for resumable downloads.
    progress: Progress,
    reporter: ProgressReporter,
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
    /// `total` must be non-zero; a zero size selects the singleton mode.
    pub(crate) fn new(
        adapter: Arc<AnyAdapter>,
        output: OutputFile,
        total: u64,
        rt: ThreadedRuntimeImpl,
        options: TaskOptions,
        reporter: ProgressReporter,
    ) -> Self {
        debug_assert!(total > 0, "a concurrent task needs a known size");
        Self {
            rt,
            output,
            adapter,
            progress: Progress::new(Some(total), 0, Vec::new()),
            reporter,
            options,
        }
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    async fn create_file_writer(&self, tmp_path: &Path) -> Result<FileWriter> {
        let total_size = self
            .progress
            .total
            .ok_or_else(|| DownloadError::other("the concurrent task has no size"))?;
        FileWriter::new(tmp_path, total_size)
            .await
            .map_err(|e| DownloadError::other(e.to_string()))
    }

    /// Download into the temporary file, then commit it or discard it.
    ///
    /// Returns only after every owned worker has exited.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    async fn download(&mut self, cancel_token: &CancellationToken) -> Teardown {
        let tmp_path = self.output.tmp.clone();
        let transfer = self.download_to(&tmp_path, cancel_token).await;
        self.output.commit(transfer, cancel_token).await
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
        )
        .connect_timeout(self.options.connect_timeout);

        let mut download = DownloadLoop::new(
            total,
            spawner,
            &file_writer,
            self.reporter.clone(),
            &self.options,
            cancel_token.clone(),
        );
        let transfer_result = download.run().await;
        let transfer_result = download.finish(transfer_result, &mut self.progress).await;
        // Always join the writer workers, so the file handle is released before
        // the temporary file is renamed or removed.
        let finalize_result = file_writer
            .finalize()
            .await
            .map_err(|e| DownloadError::other(e.to_string()));

        // The first error takes precedence.
        transfer_result.and(finalize_result)
    }
}

impl Transfer for ConcurrentTaskInner {
    fn progress(&self) -> Progress {
        self.progress.clone()
    }

    fn final_progress(&self) -> Progress {
        self.progress.clone()
    }

    async fn download(&mut self, cancel_token: &CancellationToken) -> Teardown {
        ConcurrentTaskInner::download(self, cancel_token).await
    }
}
