use std::{
    sync::Arc,
    task::{Context, Poll},
    time::Instant,
};

use super::{super::file_writer::CrcSegment, DownloadLoop, LoopEvent, WriteCompletion};
use crate::{
    runner::TaskError,
    task::{DownloadError, instance::Result},
};

/// Hand every successful write of `completions` to `on_written`, with the
/// CRCs of its bytes when the writer computes them, and return
/// the bytes they wrote, or the first failed write in the order the
/// completions were received as [`TaskError::Write`], keeping its range and
/// io error.
///
/// The writes after a failed one are handed over all the same: their bytes
/// are in the file (design §11.1).
pub(super) fn settle_completions(
    completions: Vec<WriteCompletion>,
    mut on_written: impl FnMut(std::ops::Range<u64>, &[CrcSegment]),
) -> Result<usize> {
    let mut written = 0usize;
    let mut failed = None;
    for WriteCompletion {
        range,
        result,
        crcs,
    } in completions
    {
        match result {
            Ok(()) => {
                written += (range.end - range.start) as usize;
                on_written(range, &crcs);
            }
            Err(error) if failed.is_none() => {
                failed = Some(DownloadError::transfer(TaskError::Write {
                    range,
                    source: Arc::new(error.into_io_error()),
                }));
            }
            Err(_) => {}
        }
    }
    failed.map_or(Ok(written), Err)
}

impl DownloadLoop {
    /// Pending while no write is in flight: only the loop itself dispatches
    /// writes, and it polls again after handling the event that did.
    ///
    /// Whether the writer could take data is read before the completions
    /// are polled: polling them already frees their slots and dispatches
    /// the pending write, so the writer can take data by the time the event
    /// is handled (design §9.6).
    #[inline]
    pub(super) fn poll_writer(&mut self, cx: &mut Context<'_>) -> Poll<LoopEvent> {
        let was_full = !self.writer.can_write();
        self.writer
            .poll_completions(cx)
            .map(|completions| LoopEvent::WriterCompleted(completions, was_full))
    }

    /// Account for `completions` at `now`; when the writer could not take
    /// data before them and can now, every runner's silence starts over,
    /// as the loop took no data meanwhile (design §9.6).
    pub(super) fn record_completions(
        &mut self,
        completions: Vec<WriteCompletion>,
        was_full: bool,
        now: Instant,
    ) -> Result<()> {
        let manager = &mut self.manager;
        let checkpoints = &mut self.checkpoints;
        let written = settle_completions(completions, |range, crcs| {
            if let Some(checkpoints) = checkpoints.as_mut() {
                checkpoints.note_written(range.clone(), crcs);
            }
            manager.note_written(range);
        });
        // The writes that completed count even when one failed.
        self.maybe_checkpoint(now);
        let written = written?;
        self.throughput.record_written(written);
        self.policy.on_write_ok(written);
        if was_full && self.writer.can_write() {
            self.manager.restart_silences(now);
        }
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

    /// A failed write is the task's error, and is not written; the writes
    /// around it are, before and after it in the batch (design §11.1).
    #[tokio::test]
    async fn a_failed_write_keeps_its_range_and_io_error() {
        for kind in [
            io::ErrorKind::StorageFull,
            io::ErrorKind::QuotaExceeded,
            io::ErrorKind::PermissionDenied,
        ] {
            let completions = completions(kind).await;
            assert_eq!(completions.len(), 3, "{kind:?}");

            let mut written = Vec::new();
            let error =
                settle_completions(completions, |range, _| written.push(range)).unwrap_err();
            assert_eq!(written, [0..4, 8..12], "{kind:?}");
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

        let mut written = Vec::new();
        let mut completions = pending.finish().await;
        completions.sort_by_key(|completion| completion.range.start);
        assert_eq!(
            settle_completions(completions, |range, _| written.push(range)).unwrap(),
            6
        );
        assert_eq!(written, [0..4, 4..6]);
    }

    #[test]
    fn a_closed_writer_queue_is_a_broken_pipe() {
        let completion = WriteCompletion {
            crcs: Vec::new(),
            range: 0..4,
            result: Err(FileWriterError::WriteRange {
                source: CommandError::Recv,
                chunk: None,
                path: "file.bin".into(),
            }),
        };

        let error = settle_completions(vec![completion], |range, _| {
            panic!("{range:?} was not written");
        })
        .unwrap_err();
        let Some(TaskError::Write { range, source }) = error.as_transfer() else {
            panic!("expected a write error, got {error:?}");
        };
        assert_eq!(*range, 0..4);
        assert_eq!(source.kind(), io::ErrorKind::BrokenPipe);
    }
}
