//! Pending writer for concurrent downloads.
//!
//! This wraps a [`FileRangeWriter`] with bounded write dispatch. Download code
//! can keep a small number of writes in flight and stop consuming runner data
//! when the writer is saturated.

use std::{ops::Range, path::PathBuf, sync::Arc};

use async_channel::{Receiver, Sender};
use bytes::Bytes;
use futures::task::SpawnExt;

use super::{Chunk, CommandError, FileRangeWriter, FileWriterError};
use crate::runtime::ThreadedRuntimeImpl;

/// A write request that has not completed yet.
#[derive(derive_more::Debug, Clone)]
pub struct PendingWrite {
    #[debug("{}..{}", range.start, range.end)]
    pub range: Range<u64>,
    #[debug("{} bytes", bytes.len())]
    pub bytes: Bytes,
}

/// Result of a completed write.
#[derive(Debug)]
pub struct WriteCompletion {
    pub range: Range<u64>,
    pub result: Result<(), FileWriterError>,
}

struct WriteCompletionGuard {
    tx: Sender<WriteCompletion>,
    range: Range<u64>,
    chunk: Option<Chunk>,
    armed: bool,
}

impl WriteCompletionGuard {
    fn new(tx: Sender<WriteCompletion>, range: Range<u64>, chunk: Chunk) -> Self {
        Self {
            tx,
            range,
            chunk: Some(chunk),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
        self.chunk = None;
    }
}

impl Drop for WriteCompletionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        let _ = self.tx.try_send(WriteCompletion {
            range: self.range.clone(),
            result: Err(FileWriterError::WriteRange {
                source: CommandError::Io {
                    source: std::io::Error::other("write task panicked"),
                },
                chunk: self.chunk.take(),
                path: PathBuf::new(),
            }),
        });
    }
}

/// Indicates what happened when a write was submitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStatus {
    /// The write was dispatched to the background runtime immediately.
    Dispatched,
    /// The writer is at capacity and the write was buffered.
    Pending,
}

/// Error returned when the writer cannot accept another write.
#[derive(Debug, thiserror::Error)]
#[error("writer cannot accept another write")]
pub struct WriterFullError(pub PendingWrite);

#[derive(derive_more::Debug)]
pub struct PendingWriter<F> {
    writer: Arc<F>,
    #[debug(skip)]
    threaded_rt: ThreadedRuntimeImpl,
    capacity: usize,
    in_flight: usize,

    #[debug(skip)]
    completion_tx: Sender<WriteCompletion>,
    #[debug(skip)]
    completion_rx: Receiver<WriteCompletion>,

    /// At most one write is buffered while all dispatch slots are full.
    pending_write: Option<PendingWrite>,
    /// Once any write fails, stop accepting and auto-dispatching writes.
    saw_error: bool,
}

impl<F> PendingWriter<F>
where
    F: FileRangeWriter + Send + Sync + 'static,
{
    pub fn new(writer: Arc<F>, threaded_rt: ThreadedRuntimeImpl, capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let (tx, rx) = async_channel::bounded(capacity + 1);
        Self {
            writer,
            threaded_rt,
            capacity,
            in_flight: 0,
            completion_tx: tx,
            completion_rx: rx,
            pending_write: None,
            saw_error: false,
        }
    }

    #[inline]
    pub fn in_flight_count(&self) -> usize {
        self.in_flight
    }

    #[inline]
    pub fn is_full(&self) -> bool {
        self.in_flight_count() >= self.capacity
    }

    #[inline]
    pub fn has_pending_write(&self) -> bool {
        self.pending_write.is_some()
    }

    #[inline]
    pub fn is_idle(&self) -> bool {
        self.in_flight_count() == 0 && !self.has_pending_write()
    }

    #[inline]
    pub fn can_write(&self) -> bool {
        !self.saw_error && (!self.is_full() || !self.has_pending_write())
    }

    pub fn write_range(
        &mut self,
        range: Range<u64>,
        data: Bytes,
    ) -> Result<WriteStatus, WriterFullError> {
        let write = PendingWrite { range, bytes: data };
        if self.saw_error {
            return Err(WriterFullError(write));
        }

        if !self.is_full() {
            self.dispatch_write(write.range, write.bytes);
            Ok(WriteStatus::Dispatched)
        } else if self.pending_write.is_none() {
            self.pending_write = Some(write);
            Ok(WriteStatus::Pending)
        } else {
            Err(WriterFullError(write))
        }
    }

    /// Drain already completed writes without blocking.
    pub fn try_tick(&mut self) -> Vec<WriteCompletion> {
        let completions = self.drain_completions();
        self.saw_error |= Self::completions_have_error(&completions);
        if !self.saw_error {
            self.try_flush_pending();
        }
        completions
    }

    /// Wait for at least one in-flight write to finish, then drain the rest.
    pub async fn tick(&mut self) -> Option<Vec<WriteCompletion>> {
        if self.in_flight_count() == 0 {
            return Some(Vec::new());
        }

        let first = self.completion_rx.recv().await.ok()?;
        let mut completions = vec![first];
        while let Ok(completion) = self.completion_rx.try_recv() {
            completions.push(completion);
        }
        self.ack_completions(completions.len());
        self.saw_error |= Self::completions_have_error(&completions);
        if !self.saw_error {
            self.try_flush_pending();
        }
        Some(completions)
    }

    /// Wait until all dispatched writes finish.
    pub async fn flush(&mut self) -> Vec<WriteCompletion> {
        let mut completions = Vec::new();

        loop {
            let drained = self.drain_completions();
            self.saw_error |= Self::completions_have_error(&drained);
            completions.extend(drained);

            if !self.saw_error {
                self.try_flush_pending();
            }

            if self.in_flight_count() == 0 {
                break;
            }

            match self.completion_rx.recv().await {
                Ok(completion) => {
                    self.ack_completions(1);
                    self.saw_error |= completion.result.is_err();
                    completions.push(completion);
                }
                Err(_) => break,
            }
        }

        completions.extend(self.drain_completions());
        completions
    }

    fn completions_have_error(completions: &[WriteCompletion]) -> bool {
        completions
            .iter()
            .any(|completion| completion.result.is_err())
    }

    fn ack_completions(&mut self, count: usize) {
        self.in_flight = self
            .in_flight
            .checked_sub(count)
            .expect("in_flight underflow: more completions drained than dispatched");
    }

    fn dispatch_write(&mut self, range: Range<u64>, data: Bytes) {
        self.in_flight += 1;

        let writer = Arc::clone(&self.writer);
        let tx = self.completion_tx.clone();
        let task_range = range.clone();
        let completion_range = range.clone();
        let guard_range = range.clone();
        let guard_chunk = Chunk {
            range: range.clone(),
            data: data.clone(),
        };
        let completion_chunk = Chunk {
            range,
            data: data.clone(),
        };

        if let Err(spawn_error) = self.threaded_rt.spawn(async move {
            let mut guard = WriteCompletionGuard::new(tx.clone(), guard_range, guard_chunk);
            let result = writer.write_range(task_range.clone(), data).await;
            let _ = tx
                .send(WriteCompletion {
                    range: task_range,
                    result,
                })
                .await;
            guard.disarm();
        }) {
            self.completion_tx
                .try_send(WriteCompletion {
                    range: completion_range,
                    result: Err(FileWriterError::WriteRange {
                        source: CommandError::Io {
                            source: std::io::Error::other(format!(
                                "failed to spawn write task: {spawn_error}"
                            )),
                        },
                        chunk: Some(completion_chunk),
                        path: PathBuf::new(),
                    }),
                })
                .expect("completion channel should have capacity for spawn-failure completion");
        }
    }

    fn try_flush_pending(&mut self) {
        if !self.is_full() {
            if let Some(pending) = self.pending_write.take() {
                self.dispatch_write(pending.range, pending.bytes);
            }
        }
    }

    fn drain_completions(&mut self) -> Vec<WriteCompletion> {
        let mut completions = Vec::new();
        while let Ok(completion) = self.completion_rx.try_recv() {
            completions.push(completion);
        }
        self.ack_completions(completions.len());
        completions
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use async_channel::Receiver;
    use bytes::Bytes;
    use futures::{
        future::FutureObj,
        task::{Spawn, SpawnError},
    };

    use super::{super::FileWriterFuture, *};
    use crate::runtime::{
        DowncastLocalRuntime, ObjectSafeTimer, ThreadedRuntime, TimerBuilder, TimerImpl,
    };

    #[derive(Debug, Default)]
    struct RecordingWriter {
        writes: Mutex<Vec<(Range<u64>, Bytes)>>,
    }

    impl RecordingWriter {
        fn writes(&self) -> Vec<(Range<u64>, Bytes)> {
            self.writes.lock().unwrap().clone()
        }
    }

    impl FileRangeWriter for RecordingWriter {
        fn write_range(&self, range: Range<u64>, data: Bytes) -> FileWriterFuture<'_> {
            Box::pin(async move {
                self.writes.lock().unwrap().push((range, data));
                Ok(())
            })
        }

        fn finalize(self) -> FileWriterFuture<'static> {
            Box::pin(async { Ok(()) })
        }
    }

    #[derive(Debug)]
    struct ControlledWriter {
        writes: Mutex<Vec<(Range<u64>, Bytes)>>,
        release_rx: Receiver<()>,
    }

    impl ControlledWriter {
        fn new(release_rx: Receiver<()>) -> Self {
            Self {
                writes: Mutex::new(Vec::new()),
                release_rx,
            }
        }

        fn writes(&self) -> Vec<(Range<u64>, Bytes)> {
            self.writes.lock().unwrap().clone()
        }
    }

    impl FileRangeWriter for ControlledWriter {
        fn write_range(&self, range: Range<u64>, data: Bytes) -> FileWriterFuture<'_> {
            Box::pin(async move {
                let _ = self.release_rx.recv().await;
                self.writes.lock().unwrap().push((range, data));
                Ok(())
            })
        }

        fn finalize(self) -> FileWriterFuture<'static> {
            Box::pin(async { Ok(()) })
        }
    }

    #[derive(Clone, Debug)]
    struct FailingRuntime;

    impl Spawn for FailingRuntime {
        fn spawn_obj(&self, _future: FutureObj<'static, ()>) -> Result<(), SpawnError> {
            Err(SpawnError::shutdown())
        }
    }

    impl DowncastLocalRuntime for FailingRuntime {}

    impl TimerBuilder for FailingRuntime {
        fn create_delayed_timer(&self, _duration: Duration) -> TimerImpl {
            TimerImpl::Custom(Box::new(NoopTimer))
        }
    }

    impl ThreadedRuntime for FailingRuntime {}

    #[derive(Debug)]
    struct PanicWriter;

    impl FileRangeWriter for PanicWriter {
        fn write_range(&self, _range: Range<u64>, _data: Bytes) -> FileWriterFuture<'_> {
            Box::pin(async { panic!("injected write panic") })
        }

        fn finalize(self) -> FileWriterFuture<'static> {
            Box::pin(async { Ok(()) })
        }
    }

    struct NoopTimer;

    #[async_trait::async_trait]
    impl ObjectSafeTimer for NoopTimer {
        async fn tick(&mut self) {}
    }

    #[derive(Debug)]
    struct ControlledFailingWriter {
        release_rx: Receiver<()>,
    }

    impl FileRangeWriter for ControlledFailingWriter {
        fn write_range(&self, range: Range<u64>, data: Bytes) -> FileWriterFuture<'_> {
            Box::pin(async move {
                let _ = self.release_rx.recv().await;
                Err(FileWriterError::WriteRange {
                    source: CommandError::Io {
                        source: std::io::Error::other("injected failure"),
                    },
                    chunk: Some(Chunk { range, data }),
                    path: PathBuf::new(),
                })
            })
        }

        fn finalize(self) -> FileWriterFuture<'static> {
            Box::pin(async { Ok(()) })
        }
    }

    #[derive(Debug)]
    struct DelayWriter {
        delay: Duration,
    }

    impl FileRangeWriter for DelayWriter {
        fn write_range(&self, _range: Range<u64>, _data: Bytes) -> FileWriterFuture<'_> {
            Box::pin(async move {
                tokio::time::sleep(self.delay).await;
                Ok(())
            })
        }

        fn finalize(self) -> FileWriterFuture<'static> {
            Box::pin(async { Ok(()) })
        }
    }

    async fn next_tick<F>(writer: &mut PendingWriter<F>) -> Vec<WriteCompletion>
    where
        F: FileRangeWriter + Send + Sync + 'static,
    {
        tokio::time::timeout(Duration::from_secs(1), writer.tick())
            .await
            .expect("timed out waiting for write completion")
            .expect("completion channel closed")
    }

    #[tokio::test]
    async fn basic_write_tick_completion() {
        let writer = Arc::new(RecordingWriter::default());
        let mut pending =
            PendingWriter::new(Arc::clone(&writer), ThreadedRuntimeImpl::new_tokio_rt(), 2);

        assert_eq!(
            pending
                .write_range(0..4, Bytes::from_static(b"test"))
                .unwrap(),
            WriteStatus::Dispatched
        );

        let completions = next_tick(&mut pending).await;
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].range, 0..4);
        assert!(completions[0].result.is_ok());
        assert!(pending.is_idle());

        let writes = writer.writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].0, 0..4);
        assert_eq!(&writes[0].1[..], b"test");
    }

    #[tokio::test]
    async fn pending_buffer_flushes_after_capacity_opens() {
        let (release_tx, release_rx) = async_channel::unbounded();
        let writer = Arc::new(ControlledWriter::new(release_rx));
        let mut pending =
            PendingWriter::new(Arc::clone(&writer), ThreadedRuntimeImpl::new_tokio_rt(), 1);

        assert_eq!(
            pending.write_range(0..1, Bytes::from_static(b"a")).unwrap(),
            WriteStatus::Dispatched
        );
        assert_eq!(
            pending.write_range(1..2, Bytes::from_static(b"b")).unwrap(),
            WriteStatus::Pending
        );
        assert!(pending.is_full());
        assert!(pending.has_pending_write());

        release_tx.send(()).await.unwrap();
        let first = next_tick(&mut pending).await;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].range, 0..1);
        assert!(first[0].result.is_ok());
        assert!(!pending.has_pending_write());
        assert!(pending.is_full());

        release_tx.send(()).await.unwrap();
        let second = next_tick(&mut pending).await;
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].range, 1..2);
        assert!(second[0].result.is_ok());
        assert!(pending.is_idle());

        let writes = writer.writes();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].0, 0..1);
        assert_eq!(writes[1].0, 1..2);
    }

    #[tokio::test]
    async fn full_writer_rejects_third_write() {
        let (release_tx, release_rx) = async_channel::unbounded();
        let writer = Arc::new(ControlledWriter::new(release_rx));
        let mut pending =
            PendingWriter::new(Arc::clone(&writer), ThreadedRuntimeImpl::new_tokio_rt(), 1);

        assert_eq!(
            pending.write_range(0..1, Bytes::from_static(b"a")).unwrap(),
            WriteStatus::Dispatched
        );
        assert_eq!(
            pending.write_range(1..2, Bytes::from_static(b"b")).unwrap(),
            WriteStatus::Pending
        );

        let err = pending
            .write_range(2..3, Bytes::from_static(b"c"))
            .unwrap_err();
        assert_eq!(err.0.range, 2..3);
        assert_eq!(&err.0.bytes[..], b"c");

        release_tx.send(()).await.unwrap();
        let _ = next_tick(&mut pending).await;
        release_tx.send(()).await.unwrap();
        let _ = next_tick(&mut pending).await;
        assert!(pending.is_idle());
    }

    #[tokio::test]
    async fn zero_capacity_is_clamped_to_one() {
        let (release_tx, release_rx) = async_channel::unbounded();
        let writer = Arc::new(ControlledWriter::new(release_rx));
        let mut pending =
            PendingWriter::new(Arc::clone(&writer), ThreadedRuntimeImpl::new_tokio_rt(), 0);

        assert!(!pending.is_full());
        assert_eq!(
            pending.write_range(0..1, Bytes::from_static(b"a")).unwrap(),
            WriteStatus::Dispatched
        );
        assert_eq!(pending.in_flight_count(), 1);
        assert!(pending.is_full());
        assert_eq!(
            pending.write_range(1..2, Bytes::from_static(b"b")).unwrap(),
            WriteStatus::Pending
        );

        release_tx.send(()).await.unwrap();
        let _ = next_tick(&mut pending).await;
        release_tx.send(()).await.unwrap();
        let _ = next_tick(&mut pending).await;
        assert!(pending.is_idle());
    }

    #[tokio::test]
    async fn spawn_error_decrements_in_flight_and_reports_completion() {
        let writer = Arc::new(RecordingWriter::default());
        let rt = ThreadedRuntimeImpl::new_other_rt(FailingRuntime);
        let mut pending = PendingWriter::new(Arc::clone(&writer), rt, 1);

        assert_eq!(
            pending
                .write_range(0..4, Bytes::from_static(b"boom"))
                .unwrap(),
            WriteStatus::Dispatched
        );
        assert_eq!(pending.in_flight_count(), 1);

        let completions = next_tick(&mut pending).await;
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].range, 0..4);
        assert!(completions[0].result.is_err());
        assert!(writer.writes().is_empty());
        assert!(pending.is_idle());
    }

    #[tokio::test]
    async fn panic_guard_decrements_in_flight_and_reports_completion() {
        let writer = Arc::new(PanicWriter);
        let mut pending =
            PendingWriter::new(Arc::clone(&writer), ThreadedRuntimeImpl::new_tokio_rt(), 1);

        assert_eq!(
            pending
                .write_range(0..4, Bytes::from_static(b"boom"))
                .unwrap(),
            WriteStatus::Dispatched
        );
        assert_eq!(pending.in_flight_count(), 1);

        let completions = next_tick(&mut pending).await;
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].range, 0..4);
        assert!(completions[0].result.is_err());
        assert!(pending.is_idle());
    }

    #[tokio::test]
    async fn write_error_prevents_pending_dispatch() {
        let (release_tx, release_rx) = async_channel::unbounded();
        let writer = Arc::new(ControlledFailingWriter { release_rx });
        let mut pending =
            PendingWriter::new(Arc::clone(&writer), ThreadedRuntimeImpl::new_tokio_rt(), 1);

        assert_eq!(
            pending
                .write_range(0..10, Bytes::from_static(b"will_fail_"))
                .unwrap(),
            WriteStatus::Dispatched
        );
        assert_eq!(
            pending
                .write_range(10..20, Bytes::from_static(b"should_not"))
                .unwrap(),
            WriteStatus::Pending
        );

        release_tx.send(()).await.unwrap();
        let completions = next_tick(&mut pending).await;
        assert_eq!(completions.len(), 1);
        assert!(completions[0].result.is_err());
        assert!(pending.has_pending_write());
        assert_eq!(pending.in_flight_count(), 0);
        assert!(!pending.can_write());
    }

    #[tokio::test]
    async fn slow_writer_backpressure_no_overflow() {
        let writer = Arc::new(DelayWriter {
            delay: Duration::from_millis(50),
        });
        let mut pending =
            PendingWriter::new(Arc::clone(&writer), ThreadedRuntimeImpl::new_tokio_rt(), 2);

        pending.write_range(0..1, Bytes::from_static(b"a")).unwrap();
        pending.write_range(1..2, Bytes::from_static(b"b")).unwrap();
        assert!(pending.is_full());
        assert!(pending.can_write());

        pending.write_range(2..3, Bytes::from_static(b"c")).unwrap();
        assert!(!pending.can_write());

        let err = pending
            .write_range(3..4, Bytes::from_static(b"d"))
            .unwrap_err();
        assert_eq!(err.0.range, 3..4);

        let completions = tokio::time::timeout(Duration::from_secs(5), pending.flush())
            .await
            .expect("flush timed out");
        assert_eq!(completions.len(), 3);
        assert!(
            completions
                .iter()
                .all(|completion| completion.result.is_ok())
        );
        assert!(pending.is_idle());
    }
}
