use std::{
    sync::Arc,
    task::{Context, Poll},
};

use super::{DownloadLoop, LoopEvent, WriteCompletion};
use crate::{
    runner::TaskError,
    task::{DownloadError, instance::Result},
};

/// The bytes written by `completions`, or the first failed write as
/// [`TaskError::Write`], keeping its range and io error.
pub(super) fn check_completions(completions: Vec<WriteCompletion>) -> Result<usize> {
    let mut written = 0usize;
    for WriteCompletion { range, result } in completions {
        match result {
            Ok(()) => written += (range.end - range.start) as usize,
            Err(error) => {
                return Err(DownloadError::transfer(TaskError::Write {
                    range,
                    source: Arc::new(error.into_io_error()),
                }));
            }
        }
    }
    Ok(written)
}

impl DownloadLoop {
    /// Pending while no write is in flight: only the loop itself dispatches
    /// writes, and it polls again after handling the event that did.
    #[inline]
    pub(super) fn poll_writer(&mut self, cx: &mut Context<'_>) -> Poll<LoopEvent> {
        self.writer
            .poll_completions(cx)
            .map(LoopEvent::WriterCompleted)
    }

    pub(super) fn record_completions(&mut self, completions: Vec<WriteCompletion>) -> Result<()> {
        let written = check_completions(completions)?;
        self.throughput.record_written(written);
        self.policy.on_write_ok(written);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{io, ops::Range};

    use bytes::Bytes;
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::{
        runtime::ThreadedRuntimeImpl,
        task::instance::concurrent_task::file_writer::{
            CommandError, FileRangeWriter, FileWriterError, PendingWriter,
        },
    };

    /// Fails the write of `range` with an io error of `kind`; writes nothing.
    struct FailingWriter {
        range: Range<u64>,
        kind: io::ErrorKind,
    }

    impl FileRangeWriter for FailingWriter {
        async fn write_range(&self, range: Range<u64>, _: Bytes) -> Result<(), FileWriterError> {
            if range != self.range {
                return Ok(());
            }
            Err(FileWriterError::WriteRange {
                source: CommandError::Io {
                    source: io::Error::new(self.kind, "injected"),
                },
                chunk: None,
                path: "file.bin".into(),
            })
        }

        async fn finalize(self) -> Result<(), FileWriterError> {
            Ok(())
        }
    }

    /// Write 0..4, 4..8 and 8..12 through a writer that fails 4..8.
    async fn completions(kind: io::ErrorKind) -> Vec<WriteCompletion> {
        let writer = FailingWriter { range: 4..8, kind };
        let mut pending =
            PendingWriter::new(Arc::new(writer), ThreadedRuntimeImpl::new_tokio_rt(), 4);
        for start in [0, 4, 8] {
            pending
                .write_range(start..start + 4, Bytes::from_static(b"data"))
                .unwrap();
        }
        let mut completions = pending.finish().await;
        completions.sort_by_key(|completion| completion.range.start);
        completions
    }

    #[tokio::test]
    async fn a_failed_write_keeps_its_range_and_io_error() {
        for kind in [
            io::ErrorKind::StorageFull,
            io::ErrorKind::QuotaExceeded,
            io::ErrorKind::PermissionDenied,
        ] {
            let completions = completions(kind).await;
            assert_eq!(completions.len(), 3, "{kind:?}");

            let error = check_completions(completions).unwrap_err();
            let Some(TaskError::Write { range, source }) = error.as_transfer() else {
                panic!("{kind:?}: expected a write error, got {error:?}");
            };
            assert_eq!(*range, 4..8, "{kind:?}");
            assert_eq!(source.kind(), kind);
            assert_eq!(source.to_string(), "injected");
        }
    }

    #[tokio::test]
    async fn successful_writes_count_their_bytes() {
        let writer = FailingWriter {
            range: 0..0,
            kind: io::ErrorKind::Other,
        };
        let mut pending =
            PendingWriter::new(Arc::new(writer), ThreadedRuntimeImpl::new_tokio_rt(), 4);
        pending
            .write_range(0..4, Bytes::from_static(b"data"))
            .unwrap();
        pending
            .write_range(4..6, Bytes::from_static(b"da"))
            .unwrap();

        assert_eq!(check_completions(pending.finish().await).unwrap(), 6);
    }

    #[test]
    fn a_closed_writer_queue_is_a_broken_pipe() {
        let completion = WriteCompletion {
            range: 0..4,
            result: Err(FileWriterError::WriteRange {
                source: CommandError::Recv,
                chunk: None,
                path: "file.bin".into(),
            }),
        };

        let error = check_completions(vec![completion]).unwrap_err();
        let Some(TaskError::Write { range, source }) = error.as_transfer() else {
            panic!("expected a write error, got {error:?}");
        };
        assert_eq!(*range, 0..4);
        assert_eq!(source.kind(), io::ErrorKind::BrokenPipe);
    }
}
