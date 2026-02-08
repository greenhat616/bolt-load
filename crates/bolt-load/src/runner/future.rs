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
            waker_registered: bool,
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
                        waker_registered: false,
                    });

                    continue;
                }

                FlushProj::Pushing {
                    data_tx,
                    frame,
                    waker_registered,
                } => {
                    let Some(item) = frame.take() else {
                        let old = this.as_mut().project_replace(FlushBuffFuture::Done);
                        let data_tx = match old {
                            FlushProjReplace::Pushing { data_tx, .. } => data_tx,
                            _ => unreachable!("expected Pushing"),
                        };
                        return Poll::Ready(Ok(data_tx));
                    };

                    if data_tx.is_closed() {
                        return Poll::Ready(Err(ChannelClosed));
                    }

                    match data_tx.try_push(item) {
                        Ok(()) => {
                            *frame = None;
                            continue;
                        }
                        Err(item_back) => {
                            *frame = Some(item_back);
                            if *waker_registered {
                                return Poll::Pending;
                            }
                            data_tx.register_waker(cx.waker());
                            *waker_registered = true;
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
