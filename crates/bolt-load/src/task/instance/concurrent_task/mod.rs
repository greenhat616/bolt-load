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
async fn create_file_writer(tmp_path: &Path, total: u64, drain: WaitGroup) -> Result<FileWriter> {
    FileWriter::new_drained(tmp_path, total, drain)
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

mod checkpoint;
#[cfg(test)]
mod checkpoint_bench;
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
    /// Where the checkpoints go, when the task keeps a state file (design
    /// §11.5.5); `None` takes none.
    checkpoints: Option<Box<dyn checkpoint::CheckpointReceiver>>,
    /// How long each sync of a checkpoint took, for the benchmark.
    #[cfg(test)]
    sync_times: checkpoint::SyncTimes,
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
            checkpoints: None,
            #[cfg(test)]
            sync_times: checkpoint::SyncTimes::default(),
        }
    }

    /// Hand the checkpoints of the download to `receiver`.
    #[cfg_attr(not(test), expect(dead_code, reason = "C33 hands over the state file"))]
    pub(crate) fn checkpoint_to(
        mut self,
        receiver: Box<dyn checkpoint::CheckpointReceiver>,
    ) -> Self {
        self.checkpoints = Some(receiver);
        self
    }

    /// Download into the temporary file, then commit it or discard it.
    ///
    /// Returns only after every owned worker has exited.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    async fn download(&mut self, cancel_token: &CancellationToken) -> Teardown {
        let tmp_path = self.output.tmp.clone();
        let transfer = self.download_to(&tmp_path, cancel_token).await;
        let completed = self.progress.clone();
        self.output.commit(transfer, completed, cancel_token).await
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
        let file_writer = create_file_writer(tmp_path, total, self.output.drain()).await?;

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
        if let Some(receiver) = self.checkpoints.take() {
            #[cfg_attr(not(test), expect(unused_mut, reason = "the benchmark sets the probe"))]
            let mut checkpoints = checkpoint::Checkpoints::new(
                total,
                checkpoint::Schedule::of(&self.options),
                receiver,
                file_writer.clone(),
                Arc::new(checkpoint::FileBlocks(tmp_path.to_path_buf())),
                self.output.drain(),
                std::time::Instant::now(),
            );
            #[cfg(test)]
            {
                checkpoints.sync_times = self.sync_times.clone();
            }
            download = download.with_checkpoints(checkpoints);
        }
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

    /// Every worker that holds the temporary file keeps a clone of the
    /// drain group until it exited, so a panic handler can wait for them.
    #[tokio::test]
    async fn the_writer_workers_hold_the_drain_group() {
        let dir = tempfile::tempdir().unwrap();
        let tmp_path = dir.path().join("file.tmp");
        let drain = WaitGroup::new();
        let writer = create_file_writer(&tmp_path, 4, drain.clone())
            .await
            .unwrap();
        writer
            .write_range(0..4, bytes::Bytes::from_static(b"data"))
            .await
            .unwrap();
        // Polled here, not on a thread that may not have run yet.
        let mut wait = std::pin::pin!(drain.wait());
        assert!(
            futures::poll!(wait.as_mut()).is_pending(),
            "the workers are still running"
        );
        finalize_file_writer(writer, &tmp_path).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), wait)
            .await
            .expect("the workers did not exit");
    }

    /// A real download, through the writer the features pick and real
    /// runners, records every block with the CRC of its bytes in the
    /// temporary file, the short last one included, each once.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_checkpoints_of_a_download_match_its_file() {
        use bolt_load_tests::adapter::simple::SimpleTestAdapterBuilder;

        use self::checkpoint::tests::{RecordingReceiver, assert_records_match};
        use crate::adapter::DynBoltLoadAdapter;

        const TOTAL: u64 = 5 * (1 << 20) + 123;
        let adapter = SimpleTestAdapterBuilder::new()
            .content_size(TOTAL as usize)
            .chunk_size(16 << 10)
            .support_range(true)
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("file.bin");
        let output = OutputFile::new(dest, crate::task::instance::ownership::Ownership::new());
        let tmp_path = output.tmp.clone();
        let (receiver, received) = RecordingReceiver::new(
            file_writer::SyncProbe::default(),
            checkpoint::BLOCK_SIZE.get(),
            TOTAL,
        );
        let (reporter, _status) = ProgressReporter::for_test();
        let options = TaskOptions::default()
            .checkpoint_bytes(std::num::NonZeroU64::new(1 << 20).unwrap())
            .max_connections(std::num::NonZeroUsize::new(4).unwrap());
        let mut inner = ConcurrentTaskInner::new(
            DynBoltLoadAdapter::new_arc(adapter),
            output,
            TOTAL,
            ThreadedRuntimeImpl::new_tokio_rt(),
            options,
            reporter,
        )
        .checkpoint_to(receiver);
        inner
            .download_to(&tmp_path, &CancellationToken::new())
            .await
            .unwrap();

        let content = std::fs::read(&tmp_path).unwrap();
        let recorded = received.recorded();
        let mut indices: Vec<u64> = recorded.iter().map(|block| block.index).collect();
        indices.sort_unstable();
        assert_eq!(indices, (0..6).collect::<Vec<_>>());
        assert_records_match(
            &recorded,
            &Arc::new(std::sync::Mutex::new(content)),
            checkpoint::BLOCK_SIZE.get(),
        );
    }

    #[tokio::test]
    async fn a_file_that_cannot_be_created_is_a_create_error() {
        let dir = tempfile::tempdir().unwrap();
        let tmp_path = dir.path().join("missing").join("file.tmp");

        let Err(error) = create_file_writer(&tmp_path, 4, WaitGroup::new()).await else {
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
