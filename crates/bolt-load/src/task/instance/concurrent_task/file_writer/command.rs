//! The commands of the backends' worker queues.
//!
//! A write command carries how to report its result. A [`CompletionGuard`]
//! reports it straight to the pending writer's completion channel, and it
//! enters the command before the command is queued, so a write completes
//! exactly once however it ends: done by a worker, dropped with the queue,
//! with a panicking worker, or with a task that was never polled.
use std::{
    io,
    path::{Path, PathBuf},
};

use async_channel::{Receiver, Sender, TrySendError};
use bolt_load_utils::telemetry::*;
use snafu::Snafu;

use super::{Chunk, FileRangeWriterImpl, FileWriter, FileWriterError, WriteCompletion};

#[derive(Debug, Snafu)]
pub enum CommandError {
    #[snafu(transparent)]
    Io { source: io::Error },
    #[snafu(display("failed to receive command: channel closed"))]
    Recv,
    #[snafu(display("failed to send command: channel closed"))]
    Send,
}

pub(super) type CommandAck = oneshot::Sender<io::Result<()>>;

/// Reports one write to the pending writer's completion channel; dropped
/// before that, it reports the write as failed.
pub(super) struct CompletionGuard {
    state: Option<(Sender<WriteCompletion>, Chunk)>,
}

impl CompletionGuard {
    pub(super) fn new(tx: Sender<WriteCompletion>, chunk: Chunk) -> Self {
        Self {
            state: Some((tx, chunk)),
        }
    }

    /// Report the result of a backend write.
    pub(super) fn complete(mut self, result: io::Result<()>, path: &Path) {
        let (tx, chunk) = self.state.take().expect("completed once");
        let range = chunk.range.clone();
        let result = result.map_err(|source| FileWriterError::WriteRange {
            source: source.into(),
            chunk: Some(chunk),
            path: path.to_path_buf(),
        });
        publish(tx, WriteCompletion { range, result }, true);
    }

    /// Report the result of [`FileRangeWriter::write_range`](super::FileRangeWriter::write_range),
    /// keeping its error as it is.
    pub(super) fn complete_result(mut self, result: Result<(), FileWriterError>) {
        let (tx, chunk) = self.state.take().expect("completed once");
        publish(
            tx,
            WriteCompletion {
                range: chunk.range,
                result,
            },
            true,
        );
    }
}

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        if let Some((tx, chunk)) = self.state.take() {
            let range = chunk.range.clone();
            let result = Err(FileWriterError::WriteRange {
                source: io::Error::other("write request dropped / panicked").into(),
                chunk: Some(chunk),
                path: PathBuf::new(),
            });
            // Never assert while unwinding.
            publish(tx, WriteCompletion { range, result }, false);
        }
    }
}

/// The channel holds a completion for every write in flight, so it is
/// never full; `assert_full` is off while unwinding.
fn publish(tx: Sender<WriteCompletion>, completion: WriteCompletion, assert_full: bool) {
    if let Err(TrySendError::Full(_)) = tx.try_send(completion) {
        error!("write completion channel full: in-flight accounting invariant broken");
        if assert_full {
            debug_assert!(false, "completion channel must cover every in-flight write");
        }
    }
    // Closed is expected when the consumer has been dropped.
}

/// How a worker reports a write.
pub(super) enum WriteReply {
    /// To a [`FileRangeWriter::write_range`](super::FileRangeWriter::write_range) call awaiting it.
    Awaited(CommandAck),
    /// To the pending writer's completion channel.
    Completion(CompletionGuard),
}

impl WriteReply {
    pub(super) fn complete(self, result: io::Result<()>, path: &Path) {
        match self {
            Self::Awaited(tx) => {
                let _ = tx.send(result);
            }
            Self::Completion(done) => done.complete(result, path),
        }
    }
}

/// A command of a backend's worker queue.
pub(super) enum Command {
    Write(Chunk, WriteReply),
    Finalize(CommandAck),
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Write(chunk, _) => f.debug_tuple("Write").field(chunk).finish(),
            Self::Finalize(_) => f.write_str("Finalize(..)"),
        }
    }
}

/// Why [`SubmitWrite::try_submit`] did not queue a write; the guard is
/// handed back.
pub(super) enum SubmitError {
    Full(Chunk, CompletionGuard),
    Closed(CompletionGuard),
}

/// Queues a guarded write straight to a backend's workers.
pub(super) trait SubmitWrite {
    fn try_submit(&self, chunk: Chunk, done: CompletionGuard) -> Result<(), SubmitError>;
}

pub(super) fn queue_closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "writer command channel closed")
}

impl FileWriter {
    /// The worker queue and file path of the backend; `None` for the
    /// test-only backend, which only supports awaited writes.
    pub(super) fn submission(&self) -> Option<(&Sender<Command>, &Path)> {
        match &*self.inner {
            #[cfg(feature = "mmap")]
            FileRangeWriterImpl::Mmap(writer) => Some((&writer.tx, writer.file.path())),
            FileRangeWriterImpl::Pool(writer) => Some((&writer.tx, writer.file.path())),
            FileRangeWriterImpl::Null(writer) => {
                Some((&writer.tx, Path::new(super::null_writer::NULL_PATH)))
            }
            #[cfg(feature = "compio")]
            FileRangeWriterImpl::Compio(writer) => Some((&writer.tx, &writer.path)),
            #[cfg(test)]
            FileRangeWriterImpl::Slow(_) => None,
        }
    }

    /// Queue a guarded write, waiting for room; the backend completes it.
    pub(super) async fn submit(&self, chunk: Chunk, done: CompletionGuard) {
        let (tx, path) = self.submission().expect("direct backend");
        if let Err(error) = tx
            .send(Command::Write(chunk, WriteReply::Completion(done)))
            .await
        {
            let Command::Write(_, reply) = error.into_inner() else {
                unreachable!()
            };
            reply.complete(Err(queue_closed()), path);
        }
    }
}

impl SubmitWrite for FileWriter {
    fn try_submit(&self, chunk: Chunk, done: CompletionGuard) -> Result<(), SubmitError> {
        let (tx, _) = self.submission().expect("direct backend");
        match tx.try_send(Command::Write(chunk, WriteReply::Completion(done))) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(Command::Write(chunk, WriteReply::Completion(done)))) => {
                Err(SubmitError::Full(chunk, done))
            }
            Err(TrySendError::Closed(Command::Write(_, WriteReply::Completion(done)))) => {
                Err(SubmitError::Closed(done))
            }
            Err(_) => unreachable!("submitted a direct write"),
        }
    }
}

/// A failed worker makes the backend unavailable and rejects its queued writes.
/// Receiver drop alone leaves queued values alive while senders still exist.
/// Normal finalization also closes the queue, allowing peer workers to exit.
pub(super) struct CloseOnExit<'a>(pub(super) &'a Receiver<Command>);

impl Drop for CloseOnExit<'_> {
    fn drop(&mut self) {
        self.0.close();
        while let Ok(command) = self.0.try_recv() {
            drop(command);
        }
    }
}
