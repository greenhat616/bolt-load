use std::cmp::Ordering;

use async_ringbuf::traits::*;
use bolt_load_utils::telemetry::*;
use bytes::{Bytes, BytesMut};
use futures::{FutureExt, StreamExt};

use self::drain::DataDrainOutcome;
use super::{BUFFER_SIZE, LifecycleEvent, TaskError, TaskRunError, TaskRunner};
use crate::{adapter::AdapterError, task::ControlEvent};

pub(super) mod drain;
mod emit;
mod stream;

/// The step of the event loop
enum EventLoopStep {
    /// The task is cancelled
    Cancelled,
    /// The stream yielded no non-empty data within the idle timeout
    IdleTimeout,
    /// The control signal is received (point-to-point, no need for RunnerId filtering)
    Control(Result<ControlEvent, async_channel::RecvError>),
    /// The download event is received
    Download(Option<Result<Bytes, AdapterError>>),
}

impl TaskRunner {
    /// run the task runner
    /// This function will block until the task is finished or cancelled
    /// It should be called in a new thread or async spawn context
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip(self), name = "TaskRunner::run", fields(runner_id = self.id))
    )]
    pub async fn run(&mut self) {
        let mut result = self.run_inner().await;
        if Self::should_wait_data_drain(&result) {
            match self.wait_data_drained().await {
                DataDrainOutcome::Drained => {}
                DataDrainOutcome::Cancelled => result = Err(TaskRunError::Cancelled),
                DataDrainOutcome::TimedOut => {
                    warn!(
                        "data drain timed out; reporting as timeout so undrained bytes are retried"
                    );
                    result = Err(TaskRunError::Failed(TaskError::Timeout));
                }
            }
        }

        match result {
            Ok(_) => {
                let _ = self.lifecycle_tx.push(LifecycleEvent::finished()).await;
            }
            Err(err) => match err {
                TaskRunError::Cancelled => {
                    let _ = self
                        .lifecycle_tx
                        .push(LifecycleEvent::failed(TaskError::Cancelled))
                        .await;
                }
                TaskRunError::Failed(task_err) => {
                    let _ = self
                        .lifecycle_tx
                        .push(LifecycleEvent::failed(task_err))
                        .await;
                }
            },
        }
        self.data_tx.take();
    }

    /// The inner logic of the task runner
    /// Just wrap a Result<(), TaskFailedKind> to return the error kind
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip(self), name = "TaskRunner::run_inner", fields(runner_id = self.id))
    )]
    pub(super) async fn run_inner(&mut self) -> Result<(), TaskRunError> {
        trace!("runner: started");
        self.lifecycle_tx
            .push(LifecycleEvent::Started)
            .await
            .map_err(|_| TaskRunError::Failed(TaskError::ChannelClosed))?;

        let mut buff = BytesMut::with_capacity(BUFFER_SIZE);
        let idle_timeout = self.idle_timeout;
        let mut idle_timer = async_io::Timer::after(idle_timeout);
        loop {
            let step = {
                let cancelled = self.cancel_token.cancelled().fuse();
                let control_signal = self.control_signal.recv().fuse();
                let download = self.stream.next().fuse();
                let idle = FutureExt::fuse(&mut idle_timer);
                futures::pin_mut!(cancelled, control_signal, download, idle);
                futures::select_biased! {
                    _ = cancelled => EventLoopStep::Cancelled,
                    signal = control_signal => EventLoopStep::Control(signal),
                    _ = idle => EventLoopStep::IdleTimeout,
                    result = download => EventLoopStep::Download(result),
                }
            };

            match step {
                EventLoopStep::Cancelled => {
                    trace!("runner: cancelled");
                    return Err(TaskRunError::Cancelled);
                }
                EventLoopStep::Control(Ok(signal)) => {
                    trace!("runner: control signal: {signal:?}");
                    Self::apply_control(&mut self.total, &signal)?;
                }
                EventLoopStep::Control(Err(_)) => {
                    // Control channel closed - manager has released this runner
                    trace!("runner: control signal channel closed");
                    return Err(TaskError::ChannelClosed.into());
                }
                EventLoopStep::IdleTimeout => {
                    warn!("runner: no data received in the last {idle_timeout:?}");
                    self.flush_checked(&mut buff).await?;
                    return Err(TaskError::Timeout.into());
                }
                EventLoopStep::Download(None) => {
                    self.flush_checked(&mut buff).await?;
                    break;
                }
                EventLoopStep::Download(Some(Err(source))) => {
                    // The upstream error wins over a control error deferred by the push.
                    let _ = self.flush(&mut buff).await?;
                    return Err(TaskError::StreamError { source }.into());
                }
                EventLoopStep::Download(Some(Ok(item))) => {
                    if !item.is_empty() {
                        idle_timer.set_after(idle_timeout);
                    }
                    if self
                        .buffer_item(item, &mut buff, &mut idle_timer)
                        .await?
                        .is_break()
                    {
                        break;
                    }
                }
            }
        }

        // TODO: handle the error
        match self.total {
            Some(total) => match total.cmp(&self.downloaded) {
                Ordering::Equal => {}
                Ordering::Greater => {
                    warn!(
                        "runner: downloaded content is smaller than the total size, total: {}, \
                         downloaded: {}",
                        total, self.downloaded
                    );
                    return Err(TaskError::SmallerThanTotalSize.into());
                }
                Ordering::Less => {
                    warn!(
                        "runner: downloaded content is larger than the total size, total: {}, \
                         downloaded: {}",
                        total, self.downloaded
                    );
                    return Err(TaskError::ExceededTotalSize.into());
                }
            },
            None if self.downloaded > 0 => {}
            None => {
                return Err(TaskError::Empty.into());
            }
        }
        Ok(())
    }
}
