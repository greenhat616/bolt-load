use std::{
    pin::Pin,
    task::{Context, Poll},
};

use async_channel::Receiver;
use futures::{
    stream::{BoxStream, FusedStream, SelectAll},
    Stream, StreamExt,
};

use super::RunnerId;

pub struct RunnerNotification<'a, T>
where
    T: Send + 'a,
{
    receivers: Vec<RunnerId>,
    stream: SelectAll<BoxStream<'a, T>>,
}

impl<'a, T> Default for RunnerNotification<'a, T>
where
    T: Send + 'a,
{
    fn default() -> Self {
        Self {
            receivers: Vec::new(),
            stream: SelectAll::new(),
        }
    }
}

impl<'a, T> RunnerNotification<'a, T>
where
    T: Send + 'a,
{
    pub fn remove(&mut self, runner_id: RunnerId) {
        let index = self
            .receivers
            .iter()
            .position(|id| *id == runner_id)
            .unwrap();
        self.receivers.remove(index);
        let stream = std::mem::replace(&mut self.stream, SelectAll::new());
        for (i, stream) in stream.into_iter().enumerate() {
            if i == index {
                continue;
            }
            self.stream.push(stream);
        }
    }

    pub fn add(&mut self, runner_id: RunnerId, receiver: Receiver<T>) {
        self.receivers.push(runner_id);
        self.stream.push(Box::pin(receiver));
    }
}

impl<'a, T> Stream for RunnerNotification<'a, T>
where
    T: Send + 'a,
{
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        this.stream.poll_next_unpin(cx)
    }
}

impl<'a, T> FusedStream for RunnerNotification<'a, T>
where
    T: Send + 'a,
{
    fn is_terminated(&self) -> bool {
        self.stream.is_terminated()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_channel::unbounded;
    use test_log::test;

    #[test(tokio::test)]
    async fn test_runner_notifier() {
        let (tx1, rx1) = unbounded();
        let (tx2, rx2) = unbounded();
        let mut notifier = RunnerNotification::default();
        notifier.add(0, rx1);
        notifier.add(1, rx2);
        std::thread::spawn(move || {
            tx1.send_blocking(0).unwrap();
            tx2.send_blocking(1).unwrap();
            tx1.send_blocking(2).unwrap();
            tx2.send_blocking(3).unwrap();
        });
        let mut stream = notifier.boxed();
        assert_eq!(stream.next().await, Some(0));
        assert_eq!(stream.next().await, Some(1));
        assert_eq!(stream.next().await, Some(2));
        assert_eq!(stream.next().await, Some(3));
        assert_eq!(stream.next().await, None);
    }

    #[test(tokio::test)]
    async fn test_runner_notifier_remove() {
        let (tx, rx) = unbounded();
        let mut notifier = RunnerNotification::default();
        notifier.add(0, rx);
        let tx_clone = tx.clone();
        std::thread::spawn(move || {
            tx_clone.try_send(0).unwrap();
        })
        .join()
        .unwrap();
        assert_eq!(notifier.next().await, Some(0));
        notifier.remove(0);
        std::thread::spawn(move || {
            assert!(tx.try_send(1).is_err_and(|e| e.is_closed()));
        })
        .join()
        .unwrap();
        assert_eq!(notifier.next().await, None);
    }

    #[test(tokio::test)]
    async fn test_runner_notifier_dynamic_add() {
        let (tx, rx) = unbounded();
        let mut notifier = RunnerNotification::default();
        notifier.add(0, rx);
        let tx_clone = tx.clone();
        std::thread::spawn(move || {
            tx_clone.try_send(0).unwrap();
        })
        .join()
        .unwrap();
        assert_eq!(notifier.next().await, Some(0));
        let (tx2, rx2) = unbounded();
        notifier.add(1, rx2);
        let tx_clone = tx2.clone();
        std::thread::spawn(move || {
            tx_clone.try_send(1).unwrap();
        })
        .join()
        .unwrap();
        assert_eq!(notifier.next().await, Some(1));
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), notifier.next())
                .await
                .is_err()
        )
    }
}
