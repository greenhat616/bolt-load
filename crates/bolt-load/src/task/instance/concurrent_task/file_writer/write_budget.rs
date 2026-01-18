//! Write budget mechanism for backpressure control at the runner level
//!
//! Instead of controlling concurrency, this directly throttles data reading from
//! the stream by requiring budget tokens before consuming data. This prevents
//! excessive memory buildup when disk I/O is slower than network download.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_notify::Notify;
use bolt_load_utils::telemetry::*;

/// Default budget capacity: 32 MB
/// This allows ~16 concurrent 2MB chunks to be in-flight
const DEFAULT_BUDGET_CAPACITY: usize = 32 * 1024 * 1024;

/// High watermark threshold (75% of capacity)
/// When available budget crosses this threshold during refill, wake waiters
const DEFAULT_HIGH_WATERMARK_RATIO: f64 = 0.75;

/// Write budget for controlling data flow from runners to file writer
///
/// This mechanism ensures that runners don't download data faster than
/// the file writer can write it to disk, preventing memory exhaustion.
#[derive(Clone)]
pub struct WriteBudget {
    /// Current available budget (in bytes)
    available: Arc<AtomicUsize>,
    /// Notifier for waking waiting runners
    notifier: Arc<Notify>,
    /// Total budget capacity
    capacity: usize,
    /// High watermark for triggering notifications
    high_watermark: usize,
}

impl WriteBudget {
    /// Create a new write budget with default capacity (32 MB)
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_BUDGET_CAPACITY)
    }

    /// Create a new write budget with custom capacity
    pub fn with_capacity(capacity: usize) -> Self {
        let high_watermark = (capacity as f64 * DEFAULT_HIGH_WATERMARK_RATIO) as usize;
        Self {
            available: Arc::new(AtomicUsize::new(capacity)),
            notifier: Arc::new(Notify::new()),
            capacity,
            high_watermark,
        }
    }

    /// Get the current available budget
    pub fn available(&self) -> usize {
        self.available.load(Ordering::Acquire)
    }

    /// Get the total capacity
    pub fn capacity(&self) -> usize {
        self.capacity
    }


    /// Acquire budget, waiting if necessary
    ///
    /// This will block until sufficient budget is available and consume it.
    /// The budget should be refilled manually by calling `refill()` after the write completes.
    pub async fn acquire(&self, size: usize) {
        // Fast path: try immediate acquisition
        if self.try_consume(size) {
            return;
        }

        // Slow path: wait for budget to become available
        loop {
            // Wait for notification
            let notified = self.notifier.notified();

            // Double-check after creating the future but before awaiting
            if self.try_consume(size) {
                return;
            }

            // Wait for refill notification
            notified.await;

            // Try to acquire after being notified
            if self.try_consume(size) {
                return;
            }
        }
    }

    /// Try to consume budget without waiting
    ///
    /// Returns `true` if budget was consumed, `false` otherwise
    fn try_consume(&self, size: usize) -> bool {
        loop {
            let current = self.available.load(Ordering::Acquire);
            if current < size {
                return false;
            }

            // Try to consume budget using compare-exchange
            match self.available.compare_exchange_weak(
                current,
                current - size,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    trace!(
                        "[BUDGET] Consumed {} bytes, available: {} -> {}",
                        size,
                        current,
                        current - size
                    );
                    return true;
                }
                Err(_) => {
                    // CAS failed due to concurrent modification, retry
                    continue;
                }
            }
        }
    }

    /// Refill budget (called by file writer after successful write)
    ///
    /// This increases the available budget and notifies waiting runners
    /// if the budget crosses the high watermark threshold.
    pub fn refill(&self, amount: usize) {
        let prev = self.available.fetch_add(amount, Ordering::Release);
        let new = prev + amount;

        // Cap at capacity
        if new > self.capacity {
            let excess = new - self.capacity;
            self.available.fetch_sub(excess, Ordering::Release);
            trace!(
                "[BUDGET] Refilled {} bytes (capped excess: {}), available: {} -> {}",
                amount,
                excess,
                prev,
                self.capacity
            );
        } else {
            trace!(
                "[BUDGET] Refilled {} bytes, available: {} -> {}",
                amount,
                prev,
                new
            );
        }

        // Notify waiters if we crossed the high watermark
        // Note: notify() wakes up one waiter, and they will re-check budget
        // and potentially wake up others if budget is still sufficient
        if prev < self.high_watermark && new >= self.high_watermark {
            trace!("[BUDGET] Crossed high watermark, notifying waiters");
            self.notifier.notify();
        }
    }
}

impl Default for WriteBudget {
    fn default() -> Self {
        Self::new()
    }
}


impl std::fmt::Debug for WriteBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteBudget")
            .field("available", &self.available())
            .field("capacity", &self.capacity)
            .field("high_watermark", &self.high_watermark)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_budget_new() {
        let budget = WriteBudget::new();
        assert_eq!(budget.available(), DEFAULT_BUDGET_CAPACITY);
        assert_eq!(budget.capacity(), DEFAULT_BUDGET_CAPACITY);
    }

    #[test]
    fn test_budget_with_capacity() {
        let budget = WriteBudget::with_capacity(1024);
        assert_eq!(budget.available(), 1024);
        assert_eq!(budget.capacity(), 1024);
    }

    #[test]
    fn test_try_consume_success() {
        let budget = WriteBudget::with_capacity(1000);
        assert!(budget.try_consume(100));
        assert_eq!(budget.available(), 900);
    }

    #[test]
    fn test_try_consume_failure() {
        let budget = WriteBudget::with_capacity(100);
        assert!(!budget.try_consume(200));
        assert_eq!(budget.available(), 100);
    }

    #[test]
    fn test_refill() {
        let budget = WriteBudget::with_capacity(1000);
        budget.try_consume(300);
        assert_eq!(budget.available(), 700);

        budget.refill(300);
        assert_eq!(budget.available(), 1000);
    }

    #[test]
    fn test_refill_caps_at_capacity() {
        let budget = WriteBudget::with_capacity(1000);
        budget.refill(500); // Already at 1000, so this should be capped
        assert_eq!(budget.available(), 1000);
    }

    #[tokio::test]
    async fn test_acquire_immediate() {
        let budget = WriteBudget::with_capacity(1000);
        budget.acquire(100).await;
        assert_eq!(budget.available(), 900);

        // Manually refill
        budget.refill(100);
        assert_eq!(budget.available(), 1000);
    }

    #[tokio::test]
    async fn test_acquire_waits_for_budget() {
        let budget = WriteBudget::with_capacity(100);

        // Consume all budget
        budget.acquire(100).await;
        assert_eq!(budget.available(), 0);

        // Spawn a task that will wait for budget
        let budget_clone = budget.clone();
        let task = tokio::spawn(async move {
            budget_clone.acquire(50).await;
        });

        // Give the task time to start waiting
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Refill budget
        budget.refill(100);

        // The waiting task should now complete
        task.await.unwrap();
    }

    #[tokio::test]
    async fn test_concurrent_acquisitions() {
        let budget = WriteBudget::with_capacity(1000);

        let mut tasks = vec![];
        for _ in 0..10 {
            let budget_clone = budget.clone();
            tasks.push(tokio::spawn(async move {
                budget_clone.acquire(100).await;
                // Simulate some work
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                // Manually refill after "work" completes
                budget_clone.refill(100);
            }));
        }

        // All tasks should complete successfully
        for task in tasks {
            task.await.unwrap();
        }

        // Budget should be fully refilled after all tasks complete
        assert_eq!(budget.available(), 1000);
    }

    #[test]
    fn test_debug_format() {
        let budget = WriteBudget::with_capacity(2048);
        let debug_str = format!("{:?}", budget);
        assert!(debug_str.contains("WriteBudget"));
        assert!(debug_str.contains("available"));
        assert!(debug_str.contains("capacity"));
    }
}
