use std::{
    ops::ControlFlow,
    task::{Context, Poll},
    time::Instant,
};

use super::{
    super::{
        file_writer::WriterFullError,
        runner_manager::{DownloadedChunk, SideFailed, TaskState},
    },
    DownloadLoop, LoopEvent, RunnerTick,
};
use crate::{
    adapter::{AdapterError, UnretryableError},
    runner::TaskError,
    task::{DownloadError, FallbackReason, instance::Result},
};

impl DownloadLoop {
    /// Runner data is only polled while the writer can take it; their
    /// lifecycle always is, so failures are noticed under backpressure.
    #[inline]
    pub(super) fn poll_runner(&mut self, cx: &mut Context<'_>) -> Poll<LoopEvent> {
        let data_enabled = self.writer.can_write();
        self.manager
            .poll_tick(cx, data_enabled)
            .map(LoopEvent::Runner)
    }

    /// Handle a runner event at `now`; a failure the task survives backs
    /// its range off.
    ///
    /// The other attempts in flight are counted once the failed runner is
    /// released, without the runners the loop stopped (design §9.6, M3).
    pub(super) fn on_runner(&mut self, tick: RunnerTick, now: Instant) -> Result<ControlFlow<()>> {
        if let Some(chunk) = tick.downloaded {
            self.accept_chunk(chunk, now)?;
        }
        if let Some(failure) = tick.side_failure {
            self.on_side_failure(&failure, now)?;
        }
        if let Some(failure) = tick.failure {
            if let Some(e) = mode_ending(&failure.error) {
                return Err(e);
            }
            if let Some(e) = self.policy.on_failure(&failure, self.manager.in_flight()) {
                return Err(DownloadError::transfer(e));
            }
            self.back_off(&failure, now);
        }
        Ok(if tick.state == TaskState::Finished {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        })
    }

    /// Write a chunk the planner accepted; a non-empty one is progress,
    /// and ends its runner's silence.
    fn accept_chunk(&mut self, chunk: DownloadedChunk, now: Instant) -> Result<()> {
        let progressed = !chunk.range.is_empty();
        self.writer
            .write_range(chunk.range, chunk.bytes)
            .map_err(writer_full)?;
        if progressed {
            self.policy.on_progress(now);
            self.manager.note_delivery(chunk.runner_id, now);
        }
        Ok(())
    }

    /// A side of an undecided pair failed at `now` while the other runs on
    /// (design §9.6): nothing is released or backed off, but the task
    /// falls back or ends as for any runner, and a congestion holds new
    /// connections back.
    fn on_side_failure(&mut self, failure: &SideFailed, now: Instant) -> Result<()> {
        if let Some(e) = mode_ending(&failure.error) {
            return Err(e);
        }
        if let Some(e) = self.policy.on_side_failure(failure) {
            return Err(DownloadError::transfer(e));
        }
        if self
            .backoff
            .hold_congestion(&failure.error, failure.attempts, now)
            && let Some(at) = self.next_retry(now)
        {
            self.deadline.wake_by(at);
        }
        Ok(())
    }
}

/// The error that ends the concurrent mode whatever else runs, when
/// `error` shows that the resource changed (design §11.2, D12) or that
/// ranges cannot carry the file (§4.3).
fn mode_ending(error: &TaskError) -> Option<DownloadError> {
    resource_changed(error).or_else(|| range_unsupported(error))
}

/// [`DownloadError::ResourceChanged`], when a range request found the
/// resource changed: the task downloads it again from scratch, not over
/// one connection (design D12).
fn resource_changed(error: &TaskError) -> Option<DownloadError> {
    match error {
        TaskError::StreamError {
            source:
                source @ AdapterError::Unretryable {
                    source: UnretryableError::ResourceChanged,
                },
        } => Some(DownloadError::ResourceChanged {
            source: source.clone(),
        }),
        _ => None,
    }
}

/// The error that ends the concurrent mode for the single-connection
/// fallback, when `error` shows that ranges cannot carry the file.
///
/// Only these exact causes count: any other range mismatch may be a changed
/// resource, which one connection would not fix.
fn range_unsupported(error: &TaskError) -> Option<DownloadError> {
    let TaskError::StreamError {
        source: source @ AdapterError::Unretryable { source: cause },
    } = error
    else {
        return None;
    };
    let reason = match cause {
        UnretryableError::RangeIgnored => FallbackReason::RangeIgnored,
        UnretryableError::RangeContentCoded { .. }
        | UnretryableError::RangeBodyTooLong { .. }
        | UnretryableError::RangeBodyTooShort { .. }
        | UnretryableError::BodyUndecodable { .. } => FallbackReason::ContentCoded,
        _ => return None,
    };
    Some(DownloadError::RangeUnsupported {
        reason,
        source: source.clone(),
    })
}

#[cold]
fn writer_full(e: WriterFullError) -> DownloadError {
    DownloadError::transfer(TaskError::Other {
        message: format!(
            "writer full (invariant violation): {:?}",
            e.pending_write.range
        ),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bolt_load_core::adapter::{ContentRange, RetryableError};

    use super::*;

    fn reason(error: impl Into<AdapterError>) -> Option<FallbackReason> {
        let error = TaskError::StreamError {
            source: error.into(),
        };
        match range_unsupported(&error)? {
            DownloadError::RangeUnsupported { reason, .. } => Some(reason),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// A changed resource ends the mode as such, never as a fallback
    /// (design §11.2, D12).
    #[test]
    fn a_changed_resource_ends_the_mode_without_a_fallback() {
        let changed = TaskError::StreamError {
            source: UnretryableError::ResourceChanged.into(),
        };
        assert!(range_unsupported(&changed).is_none());
        assert!(matches!(
            mode_ending(&changed),
            Some(DownloadError::ResourceChanged { .. })
        ));
        let ignored = TaskError::StreamError {
            source: UnretryableError::RangeIgnored.into(),
        };
        assert!(matches!(
            mode_ending(&ignored),
            Some(DownloadError::RangeUnsupported { .. })
        ));
        assert!(mode_ending(&TaskError::Timeout).is_none());
    }

    #[test]
    fn only_errors_that_rule_out_ranges_fall_back() {
        let got = ContentRange {
            first: 0,
            last: 9,
            complete: Some(10),
        };
        let io = || Arc::new(std::io::Error::from(std::io::ErrorKind::InvalidData));
        assert_eq!(
            reason(UnretryableError::RangeIgnored),
            Some(FallbackReason::RangeIgnored)
        );
        for coded in [
            UnretryableError::RangeContentCoded {
                coding: "gzip".into(),
            },
            UnretryableError::RangeBodyTooLong { got },
            UnretryableError::RangeBodyTooShort { got, received: 4 },
            UnretryableError::BodyUndecodable { source: io() },
        ] {
            assert_eq!(reason(coded), Some(FallbackReason::ContentCoded));
        }
        // A mismatch may be a changed resource; the others are not about
        // ranges at all.
        for other in [
            AdapterError::from(UnretryableError::RangeMismatch {
                requested: 0..10,
                got: Some(got),
            }),
            UnretryableError::ContentCoded {
                coding: "gzip".into(),
            }
            .into(),
            UnretryableError::Io { source: io() }.into(),
            RetryableError::Io { source: io() }.into(),
        ] {
            assert_eq!(reason(other), None);
        }
        assert!(range_unsupported(&TaskError::Timeout).is_none());
    }
}
