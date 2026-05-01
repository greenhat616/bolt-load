//! Pending writer for file writer
//!
//! Manages write dispatch with backlog control. Writes are dispatched to a
//! background runtime up to a configurable concurrency limit. When at capacity,
//! a single write can be buffered as "pending" and will be auto-flushed once
//! a slot opens. Callers should check [`is_full`] / [`has_pending_write`] to
//! decide whether to submit more writes.

use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_channel::{Receiver, Sender};
use bytes::Bytes;
use futures::task::{Spawn, SpawnExt};

use super::{FileRangeWriter, FileWriterError};
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

    /// Tracks in-flight writes
    in_flight: Arc<AtomicUsize>,

    /// Channel for receiving write completions.
    #[debug(skip)]
    completion_tx: Sender<WriteCompletion>,
    #[debug(skip)]
    completion_rx: Receiver<WriteCompletion>,

    /// At most one write can be buffered when the backlog is full.
    pending_write: Option<PendingWrite>,
}

impl<F> PendingWriter<F>
where
    F: FileRangeWriter + 'static,
{
    pub fn new(writer: Arc<F>, threaded_rt: ThreadedRuntimeImpl, capacity: usize) -> Self {
        // Use a bounded channel equal to capacity – completions are always
        // consumed by tick(), so this will never block in practice.
        let (tx, rx) = async_channel::bounded(capacity.max(1));
        Self {
            writer,
            threaded_rt,
            capacity,
            in_flight: Arc::new(AtomicUsize::new(0)),
            completion_tx: tx,
            completion_rx: rx,
            pending_write: None,
        }
    }

    /// Number of writes currently being executed in the background.
    #[inline]
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
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
    pub fn try_tick(&mut self) -> Vec<WriteCompletion> {
        let completions = self.drain_completions();
        self.try_flush_pending();
        completions
    }

    /// Async: wait for **at least one** completion, then drain any others
    /// that are immediately available. Auto-flushes the pending write
    /// if a slot opens.
    ///
    /// Returns `None` only if all senders have been dropped (should not
    /// happen while `self` is alive).
    pub async fn tick(&mut self) -> Option<Vec<WriteCompletion>> {
        let first = self.completion_rx.recv().await.ok()?;
        let mut completions = vec![first];
        // drain the rest that are ready
        while let Ok(c) = self.completion_rx.try_recv() {
            completions.push(c);
        }
        self.try_flush_pending();
        Some(completions)
    }

    /// Async: wait until **all** in-flight and pending writes have completed.
    /// Returns every completion in order received.
    pub async fn flush(&mut self) -> Vec<WriteCompletion> {
        self.try_flush_pending();
        let mut completions = Vec::new();
        while self.in_flight_count() > 0 {
            match self.completion_rx.recv().await {
                Ok(c) => completions.push(c),
                Err(_) => break,
            }
            // After each completion, re-check if we can flush the pending write.
            self.try_flush_pending();
        }
        completions
    }

    // ── internals ────────────────────────────────────────────────────

    /// Spawn a write task on the threaded runtime.
    fn dispatch_write(&self, range: Range<u64>, data: Bytes) {
        self.in_flight.fetch_add(1, Ordering::AcqRel);

        let writer = Arc::clone(&self.writer);
        let in_flight = Arc::clone(&self.in_flight);
        let tx = self.completion_tx.clone();
        let task_range = range.clone();

        self.threaded_rt.spawn(async move {
            let result = writer.write_range(task_range.clone(), data).await;
            // Decrement before sending so that `is_full` reflects reality
            // by the time the caller processes the completion.
            in_flight.fetch_sub(1, Ordering::AcqRel);
            // If the receiver is dropped we just discard the result.
            let _ = tx
                .send(WriteCompletion {
                    range: task_range,
                    result,
                })
                .await;
        });
    }

    /// If a slot has opened and we have a buffered write, dispatch it.
    fn try_flush_pending(&mut self) {
        if !self.is_full() {
            if let Some(pw) = self.pending_write.take() {
                self.dispatch_write(pw.range, pw.bytes);
            }
        }
    }

    /// Drain all immediately-available completions from the channel.
    fn drain_completions(&mut self) -> Vec<WriteCompletion> {
        let mut completions = Vec::new();
        while let Ok(c) = self.completion_rx.try_recv() {
            completions.push(c);
        }
        completions
    }
}
