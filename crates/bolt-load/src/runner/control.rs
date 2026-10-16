use bolt_load_utils::telemetry::*;

use super::{TaskError, TaskRunner};
use crate::task::ControlEvent;

/// Type alias for the control signal sender (point-to-point channel)
pub type ControlSignalSender = async_channel::Sender<ControlEvent>;
/// Type alias for the control signal receiver
/// Using async_channel for point-to-point communication instead of broadcast
pub type ControlSignalReceiver = async_channel::Receiver<ControlEvent>;

impl TaskRunner {
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
