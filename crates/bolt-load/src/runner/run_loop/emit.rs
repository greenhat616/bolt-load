use async_ringbuf::traits::*;
use bolt_load_utils::telemetry::*;
use bytes::{Bytes, BytesMut};
use futures::{FutureExt, future::Either};

use super::super::{DataFrame, TaskError, TaskRunError, TaskRunner};

impl TaskRunner {
    /// Push `data` downstream, applying the control signals that arrive meanwhile.
    ///
    /// Returns `Err` when the push fails or the runner is cancelled, and
    /// `Ok(Some(_))` when the push succeeded but a control signal failed
    /// meanwhile; only the first such error is kept. The stream is not polled
    /// while pushing, so waiting for the consumer does not count as idle.
    pub(in crate::runner) async fn emit(
        &mut self,
        data: Bytes,
    ) -> Result<Option<TaskRunError>, TaskRunError> {
        let len = data.len() as u64;
        let Self {
            total,
            downloaded,
            control_signal,
            data_tx,
            cancel_token,
            ..
        } = self;
        let data_tx = data_tx.as_mut().ok_or(TaskError::ChannelClosed)?;
        let push = data_tx.push(DataFrame { data });
        let cancelled = cancel_token.cancelled().fuse();
        futures::pin_mut!(push, cancelled);
        let mut deferred = None;
        let mut control_open = true;
        loop {
            let control = if control_open {
                Either::Left(control_signal.recv())
            } else {
                Either::Right(futures::future::pending())
            }
            .fuse();
            futures::pin_mut!(control);
            futures::select_biased! {
                _ = cancelled => return Err(TaskRunError::Cancelled),
                signal = control => {
                    let err = match signal {
                        Ok(signal) => Self::apply_control(total, &signal).err().map(Into::into),
                        Err(_) => {
                            // The manager has released this runner
                            control_open = false;
                            Some(TaskError::ChannelClosed.into())
                        }
                    };
                    match err {
                        Some(err) if deferred.is_some() => {
                            warn!("discarding control error during push: {err:?}");
                        }
                        err => deferred = deferred.or(err),
                    }
                }
                res = push => {
                    res.map_err(|_| TaskError::ChannelClosed)?;
                    *downloaded += len;
                    return Ok(deferred);
                }
            }
        }
    }

    /// Push the buffered bytes, if any. See [`Self::emit`] for the result.
    pub(super) async fn flush(
        &mut self,
        buff: &mut BytesMut,
    ) -> Result<Option<TaskRunError>, TaskRunError> {
        if buff.is_empty() {
            return Ok(None);
        }
        self.emit(buff.split().freeze()).await
    }

    /// Push the buffered bytes, failing with a control error deferred by the push.
    pub(super) async fn flush_checked(&mut self, buff: &mut BytesMut) -> Result<(), TaskRunError> {
        match self.flush(buff).await? {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}
