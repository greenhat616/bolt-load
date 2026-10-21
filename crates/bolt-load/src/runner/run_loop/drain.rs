use async_ringbuf::traits::*;
use futures::FutureExt;

use super::super::{DATA_FRAME_CHANNEL_CAPACITY, TaskError, TaskRunError, TaskRunner};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::runner) enum DataDrainOutcome {
    Drained,
    Cancelled,
    TimedOut,
}

impl TaskRunner {
    pub(super) fn should_wait_data_drain(result: &Result<(), TaskRunError>) -> bool {
        matches!(
            result,
            Ok(())
                | Err(TaskRunError::Failed(
                    TaskError::ExceededTotalSize
                        | TaskError::StreamError { .. }
                        | TaskError::Timeout,
                ))
        )
    }

    pub(in crate::runner) async fn wait_data_drained(&mut self) -> DataDrainOutcome {
        let Some(data_tx) = self.data_tx.as_mut() else {
            return DataDrainOutcome::Drained;
        };

        if self.cancel_token.is_cancelled() {
            return DataDrainOutcome::Cancelled;
        }
        if data_tx.is_closed() {
            return DataDrainOutcome::Drained;
        }

        let timeout = async_io::Timer::after(self.data_drain_timeout);
        let drained = data_tx.wait_vacant(DATA_FRAME_CHANNEL_CAPACITY).fuse();
        let cancelled = self.cancel_token.cancelled().fuse();
        let timeout = futures::FutureExt::fuse(timeout);
        futures::pin_mut!(drained, cancelled, timeout);

        futures::select_biased! {
            _ = cancelled => DataDrainOutcome::Cancelled,
            _ = drained => DataDrainOutcome::Drained,
            _ = timeout => DataDrainOutcome::TimedOut,
        }
    }
}
