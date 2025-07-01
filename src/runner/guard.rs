use crate::task::{ManagerMessage, RunnerId};

use super::ManagerMessagesVariant;
use async_channel::Sender;
use smol_cancellation_token::CancellationToken;

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

    pub async fn send_message(&self, message: ManagerMessagesVariant) {
        if let Err(e) = self
            .control_signal
            .send(ManagerMessage(self.runner_id, message))
            .await
        {
            tracing::warn!("Failed to send message to task runner: {e:?}");
        }
    }
}
impl Drop for TaskRunnerGuard {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}
