//! Fault injection primitives for test adapters.
//!
//! Built on `async-channel`, so they work under both Tokio and Smol.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use bolt_load_core::adapter::{
    AdapterError,
    error::{retryable::RetryableError, unretryable::InternalSnafu},
};

/// A one-shot barrier: callers of [`Gate::pass`] block until [`Gate::release`].
///
/// Every arrival is announced, so tests can wait for [`Gate::entered`]
/// instead of sleeping.
#[derive(Clone, Debug)]
pub struct Gate {
    entered_tx: async_channel::Sender<()>,
    entered_rx: async_channel::Receiver<()>,
    release_tx: async_channel::Sender<()>,
    release_rx: async_channel::Receiver<()>,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    pub fn new() -> Self {
        let (entered_tx, entered_rx) = async_channel::unbounded();
        let (release_tx, release_rx) = async_channel::bounded(1);
        Self {
            entered_tx,
            entered_rx,
            release_tx,
            release_rx,
        }
    }

    /// Announce the arrival, then wait until the gate is released.
    pub async fn pass(&self) {
        let _ = self.entered_tx.try_send(());
        // Nothing is ever sent; `recv` returns once the channel is closed.
        let _ = self.release_rx.recv().await;
    }

    /// Wait until one more caller has arrived at the gate.
    pub async fn entered(&self) {
        let _ = self.entered_rx.recv().await;
    }

    /// Open the gate for all current and future callers.
    pub fn release(&self) {
        self.release_tx.close();
    }
}

/// The kind of an injected failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultKind {
    Retryable,
    Unretryable,
}

impl FaultKind {
    pub fn error(self, context: &str) -> AdapterError {
        match self {
            FaultKind::Retryable => RetryableError::Io {
                source: Arc::new(std::io::Error::other(format!(
                    "injected retryable fault: {context}"
                ))),
            }
            .into(),
            FaultKind::Unretryable => InternalSnafu {
                message: format!("injected unretryable fault: {context}"),
            }
            .build()
            .into(),
        }
    }
}

/// How connection attempts behave.
#[derive(Clone, Debug, Default)]
pub enum FaultPlan {
    /// Connect normally.
    #[default]
    None,
    /// Fail the first `n` attempts, then connect normally.
    FailFirst(usize, FaultKind),
    /// Fail every attempt.
    AlwaysFail(FaultKind),
    /// Block every attempt at the gate until it is released.
    Block(Gate),
    /// Never complete the `n`-th (0-based) attempt; connect the others normally.
    HangAt(usize),
}

impl FaultPlan {
    /// Apply the plan to the `attempt`-th (0-based) connection attempt.
    pub async fn apply(&self, attempt: usize, context: &str) -> Result<(), AdapterError> {
        match self {
            FaultPlan::None => Ok(()),
            FaultPlan::FailFirst(n, kind) if attempt < *n => Err(kind.error(context)),
            FaultPlan::FailFirst(..) => Ok(()),
            FaultPlan::AlwaysFail(kind) => Err(kind.error(context)),
            FaultPlan::Block(gate) => {
                gate.pass().await;
                Ok(())
            }
            FaultPlan::HangAt(n) if attempt == *n => std::future::pending().await,
            FaultPlan::HangAt(_) => Ok(()),
        }
    }
}

/// Increments a counter for as long as it is alive.
#[derive(Debug)]
pub struct CounterGuard(Arc<AtomicUsize>);

impl CounterGuard {
    pub fn new(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter.clone())
    }
}

impl Drop for CounterGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn test_gate_blocks_until_released() {
        let gate = Gate::new();
        let waiter = tokio::spawn({
            let gate = gate.clone();
            async move { gate.pass().await }
        });
        gate.entered().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        gate.release();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        // Released gates let later callers through immediately.
        tokio::time::timeout(Duration::from_secs(1), gate.pass())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_fault_plan() {
        let plan = FaultPlan::FailFirst(2, FaultKind::Retryable);
        assert!(plan.apply(0, "t").await.is_err());
        assert!(plan.apply(1, "t").await.is_err());
        assert!(plan.apply(2, "t").await.is_ok());
        assert!(matches!(
            FaultPlan::AlwaysFail(FaultKind::Unretryable)
                .apply(5, "t")
                .await,
            Err(AdapterError::Unretryable { .. })
        ));
    }

    #[test]
    fn test_counter_guard() {
        let counter = Arc::new(AtomicUsize::new(0));
        let a = CounterGuard::new(&counter);
        let b = CounterGuard::new(&counter);
        assert_eq!(counter.load(Ordering::SeqCst), 2);
        drop(a);
        drop(b);
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }
}
