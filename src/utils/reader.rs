use blocking::Unblock;
use bytes::Bytes;
use futures::{AsyncRead, Stream};
use std::io::Read;
use std::pin::Pin;
use std::task::{Context, Poll};

pub struct CrossRuntimeStream {
    pub(crate) reader: Unblock<Box<dyn Read + Send>>,
    pub(crate) buffer: Vec<u8>,
}

impl CrossRuntimeStream {
    pub fn new(reader: impl Read + Send + 'static, buffer_size: usize) -> Self {
        Self {
            reader: Unblock::new(Box::new(reader)),
            buffer: vec![0; buffer_size],
        }
    }
}

impl Stream for CrossRuntimeStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        match Pin::new(&mut this.reader).poll_read(cx, &mut this.buffer) {
            Poll::Ready(Ok(0)) => Poll::Ready(None),
            Poll::Ready(Ok(n)) => {
                let chunk = Bytes::copy_from_slice(&this.buffer[..n]);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{StreamExt, TryStreamExt};
    use std::io::Cursor;
    use test_log::test;

    #[test(tokio::test)]
    async fn test_cross_runtime_stream() {
        let data = vec![1, 2, 3, 4, 5];
        let reader = Box::new(Cursor::new(data.clone()));

        let stream = CrossRuntimeStream::new(reader, 2);

        let result: Result<Vec<u8>, _> = stream
            .map(|result| result.map(|bytes| bytes.to_vec()))
            .try_concat()
            .await;

        assert_eq!(result.unwrap(), data);
    }

    #[test(tokio::test)]
    async fn test_empty_reader() {
        let reader = Box::new(Cursor::new(vec![]));
        let mut stream = CrossRuntimeStream::new(reader, 1024);

        assert!(stream.next().await.is_none());
    }

    #[test(tokio::test)]
    async fn test_large_data() {
        let data = vec![42; 100_000];
        let reader = Box::new(Cursor::new(data.clone()));

        let stream = CrossRuntimeStream::new(reader, 1024);

        let result: Result<Vec<u8>, _> = stream
            .map(|result| result.map(|bytes| bytes.to_vec()))
            .try_concat()
            .await;

        assert_eq!(result.unwrap(), data);
    }
}
