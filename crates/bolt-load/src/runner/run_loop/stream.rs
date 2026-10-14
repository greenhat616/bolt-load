use std::{
    cmp::Ordering,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use bolt_load_utils::telemetry::*;
use bytes::{Bytes, BytesMut};

use super::{
    Continuation, Phase, RunResult, RunnerLoop,
    emit::{Emission, EmitResult},
};
use crate::{
    adapter::AdapterError,
    runner::{BUFFER_SIZE, TaskError, TaskRunner},
};

/// Reading the stream into the buffer, or emitting the buffer.
pub(super) struct StreamingState {
    /// Fires when the stream yields no non-empty item in time; paused while emitting.
    idle: async_io::Timer,
    buffer: BytesMut,
    /// The part of the current item not buffered yet.
    item: Option<ItemCursor>,
    /// The buffer being emitted, and what follows its push.
    staged: Option<(Emission, Continuation)>,
}

struct ItemCursor {
    bytes: Bytes,
    offset: usize,
}

impl StreamingState {
    pub(super) fn new(idle_timeout: Duration) -> Self {
        Self {
            idle: async_io::Timer::after(idle_timeout),
            buffer: BytesMut::with_capacity(BUFFER_SIZE),
            item: None,
            staged: None,
        }
    }

    #[inline]
    pub(super) fn is_emitting(&self) -> bool {
        self.staged.is_some()
    }

    #[inline]
    pub(super) fn staged_mut(&mut self) -> Option<&mut Emission> {
        self.staged.as_mut().map(|(emission, _)| emission)
    }
}

impl TaskRunner {
    /// Bytes that may still be buffered before reaching the size limit.
    fn allowance(&self, buff: &BytesMut) -> usize {
        self.total.map_or(usize::MAX, |total| {
            total.saturating_sub(self.downloaded + buff.len() as u64) as usize
        })
    }

    /// Check the delivered bytes against the size limit once the stream ended
    /// or reached it.
    pub(super) fn validate_size(&self) -> RunResult {
        match self.total {
            Some(total) => match total.cmp(&self.downloaded) {
                Ordering::Equal => Ok(()),
                Ordering::Greater => {
                    warn!(
                        "runner: downloaded content is smaller than the total size, total: {}, \
                         downloaded: {}",
                        total, self.downloaded
                    );
                    Err(TaskError::SmallerThanTotalSize.into())
                }
                Ordering::Less => {
                    warn!(
                        "runner: downloaded content is larger than the total size, total: {}, \
                         downloaded: {}",
                        total, self.downloaded
                    );
                    Err(TaskError::ExceededTotalSize.into())
                }
            },
            None if self.downloaded > 0 => Ok(()),
            None => Err(TaskError::Empty.into()),
        }
    }
}

impl RunnerLoop<'_> {
    fn streaming(&mut self) -> &mut StreamingState {
        let Phase::Streaming(state) = &mut self.phase else {
            unreachable!("not streaming");
        };
        state
    }

    #[inline]
    pub(super) fn poll_idle(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        Pin::new(&mut self.streaming().idle).poll(cx).map(drop)
    }

    #[inline]
    pub(super) fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, AdapterError>>> {
        self.runner.stream.as_mut().poll_next(cx)
    }

    pub(super) fn on_item(&mut self, item: Bytes) {
        let idle_timeout = self.runner.idle_timeout;
        let state = self.streaming();
        if !item.is_empty() {
            state.idle.set_after(idle_timeout);
        }
        state.item = Some(ItemCursor {
            bytes: item,
            offset: 0,
        });
        self.buffer_item();
    }

    /// Buffer the current item, emitting every full buffer.
    ///
    /// The size limit is re-read after each push, since the manager may lower
    /// it meanwhile; bytes already buffered are kept.
    fn buffer_item(&mut self) {
        let Phase::Streaming(state) = &mut self.phase else {
            unreachable!("not streaming");
        };
        let allowed = self.runner.allowance(&state.buffer);
        if allowed == 0 {
            return self.flush(Continuation::Validate);
        }
        let item = state.item.as_mut().expect("an item is being buffered");
        if item.offset == item.bytes.len() {
            state.item = None;
            return;
        }
        let writable = (item.bytes.len() - item.offset).min(allowed);
        if state.buffer.len() + writable <= BUFFER_SIZE {
            state
                .buffer
                .extend_from_slice(&item.bytes[item.offset..item.offset + writable]);
            if writable == allowed {
                // The item, and whatever owns its bytes, outlives the final push.
                return self.flush(Continuation::Validate);
            }
            state.item = None;
            return;
        }
        let take = BUFFER_SIZE - state.buffer.len();
        state
            .buffer
            .extend_from_slice(&item.bytes[item.offset..item.offset + take]);
        item.offset += take;
        self.flush(Continuation::ResumeItem);
    }

    /// After a full buffer of the current item was pushed.
    pub(super) fn resume_item(&mut self) {
        let idle_timeout = self.runner.idle_timeout;
        // Backpressure is not idleness.
        self.streaming().idle.set_after(idle_timeout);
        self.buffer_item();
    }

    /// Emit the buffered bytes, if any, then continue with `then`.
    pub(super) fn flush(&mut self, then: Continuation) {
        let state = self.streaming();
        if state.buffer.is_empty() {
            return self.resume_after_emit(then, Ok(None));
        }
        let emission = Emission::new(state.buffer.split().freeze());
        state.staged = Some((emission, then));
    }

    pub(super) fn on_emitted(&mut self, result: EmitResult) {
        let (_, then) = self.streaming().staged.take().expect("a buffer was staged");
        self.resume_after_emit(then, result);
    }
}
