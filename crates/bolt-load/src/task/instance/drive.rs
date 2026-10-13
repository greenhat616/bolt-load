//! The lifecycle shared by both task modes: initialize, download, report.

use futures::FutureExt;
use smol_cancellation_token::CancellationToken;

use super::{ProgressWithSpeed, Result, TaskControl, TaskEvent, TaskInstanceError};
use crate::{
    runner::TaskError,
    runtime::{LocalRuntimeBuilderImpl, ThreadedRuntimeExt, ThreadedRuntimeImpl},
    task::Progress,
};

/// One download attempt of a task mode.
pub(super) trait Transfer {
    /// Retrieve the metadata the download depends on.
    async fn prepare(&mut self) -> Result<()>;
    /// The progress published when the download starts.
    fn progress(&self) -> Progress;
    /// The progress published with [`TaskEvent::Finished`].
    fn final_progress(&self) -> Progress;
    /// Download and publish or discard the output file.
    ///
    /// Cancellation is handled inside, so when this returns every spawned
    /// worker has been joined and the output file has been committed.
    async fn download(&mut self, cancel_token: &CancellationToken) -> Result<()>;
}

/// Drive `transfer` to completion, publishing its lifecycle on `event_tx`.
///
/// The terminal event is sent last, once the transfer has been torn down.
pub(super) async fn drive<T: Transfer>(
    mut transfer: T,
    event_tx: async_channel::Sender<TaskEvent>,
    cancel_token: CancellationToken,
) {
    let _ = event_tx.send(TaskEvent::Initializing).await;
    let result = async {
        {
            let prepare = transfer.prepare().fuse();
            let cancelled = cancel_token.cancelled().fuse();
            futures::pin_mut!(prepare, cancelled);
            futures::select_biased! {
                _ = cancelled => Err(TaskInstanceError::new_failed(TaskError::Cancelled)),
                res = prepare => res,
            }?;
        }
        let _ = event_tx
            .send(TaskEvent::Downloading(ProgressWithSpeed::new(
                transfer.progress(),
                0.0,
                0.0,
            )))
            .await;
        // Never raced against cancellation, so its teardown always completes.
        transfer.download(&cancel_token).await
    }
    .await;
    let terminal = match result {
        Ok(()) => TaskEvent::Finished(transfer.final_progress()),
        Err(e) => TaskEvent::Failed(e),
    };
    let _ = event_tx.send(terminal).await;
}

/// Run the future built by `make` on a local runtime in a blocking thread.
///
/// The future is built on that thread, so it does not need to be `Send`.
pub(super) fn spawn_on_local<Make, Fut>(
    rt: ThreadedRuntimeImpl,
    local_runtime_builder: Option<LocalRuntimeBuilderImpl>,
    cancel_token: CancellationToken,
    make: Make,
) -> TaskControl
where
    Make: FnOnce(ThreadedRuntimeImpl) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    // TODO: replace the local task executor with global runtime executor while `-Zhigher-ranked-assumptions` is stable
    // Ref:  https://github.com/rust-lang/rust/issues/100013.
    let task = ::blocking::unblock(move || {
        let local_rt = rt
            .downcast_local(local_runtime_builder)
            .expect("failed to downcast the runtime");
        local_rt.block_on(make(rt));
    });
    TaskControl::new(cancel_token, task)
}
