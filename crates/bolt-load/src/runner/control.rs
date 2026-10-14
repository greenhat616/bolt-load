use std::task::{Context, Poll};

use bolt_load_utils::telemetry::*;
use futures::Stream;

use super::{TaskError, TaskRunError, TaskRunner};
use crate::task::ControlEvent;

/// Type alias for the control signal sender (point-to-point channel)
pub type ControlSignalSender = async_channel::Sender<ControlEvent>;
/// Type alias for the control signal receiver
/// Using async_channel for point-to-point communication instead of broadcast
pub type ControlSignalReceiver = async_channel::Receiver<ControlEvent>;

impl TaskRunner {
    /// The next control signal; `ChannelClosed` once the manager released the runner.
    #[inline]
    pub(super) fn poll_control(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<ControlEvent, TaskRunError>> {
        self.control_signal
            .as_mut()
            .poll_next(cx)
            .map(|signal| signal.ok_or(TaskRunError::Failed(TaskError::ChannelClosed)))
    }

    /// Apply a control signal to this runner's size limit.
    pub(super) fn apply_limit(&mut self, signal: &ControlEvent) -> Result<(), TaskRunError> {
        Self::apply_control(&mut self.total, signal).map_err(Into::into)
    }

    /// Apply a control signal from the manager to the size limit `total`.
    ///
    /// Since we now use point-to-point channels, the signal is guaranteed to be for this runner.
    pub(super) fn apply_control(
        total: &mut Option<u64>,
        signal: &ControlEvent,
    ) -> Result<(), TaskError> {
        match signal {
            ControlEvent::LimitTotal(new_total) => {
                if let Some(current_total) = *total {
                    if current_total > *new_total {
                        trace!("runner: limit total to {new_total}");
                        *total = Some(*new_total);
                    } else {
                        warn!(
                            "runner: limit total is larger than current total,
                            limit: {new_total}, current: {current_total}"
                        );
                        Err(TaskError::ExceededTotalSize)?;
                    }
                }
            }
        }
        Ok(())
    }
}
