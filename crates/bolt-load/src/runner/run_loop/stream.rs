use std::{
    cmp::Ordering,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use bolt_load_utils::telemetry::*;
use bytes::{Bytes, BytesMut};

use super::{
    Continuation, Phase, RunResult, RunnerLoop,
    emit::{Emission, EmitResult},
};
use crate::{
    adapter::AdapterError,
    runner::{BUFFER_SIZE, RunnerInput, TaskError, TaskRunner},
    runtime::{Deadline, TimerSource, instant_after},
};

/// Reading the stream into the buffer, or emitting the buffer.
pub(super) struct StreamingState {
    /// Fires when the stream yields no non-empty item in time; paused while emitting.
    idle: Deadline,
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
    /// Idle from now on `source`'s clock.
    pub(super) fn new(idle_timeout: Duration, source: &TimerSource) -> Self {
        Self {
            idle: source.deadline_after(source.now(), idle_timeout),
            buffer: BytesMut::with_capacity(BUFFER_SIZE),
            item: None,
            staged: None,
        }
    }

    #[inline]
    pub(super) fn is_emitting(&self) -> bool {
        self.staged.is_some()
    }

    /// The buffer being emitted, and whether the runner's own stop may
    /// interrupt it: only while more of the current item follows, so the
    /// runner has not decided how it ends.
    #[inline]
    pub(super) fn staged_mut(&mut self) -> Option<(&mut Emission, bool)> {
        self.staged.as_mut().map(|(emission, then)| {
            let stoppable = matches!(then, Continuation::ResumeItem);
            (emission, stoppable)
        })
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
    ///
    /// An unknown total accepts any length, zero included: a stream that ends
    /// without an error is complete as far as the HTTP framing can tell, so an
    /// empty body is an empty file (#153).
    pub(super) fn validate_size(&self) -> RunResult {
        match self.total {
            Some(total) => match total.cmp(&self.downloaded) {
                Ordering::Equal => Ok(()),
                // Also a valid 206 whose `Content-Range` ends before the
                // requested end. The task treats it as transient and requests
                // the rest from the received offset.
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
            None => Ok(()),
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
        self.streaming().idle.poll(cx)
    }

    #[inline]
    pub(super) fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, AdapterError>>> {
        let RunnerInput::Connected(stream) = &mut self.runner.input else {
            unreachable!("streaming before connecting");
        };
        stream.as_mut().poll_next(cx)
    }

    pub(super) fn on_item(&mut self, item: Bytes) {
        if !item.is_empty() {
            let now = self.runner.timer_source.now();
            if let Some(activity) = &self.runner.read_activity {
                activity.note_at(now);
            }
            self.restart_idle(now);
        }
        let state = self.streaming();
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

    /// The idle timeout runs again from `now`.
    fn restart_idle(&mut self, now: Instant) {
        let at = instant_after(now, self.runner.idle_timeout);
        self.streaming().idle.reset(at);
    }

    /// After a full buffer of the current item was pushed.
    pub(super) fn resume_item(&mut self) {
        // Backpressure is not idleness, nor silence (see `on_emitted`).
        let now = self.runner.timer_source.now();
        self.restart_idle(now);
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
        // Waiting for the consumer is not silence either.
        if let Some(activity) = &self.runner.read_activity {
            activity.start_emitting();
        }
    }

    pub(super) fn on_emitted(&mut self, result: EmitResult) {
        let (_, then) = self.streaming().staged.take().expect("a buffer was staged");
        if let Some(activity) = &self.runner.read_activity {
            // It reads on only past a full buffer of its item, as in
            // `resume_item`; a push that ends it is no read.
            if matches!(then, Continuation::ResumeItem) && matches!(result, Ok(None)) {
                activity.resume_reading(self.runner.timer_source.now());
            } else {
                activity.stop_emitting();
            }
        }
        self.resume_after_emit(then, result);
    }
}
