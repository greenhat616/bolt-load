use std::ops::ControlFlow;

use bytes::{Bytes, BytesMut};

use super::super::{BUFFER_SIZE, TaskRunError, TaskRunner};

impl TaskRunner {
    /// Bytes that may still be buffered before reaching the size limit.
    fn allowance(&self, buff: &BytesMut) -> usize {
        self.total.map_or(usize::MAX, |total| {
            total.saturating_sub(self.downloaded + buff.len() as u64) as usize
        })
    }

    /// Buffer a stream item, pushing every full buffer downstream.
    ///
    /// The size limit is re-read after each push, since the manager may lower
    /// it meanwhile; bytes already buffered are kept.
    pub(super) async fn buffer_item(
        &mut self,
        item: Bytes,
        buff: &mut BytesMut,
        idle_timer: &mut async_io::Timer,
    ) -> Result<ControlFlow<()>, TaskRunError> {
        let mut offset = 0;
        loop {
            let allowed = self.allowance(buff);
            if allowed == 0 {
                self.flush_checked(buff).await?;
                return Ok(ControlFlow::Break(()));
            }
            if offset == item.len() {
                return Ok(ControlFlow::Continue(()));
            }
            let writable = (item.len() - offset).min(allowed);
            if buff.len() + writable <= BUFFER_SIZE {
                buff.extend_from_slice(&item[offset..offset + writable]);
                if writable == allowed {
                    self.flush_checked(buff).await?;
                    return Ok(ControlFlow::Break(()));
                }
                return Ok(ControlFlow::Continue(()));
            }
            let take = BUFFER_SIZE - buff.len();
            buff.extend_from_slice(&item[offset..offset + take]);
            offset += take;
            self.flush_checked(buff).await?;
            // Backpressure is not idleness.
            idle_timer.set_after(self.idle_timeout);
        }
    }
}
