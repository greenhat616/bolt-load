//! Pending writer for file writer
//!
//! Manages write dispatch with backlog control. Writes are dispatched to a
//! background runtime up to a configurable concurrency limit. When at capacity,
//! a single write can be buffered as "pending" and will be auto-flushed once
//! a slot opens. Callers should check [`is_full`] / [`has_pending_write`] to
//! decide whether to submit more writes.

use std::{ops::Range, path::PathBuf, sync::Arc};

use async_channel::{Receiver, Sender};
use bytes::Bytes;
use futures::task::SpawnExt;

use super::{Chunk, CommandError, FileRangeWriter, FileWriterError};
use crate::runtime::ThreadedRuntimeImpl;

/// A single write request: range + data.
#[derive(derive_more::Debug, Clone)]
pub struct PendingWrite {
    #[debug("{}..{}", range.start, range.end)]
    pub range: Range<u64>,
    #[debug("{} bytes", bytes.len())]
    pub bytes: Bytes,
}

/// Result of a completed write, returned from [`PendingWriter::tick`].
#[derive(Debug)]
pub struct WriteCompletion {
    pub range: Range<u64>,
    pub result: Result<(), FileWriterError>,
}

/// Indicates what happened when [`PendingWriter::write_range`] was called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStatus {
    /// The write was dispatched to the background runtime immediately.
    Dispatched,
    /// The backlog was full; the write was buffered as a pending write
    /// and will be auto-dispatched when a slot opens.
    Pending,
}

/// Error returned when a write cannot be accepted at all.
#[derive(Debug, thiserror::Error)]
#[error("writer is full and a pending write already exists")]
pub struct WriterFullError(pub PendingWrite);

#[derive(derive_more::Debug)]
pub struct PendingWriter<F> {
    writer: Arc<F>,
    #[debug(skip)]
    threaded_rt: ThreadedRuntimeImpl,
    capacity: usize,

    in_flight: usize,

    /// Channel for receiving write completions.
    #[debug(skip)]
    completion_tx: Sender<WriteCompletion>,
    #[debug(skip)]
    completion_rx: Receiver<WriteCompletion>,

    /// At most one write can be buffered when the backlog is full.
    pending_write: Option<PendingWrite>,

    /// Once a write error is observed, stop dispatching pending writes.
    saw_error: bool,
}

impl<F> PendingWriter<F>
where
    F: FileRangeWriter + 'static,
{
    pub fn new(writer: Arc<F>, threaded_rt: ThreadedRuntimeImpl, capacity: usize) -> Self {
        let capacity = capacity.max(1);

        // +1 ensures the spawn-failure error path can always deliver a
        // completion even when all normal slots are unconsumed.
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

    /// `true` when the number of in-flight writes has reached `capacity`.
    #[inline]
    pub fn is_full(&self) -> bool {
        self.in_flight_count() >= self.capacity
    }

    /// `true` when there is a buffered write waiting for a free slot.
    #[inline]
    pub fn has_pending_write(&self) -> bool {
        self.pending_write.is_some()
    }

    /// `true` when there are no in-flight writes **and** no pending write.
    #[inline]
    pub fn is_idle(&self) -> bool {
        self.in_flight_count() == 0 && !self.has_pending_write()
    }

    /// The caller should check this before calling [`write_range`]:
    /// returns `true` when the writer can accept a new write without error.
    #[inline]
    pub fn can_write(&self) -> bool {
        // Either we have a free slot, or at worst we can store one pending write.
        !self.is_full() || !self.has_pending_write()
    }

    /// Submit a write.
    ///
    /// - If a slot is available, the write is dispatched immediately → [`WriteStatus::Dispatched`].
    /// - If at capacity but no pending write exists, it is buffered → [`WriteStatus::Pending`].
    /// - If at capacity **and** a pending write already exists, returns [`WriterFullError`]
    ///   containing the rejected [`PendingWrite`] so the caller can decide what to do.
    ///
    /// Callers are encouraged to check [`can_write`] before calling.
    pub fn write_range(
        &mut self,
        range: Range<u64>,
        data: Bytes,
    ) -> Result<WriteStatus, WriterFullError> {
        if !self.is_full() {
            self.dispatch_write(range, data);
            Ok(WriteStatus::Dispatched)
        } else if self.pending_write.is_none() {
            self.pending_write = Some(PendingWrite { range, bytes: data });
            Ok(WriteStatus::Pending)
        } else {
            Err(WriterFullError(PendingWrite { range, bytes: data }))
        }
    }

    /// Non-blocking: drain all available completions and auto-flush any
    /// pending write if a slot has opened.
    ///
    /// Returns an empty `Vec` if nothing has completed yet.
    /// Skips auto-flush once any write error has been observed.
    pub fn try_tick(&mut self) -> Vec<WriteCompletion> {
        let completions = self.drain_completions();
        self.saw_error |= Self::completions_have_error(&completions);
        if !self.saw_error {
            self.try_flush_pending();
        }
        completions
    }

    /// Async: wait for **at least one** completion, then drain any others
    /// that are immediately available. Auto-flushes the pending write
    /// if a slot opens, unless an error has been observed.
    ///
    /// Returns `None` only if all senders have been dropped (should not
    /// happen while `self` is alive).
    pub async fn tick(&mut self) -> Option<Vec<WriteCompletion>> {
        let first = self.completion_rx.recv().await.ok()?;
        let mut completions = vec![first];
        while let Ok(c) = self.completion_rx.try_recv() {
            completions.push(c);
        }
        self.ack_completions(completions.len());
        self.saw_error |= Self::completions_have_error(&completions);
        if !self.saw_error {
            self.try_flush_pending();
        }
        Some(completions)
    }

    /// Async: wait until **all** in-flight writes have completed.
    /// Returns every completion in order received.
    /// Stops dispatching the pending write once an error is observed.
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
                Ok(c) => {
                    self.ack_completions(1);
                    self.saw_error |= c.result.is_err();
                    completions.push(c);
                }
                Err(_) => break,
            }
        }

        completions.extend(self.drain_completions());
        completions
    }

    // ── internals ────────────────────────────────────────────────────

    fn completions_have_error(completions: &[WriteCompletion]) -> bool {
        completions.iter().any(|c| c.result.is_err())
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
        let completion_chunk = Chunk {
            range,
            data: data.clone(),
        };

        if let Err(spawn_error) = self.threaded_rt.spawn(async move {
            let result = writer.write_range(task_range.clone(), data).await;
            let _ = tx
                .send(WriteCompletion {
                    range: task_range,
                    result,
                })
                .await;
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

    /// If a slot has opened and we have a buffered write, dispatch it.
    fn try_flush_pending(&mut self) {
        if !self.is_full() {
            if let Some(pw) = self.pending_write.take() {
                self.dispatch_write(pw.range, pw.bytes);
            }
        }
    }

    fn drain_completions(&mut self) -> Vec<WriteCompletion> {
        let mut completions = Vec::new();
        while let Ok(c) = self.completion_rx.try_recv() {
            completions.push(c);
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

    use super::*;
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
        async fn write_range(&self, range: Range<u64>, data: Bytes) -> Result<(), FileWriterError> {
            self.writes.lock().unwrap().push((range, data));
            Ok(())
        }

        async fn finalize(self) -> Result<(), FileWriterError> {
            Ok(())
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
        async fn write_range(&self, range: Range<u64>, data: Bytes) -> Result<(), FileWriterError> {
            let _ = self.release_rx.recv().await;
            self.writes.lock().unwrap().push((range, data));
            Ok(())
        }

        async fn finalize(self) -> Result<(), FileWriterError> {
            Ok(())
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

    struct NoopTimer;

    #[async_trait::async_trait]
    impl ObjectSafeTimer for NoopTimer {
        async fn tick(&mut self) {}
    }

    async fn next_tick<F>(writer: &mut PendingWriter<F>) -> Vec<WriteCompletion>
    where
        F: FileRangeWriter + 'static,
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
    async fn pending_buffer_fallback_flushes_after_capacity_opens() {
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
    async fn writer_full_error_boundary_rejects_third_write() {
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

        match &completions[0].result {
            Err(FileWriterError::WriteRange {
                source,
                chunk,
                path,
            }) => {
                assert!(matches!(source, CommandError::Io { .. }));
                let chunk = chunk.as_ref().unwrap();
                assert_eq!(chunk.range, 0..4);
                assert_eq!(&chunk.data[..], b"boom");
                assert!(path.as_os_str().is_empty());
            }
            other => panic!("expected WriteRange spawn failure, got {other:?}"),
        }

        assert!(writer.writes().is_empty());
        assert!(pending.is_idle());
    }

    // ── FailingWriter fixture ────────────────────────────────────────

    #[derive(Debug)]
    struct FailingWriter {
        fail_on_index: usize,
        write_count: Mutex<usize>,
    }

    impl FailingWriter {
        fn new(fail_on_index: usize) -> Self {
            Self {
                fail_on_index,
                write_count: Mutex::new(0),
            }
        }
    }

    impl FileRangeWriter for FailingWriter {
        async fn write_range(&self, range: Range<u64>, data: Bytes) -> Result<(), FileWriterError> {
            let idx = {
                let mut count = self.write_count.lock().unwrap();
                let idx = *count;
                *count += 1;
                idx
            };
            if idx == self.fail_on_index {
                return Err(FileWriterError::WriteRange {
                    source: CommandError::Io {
                        source: std::io::Error::other("injected failure"),
                    },
                    chunk: Some(Chunk { range, data }),
                    path: PathBuf::new(),
                });
            }
            Ok(())
        }

        async fn finalize(self) -> Result<(), FileWriterError> {
            Ok(())
        }
    }

    // ── ControlledFailingWriter fixture ──────────────────────────────

    #[derive(Debug)]
    struct ControlledFailingWriter {
        release_rx: Receiver<()>,
    }

    impl FileRangeWriter for ControlledFailingWriter {
        async fn write_range(&self, range: Range<u64>, data: Bytes) -> Result<(), FileWriterError> {
            let _ = self.release_rx.recv().await;
            Err(FileWriterError::WriteRange {
                source: CommandError::Io {
                    source: std::io::Error::other("injected failure"),
                },
                chunk: Some(Chunk { range, data }),
                path: PathBuf::new(),
            })
        }

        async fn finalize(self) -> Result<(), FileWriterError> {
            Ok(())
        }
    }

    // ── DelayWriter fixture ──────────────────────────────────────────

    #[derive(Debug)]
    struct DelayWriter {
        delay: Duration,
    }

    impl FileRangeWriter for DelayWriter {
        async fn write_range(
            &self,
            _range: Range<u64>,
            _data: Bytes,
        ) -> Result<(), FileWriterError> {
            tokio::time::sleep(self.delay).await;
            Ok(())
        }

        async fn finalize(self) -> Result<(), FileWriterError> {
            Ok(())
        }
    }

    // ── P0 tests ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn flush_drains_all_completed_writes() {
        let writer = Arc::new(RecordingWriter::default());
        let mut pending =
            PendingWriter::new(Arc::clone(&writer), ThreadedRuntimeImpl::new_tokio_rt(), 4);

        for i in 0u64..4 {
            let start = i * 10;
            pending
                .write_range(start..start + 10, Bytes::from(vec![i as u8; 10]))
                .unwrap();
        }

        let completions = tokio::time::timeout(Duration::from_secs(5), pending.flush())
            .await
            .expect("flush timed out");
        assert_eq!(completions.len(), 4);
        assert!(completions.iter().all(|c| c.result.is_ok()));
        assert!(pending.is_idle());
    }

    #[tokio::test]
    async fn flush_reports_write_error() {
        let writer = Arc::new(FailingWriter::new(0));
        let mut pending =
            PendingWriter::new(Arc::clone(&writer), ThreadedRuntimeImpl::new_tokio_rt(), 2);

        pending
            .write_range(0..10, Bytes::from_static(b"will_fail_"))
            .unwrap();

        let completions = tokio::time::timeout(Duration::from_secs(5), pending.flush())
            .await
            .expect("flush timed out");
        assert_eq!(completions.len(), 1);
        assert!(completions[0].result.is_err());
    }

    #[tokio::test]
    async fn write_error_prevents_pending_dispatch() {
        let (release_tx, release_rx) = async_channel::unbounded();
        let writer = Arc::new(ControlledFailingWriter { release_rx });
        let mut pending =
            PendingWriter::new(Arc::clone(&writer), ThreadedRuntimeImpl::new_tokio_rt(), 1);

        // First write dispatched — blocks until released, then fails.
        assert_eq!(
            pending
                .write_range(0..10, Bytes::from_static(b"will_fail_"))
                .unwrap(),
            WriteStatus::Dispatched
        );
        // Second write goes into pending buffer (first is still blocked).
        assert_eq!(
            pending
                .write_range(10..20, Bytes::from_static(b"should_not"))
                .unwrap(),
            WriteStatus::Pending
        );

        // Release the first write to let it fail.
        release_tx.send(()).await.unwrap();

        // tick sees the error → should NOT auto-flush pending.
        let completions = next_tick(&mut pending).await;
        assert_eq!(completions.len(), 1);
        assert!(completions[0].result.is_err());
        // The pending write should still be buffered (not dispatched).
        assert!(pending.has_pending_write());
        assert_eq!(pending.in_flight_count(), 0);
    }

    #[tokio::test]
    async fn error_persists_across_tick_and_flush() {
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
        let _ = next_tick(&mut pending).await;
        assert!(pending.has_pending_write());

        // A subsequent flush should NOT dispatch the pending write either.
        let completions = tokio::time::timeout(Duration::from_secs(1), pending.flush())
            .await
            .expect("flush timed out");
        assert!(completions.is_empty());
        assert!(pending.has_pending_write());
    }

    // ── P1 tests ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn slow_writer_backpressure_no_overflow() {
        let writer = Arc::new(DelayWriter {
            delay: Duration::from_millis(50),
        });
        let mut pending =
            PendingWriter::new(Arc::clone(&writer), ThreadedRuntimeImpl::new_tokio_rt(), 2);

        // Fill capacity.
        pending.write_range(0..1, Bytes::from_static(b"a")).unwrap();
        pending.write_range(1..2, Bytes::from_static(b"b")).unwrap();
        assert!(pending.is_full());
        assert!(pending.can_write()); // can still accept one pending

        // Fill pending slot.
        pending.write_range(2..3, Bytes::from_static(b"c")).unwrap();
        assert!(!pending.can_write());

        // Reject when truly full.
        let err = pending
            .write_range(3..4, Bytes::from_static(b"d"))
            .unwrap_err();
        assert_eq!(err.0.range, 3..4);

        // Drain everything.
        let completions = tokio::time::timeout(Duration::from_secs(5), pending.flush())
            .await
            .expect("flush timed out");
        assert_eq!(completions.len(), 3);
        assert!(completions.iter().all(|c| c.result.is_ok()));
        assert!(pending.is_idle());
    }

    #[tokio::test]
    async fn high_concurrency_in_flight_consistency() {
        let writer = Arc::new(DelayWriter {
            delay: Duration::from_millis(10),
        });
        let capacity = 8;
        let mut pending = PendingWriter::new(
            Arc::clone(&writer),
            ThreadedRuntimeImpl::new_tokio_rt(),
            capacity,
        );

        for i in 0u64..16 {
            while !pending.can_write() {
                let _ = next_tick(&mut pending).await;
            }
            assert!(pending.in_flight_count() <= capacity);
            pending
                .write_range(i * 10..(i + 1) * 10, Bytes::from(vec![i as u8; 10]))
                .unwrap();
        }

        let completions = tokio::time::timeout(Duration::from_secs(10), pending.flush())
            .await
            .expect("flush timed out");
        // Total completions = those drained during the loop + those from flush.
        // Just verify we're idle and counter is back at 0.
        assert!(pending.is_idle());
        assert_eq!(pending.in_flight_count(), 0);
        // We should have gotten some completions from flush.
        assert!(!completions.is_empty());
    }
}
