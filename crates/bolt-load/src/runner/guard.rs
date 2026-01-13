use smol_cancellation_token::CancellationToken;

use super::ControlSignalReceiver;
use crate::task::{ControlEvent, RunnerId};

/// Type alias for the control signal sender (point-to-point channel)
pub type ControlSignalSender = async_channel::Sender<ControlEvent>;

pub struct TaskRunnerGuard {
    runner_id: RunnerId,
    cancel_token: CancellationToken,
    control_signal: ControlSignalSender,
}

impl TaskRunnerGuard {
    pub fn new(
        runner_id: RunnerId,
        cancel_token: CancellationToken,
        control_signal: ControlSignalSender,
    ) -> Self {
        Self {
            runner_id,
            cancel_token,
            control_signal,
        }
    }

    /// Get the runner ID associated with this guard
    pub fn runner_id(&self) -> RunnerId {
        self.runner_id
    }

    // TODO: support a callback to waiting the task runner to be cancelled?
    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }

    /// Send a control message to the runner.
    /// Since we now use point-to-point channels, the message goes directly to the specific runner.
    pub fn send_message(&self, message: ControlEvent) {
        self.control_signal
            .try_send(message)
            .expect("Manager control channel should never full or closed");
    }

    /// Create a control channel pair for a new runner
    pub fn create_control_channel() -> (ControlSignalSender, ControlSignalReceiver) {
        async_channel::bounded(8)
    }
}
impl Drop for TaskRunnerGuard {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}
