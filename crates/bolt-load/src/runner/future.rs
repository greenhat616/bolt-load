use std::{
    pin::Pin,
    task::{Context, Poll},
};

use async_ringbuf::traits::{AsyncProducer, Producer};
use futures::future::FusedFuture;

use super::{DataFrame, DataFrameSender};

#[derive(Debug, snafu::Snafu)]
#[snafu(display("data channel is closed"))]
pub struct ChannelClosed;

pin_project_lite::pin_project! {
    #[project = FlushProj]
    #[project_replace = FlushProjReplace]
    pub enum FlushBuffFuture {
        Init {
            data_tx: DataFrameSender,
            frame: DataFrame,
        },
        Pushing {
            data_tx: DataFrameSender,
            frame: Option<DataFrame>,
        },
        Done,
    }
}

impl FlushBuffFuture {
    pub fn new(data_tx: DataFrameSender, frame: DataFrame) -> Self {
        Self::Init { data_tx, frame }
    }
}

impl Future for FlushBuffFuture {
    type Output = Result<DataFrameSender, ChannelClosed>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self;

        loop {
            match this.as_mut().project() {
                FlushProj::Init { .. } => {
                    let old = this.as_mut().project_replace(FlushBuffFuture::Done);
                    let (data_tx, frame) = match old {
                        FlushProjReplace::Init { data_tx, frame } => (data_tx, frame),
                        _ => unreachable!("expected Init"),
                    };

                    this.set(FlushBuffFuture::Pushing {
                        data_tx,
                        frame: Some(frame),
                    });

                    continue;
                }

                FlushProj::Pushing { data_tx, frame } => {
                    let Some(item) = frame.take() else {
                        let old = this.as_mut().project_replace(FlushBuffFuture::Done);
                        let data_tx = match old {
                            FlushProjReplace::Pushing { data_tx, .. } => data_tx,
                            _ => unreachable!("expected Pushing"),
                        };
                        return Poll::Ready(Ok(data_tx));
                    };

                    if data_tx.is_closed() {
                        this.as_mut().project_replace(FlushBuffFuture::Done);
                        return Poll::Ready(Err(ChannelClosed));
                    }

                    // Register waker BEFORE try_push to prevent lost wakes
                    data_tx.register_waker(cx.waker());

                    match data_tx.try_push(item) {
                        Ok(()) => {
                            *frame = None;
                            continue;
                        }
                        Err(item_back) => {
                            *frame = Some(item_back);
                            return Poll::Pending;
                        }
                    }
                }

                FlushProj::Done => {
                    panic!("FlushBuffFuture polled after completion");
                }
            }
        }
    }
}

impl FusedFuture for FlushBuffFuture {
    fn is_terminated(&self) -> bool {
        matches!(self, FlushBuffFuture::Done)
    }
}

const _: () = {
    struct AssertUnpin<T: Unpin> {
        _t: std::marker::PhantomData<T>,
    }
    impl<T: Unpin> AssertUnpin<T> {
        pub const fn new() -> Self {
            Self {
                _t: std::marker::PhantomData,
            }
        }
    }

    AssertUnpin::<FlushBuffFuture>::new();
};

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        panic::AssertUnwindSafe,
        pin::Pin,
        task::{Context, Poll},
    };

    use async_ringbuf::{
        AsyncHeapRb,
        traits::{Producer, Split},
    };
    use bytes::Bytes;
    use futures::{StreamExt, future::FusedFuture, task::noop_waker};

    use super::*;

    fn frame(data: &'static [u8]) -> DataFrame {
        DataFrame {
            data: Bytes::from_static(data),
        }
    }

    fn poll_once(
        future: Pin<&mut FlushBuffFuture>,
    ) -> Poll<Result<DataFrameSender, ChannelClosed>> {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        future.poll(&mut cx)
    }

    #[tokio::test]
    async fn waits_for_backpressure_when_data_channel_is_full() {
        let rb = AsyncHeapRb::<DataFrame>::new(1);
        let (mut data_tx, mut data_rx) = rb.split();
        data_tx.try_push(frame(b"occupied")).unwrap();

        let mut future = Box::pin(FlushBuffFuture::new(data_tx, frame(b"pending")));
        assert!(matches!(poll_once(future.as_mut()), Poll::Pending));
        assert!(!future.is_terminated());

        let occupied = data_rx.next().await.unwrap();
        assert_eq!(&occupied.data[..], b"occupied");

        let _data_tx = future
            .await
            .expect("flush should complete once channel capacity is available");
        let pending = data_rx.next().await.unwrap();
        assert_eq!(&pending.data[..], b"pending");
    }

    #[tokio::test]
    async fn returns_channel_closed_when_receiver_closes_during_push() {
        let rb = AsyncHeapRb::<DataFrame>::new(1);
        let (mut data_tx, data_rx) = rb.split();
        data_tx.try_push(frame(b"occupied")).unwrap();

        let mut future = Box::pin(FlushBuffFuture::new(data_tx, frame(b"pending")));
        assert!(matches!(poll_once(future.as_mut()), Poll::Pending));

        drop(data_rx);

        assert!(future.await.is_err());
    }

    #[test]
    fn panics_when_polled_after_completion() {
        let rb = AsyncHeapRb::<DataFrame>::new(1);
        let (data_tx, _data_rx) = rb.split();
        let mut future = Box::pin(FlushBuffFuture::new(data_tx, frame(b"done")));

        assert!(matches!(poll_once(future.as_mut()), Poll::Ready(Ok(_))));

        let panic = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = poll_once(future.as_mut());
        }));
        assert!(panic.is_err());
    }
}
