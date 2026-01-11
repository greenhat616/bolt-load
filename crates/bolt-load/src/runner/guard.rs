use async_broadcast::Sender;
use bolt_load_utils::telemetry::*;
use smol_cancellation_token::CancellationToken;

use super::ManagerMessagesVariant;
use crate::task::{ManagerMessage, RunnerId};

pub struct TaskRunnerGuard {
    runner_id: RunnerId,
    cancel_token: CancellationToken,
    control_signal: Sender<ManagerMessage>,
}

impl TaskRunnerGuard {
    pub fn new(
        runner_id: RunnerId,
        cancel_token: CancellationToken,
        control_signal: Sender<ManagerMessage>,
    ) -> Self {
        Self {
            runner_id,
            cancel_token,
            control_signal,
        }
    }

    // TODO: support a callback to waiting the task runner to be cancelled?
    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }

    pub fn send_message(&self, message: ManagerMessagesVariant) {
        self.control_signal
            .try_broadcast(ManagerMessage(self.runner_id, message))
            .expect("Manager control channel should never full or closed");
    }
}
impl Drop for TaskRunnerGuard {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}
