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

/// A failed operation on the temporary file, as [`DownloadError::Io`] with
/// its io error, the way the singleton mode reports it.
fn file_error(tmp_path: &Path, error: FileWriterError) -> DownloadError {
    DownloadError::io(error.op(), tmp_path, error.into_io_error())
}

/// Create the writer of the temporary file and size it to `total`.
#[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
async fn create_file_writer(tmp_path: &Path, total: u64) -> Result<FileWriter> {
    FileWriter::new(tmp_path, total)
        .await
        .map_err(|error| match error {
            FileWriterBuilderError::RangeWriter { source } => file_error(tmp_path, source),
            // Every kind `FileWriter::new` picks is given its file or path.
            FileWriterBuilderError::Validation { message } => DownloadError::other(message),
        })
}

/// Join the writer's workers and sync the temporary file to disk.
async fn finalize_file_writer(writer: impl FileRangeWriter, tmp_path: &Path) -> Result<()> {
    writer
        .finalize()
        .await
        .map_err(|error| file_error(tmp_path, error))
}

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
        let file_writer = create_file_writer(tmp_path, total).await?;

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
        let finalize_result = finalize_file_writer(file_writer, tmp_path).await;

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

#[cfg(test)]
mod tests {
    use std::{io, ops::Range};

    use bytes::Bytes;
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::task::IoOp;

    /// The operation, path and io error kind of a file error.
    fn file_error_parts(error: &DownloadError) -> (IoOp, &Path, io::ErrorKind) {
        let DownloadError::Io { op, path, source } = error else {
            panic!("expected a file error, got {error:?}");
        };
        (*op, path.as_path(), source.kind())
    }

    /// Writes nothing and fails `finalize` with an io error of its kind.
    struct FailingFinalize(io::ErrorKind);

    impl FileRangeWriter for FailingFinalize {
        async fn write_range(&self, _: Range<u64>, _: Bytes) -> Result<(), FileWriterError> {
            Ok(())
        }

        async fn finalize(self) -> Result<(), FileWriterError> {
            Err(FileWriterError::Finalize {
                source: CommandError::Io {
                    source: io::Error::new(self.0, "injected"),
                },
                path: "elsewhere.tmp".into(),
            })
        }
    }

    #[tokio::test]
    async fn a_file_that_cannot_be_created_is_a_create_error() {
        let dir = tempfile::tempdir().unwrap();
        let tmp_path = dir.path().join("missing").join("file.tmp");

        let Err(error) = create_file_writer(&tmp_path, 4).await else {
            panic!("created a file in a missing directory");
        };
        assert_eq!(
            file_error_parts(&error),
            (IoOp::Create, tmp_path.as_path(), io::ErrorKind::NotFound)
        );
    }

    #[tokio::test]
    async fn a_size_the_file_system_refuses_is_an_allocate_error() {
        let dir = tempfile::tempdir().unwrap();
        let tmp_path = dir.path().join("file.tmp");

        // The pool writer always sizes the file; the kind `FileWriter::new`
        // picks may not.
        let Err(FileWriterBuilderError::RangeWriter { source }) =
            FileWriter::new_with_kind(&tmp_path, u64::MAX, FileRangeWriterKind::Pool).await
        else {
            panic!("sized a file to u64::MAX");
        };
        let error = file_error(&tmp_path, source);
        let (op, path, _) = file_error_parts(&error);
        assert_eq!((op, path), (IoOp::Allocate, tmp_path.as_path()));
    }

    #[tokio::test]
    async fn a_failed_finalize_is_a_flush_error_on_the_temporary_file() {
        let tmp_path = Path::new("file.tmp");
        for kind in [io::ErrorKind::StorageFull, io::ErrorKind::PermissionDenied] {
            let error = finalize_file_writer(FailingFinalize(kind), tmp_path)
                .await
                .unwrap_err();
            assert_eq!(file_error_parts(&error), (IoOp::Flush, tmp_path, kind));
            assert!(
                error
                    .to_string()
                    .starts_with("failed to flush file.tmp: injected"),
                "{error}"
            );
        }
    }
}
