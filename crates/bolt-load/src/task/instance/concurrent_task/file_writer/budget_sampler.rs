//! Lock-Free Budget Sampler for disk write throttling
//!
//! This module implements a budget-based rate limiter that measures actual write
//! speed using EWMA and applies backpressure when disk writes are slower than
//! network downloads.

use std::{
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::Poll,
    time::{Duration, Instant},
};

use event_listener::{Event, EventListener};

/// Fixed-point shift for EWMA calculations (16 bits = 65536)
const FP_SHIFT: u32 = 16;
const FP_ONE: u64 = 1 << FP_SHIFT; // 65536

/// Configuration for the BudgetSampler
#[derive(Debug, Clone)]
pub struct BudgetConfig {
    /// Initial budget capacity (bytes)
    pub initial_capacity: u64,
    /// Minimum budget capacity (bytes), prevents complete starvation
    pub min_capacity: u64,
    /// Maximum budget capacity (bytes)
    pub max_capacity: u64,
    /// EWMA smoothing factor in fixed-point (alpha = ewma_alpha_fp / 65536)
    /// Default: 13107 (≈0.2)
    pub ewma_alpha_fp: u64,
    /// Sampling interval for EWMA updates
    pub sample_interval: Duration,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            initial_capacity: 64 * 1024 * 1024,  // 64MB
            min_capacity: 64 * 1024,              // 64KB
            max_capacity: 256 * 1024 * 1024,      // 256MB
            ewma_alpha_fp: 13107,                 // ≈0.2
            sample_interval: Duration::from_millis(500),
        }
    }
}

impl BudgetConfig {
    /// Create a new BudgetConfig with custom values
    pub fn new(
        initial_capacity: u64,
        min_capacity: u64,
        max_capacity: u64,
        ewma_alpha_fp: u64,
        sample_interval: Duration,
    ) -> Self {
        Self {
            initial_capacity,
            min_capacity,
            max_capacity,
            ewma_alpha_fp,
            sample_interval,
        }
    }
}

/// Lock-Free Budget Sampler for disk write throttling
///
/// Uses atomic operations and event-listener for FIFO fair waiting.
#[derive(Debug)]
pub struct BudgetSampler {
    /// Configuration
    config: BudgetConfig,
    /// Current available budget (bytes)
    budget: AtomicU64,
    /// EWMA speed (bytes/sec) in fixed-point format
    ewma_speed_fp: AtomicU64,
    /// Last sample timestamp (nanos since start)
    last_sample_nanos: AtomicU64,
    /// Bytes written since last sample
    bytes_written_since_sample: AtomicU64,
    /// Notifier for waiters (FIFO by event-listener)
    notifier: Event,
    /// Start time for relative timestamps
    start_at: Instant,
}

impl BudgetSampler {
    /// Create a new BudgetSampler with the given configuration
    pub fn new(config: BudgetConfig) -> Arc<Self> {
        let start_at = Instant::now();
        Arc::new(Self {
            budget: AtomicU64::new(config.initial_capacity),
            ewma_speed_fp: AtomicU64::new(0),
            last_sample_nanos: AtomicU64::new(0),
            bytes_written_since_sample: AtomicU64::new(0),
            notifier: Event::new(),
            start_at,
            config,
        })
    }

    /// Create a new BudgetSampler with default configuration
    pub fn with_defaults() -> Arc<Self> {
        Self::new(BudgetConfig::default())
    }

    /// Acquire budget for the given number of bytes
    ///
    /// Returns a future that resolves to a TokenGuard when budget is available.
    /// The TokenGuard will automatically release the budget and update EWMA on drop.
    pub fn acquire(self: &Arc<Self>, amount: u64) -> AcquireToken {
        AcquireToken {
            sampler: Arc::clone(self),
            amount,
            listener: None,
        }
    }

    /// Get current EWMA speed in bytes/sec
    pub fn ewma_speed(&self) -> f64 {
        let fp = self.ewma_speed_fp.load(Ordering::Relaxed);
        (fp as f64) / (FP_ONE as f64)
    }

    /// Get current available budget
    pub fn available_budget(&self) -> u64 {
        self.budget.load(Ordering::Relaxed)
    }

    /// Called when a write operation completes
    fn on_write_complete(&self, bytes: u64, _duration_nanos: u64) {
        // 1. Accumulate bytes for sampling
        self.bytes_written_since_sample
            .fetch_add(bytes, Ordering::Relaxed);

        // 2. Check if we need to update EWMA (based on sample interval)
        let now_nanos = self.now_nanos();
        let last = self.last_sample_nanos.load(Ordering::Relaxed);
        let elapsed_nanos = now_nanos.saturating_sub(last);
        let sample_interval_nanos = self.config.sample_interval.as_nanos() as u64;

        if elapsed_nanos >= sample_interval_nanos {
            // Try to claim the sample update (only one thread will succeed)
            if self
                .last_sample_nanos
                .compare_exchange(last, now_nanos, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                self.update_ewma(elapsed_nanos);
            }
        }

        // 3. Return budget (capped at max_capacity)
        let max = self.config.max_capacity;
        let _ = self.budget.fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
            Some(cur.saturating_add(bytes).min(max))
        });

        // 4. Wake one waiter (FIFO)
        self.notifier.notify_additional(1);
    }

    /// Update EWMA speed calculation
    fn update_ewma(&self, elapsed_nanos: u64) {
        // Get and reset bytes written since last sample
        let bytes = self
            .bytes_written_since_sample
            .swap(0, Ordering::AcqRel);

        // Calculate instantaneous speed (bytes/sec)
        let elapsed_secs = (elapsed_nanos as f64) / 1_000_000_000.0;
        let inst_speed = if elapsed_secs > 0.0 {
            (bytes as f64) / elapsed_secs
        } else {
            0.0
        };

        // Convert to fixed-point
        let inst_speed_fp = (inst_speed * (FP_ONE as f64)) as u64;

        // Update EWMA using fixed-point arithmetic
        let alpha = self.config.ewma_alpha_fp;
        let _ = self.ewma_speed_fp.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |cur_fp| {
                if cur_fp == 0 {
                    // Cold start: use instantaneous value
                    Some(inst_speed_fp)
                } else {
                    // EWMA: new = old + alpha * (sample - old)
                    // In fixed-point: new_fp = cur_fp + (alpha * (inst_fp - cur_fp)) >> FP_SHIFT
                    let diff = if inst_speed_fp >= cur_fp {
                        let d = inst_speed_fp - cur_fp;
                        let adj = ((d as u128) * (alpha as u128) >> FP_SHIFT) as u64;
                        cur_fp.saturating_add(adj)
                    } else {
                        let d = cur_fp - inst_speed_fp;
                        let adj = ((d as u128) * (alpha as u128) >> FP_SHIFT) as u64;
                        cur_fp.saturating_sub(adj)
                    };
                    Some(diff)
                }
            },
        );
    }

    /// Get current time in nanoseconds since start
    fn now_nanos(&self) -> u64 {
        let elapsed = Instant::now().duration_since(self.start_at);
        elapsed.as_nanos().min(u64::MAX as u128) as u64
    }

    /// Get current time in nanos for external tracking
    /// Use this with `release()` for cross-component acquire/release patterns.
    pub fn current_nanos(&self) -> u64 {
        self.now_nanos()
    }

    /// Acquire budget without RAII guard (for cross-component usage)
    /// Returns the start time in nanos. Call `release()` with this value when done.
    pub fn acquire_permit(self: &Arc<Self>, amount: u64) -> AcquirePermit {
        AcquirePermit {
            sampler: Arc::clone(self),
            amount,
            listener: None,
        }
    }

    /// Release budget and update EWMA (for cross-component usage)
    /// This is an alternative to TokenGuard when acquire and release happen
    /// in different components (e.g., Runner acquires, WriteWorker releases).
    pub fn release(&self, amount: u64, start_nanos: u64) {
        let duration_nanos = self.now_nanos().saturating_sub(start_nanos);
        self.on_write_complete(amount, duration_nanos);
    }
}

/// Try to atomically acquire budget using CAS
#[inline]
fn try_acquire(budget: &AtomicU64, amount: u64) -> bool {
    budget
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
            (cur >= amount).then(|| cur - amount)
        })
        .is_ok()
}

pin_project_lite::pin_project! {
    /// Future for acquiring budget tokens
    #[must_use = "futures do nothing unless you `.await` or poll them"]
    pub struct AcquireToken {
        sampler: Arc<BudgetSampler>,
        amount: u64,
        #[pin]
        listener: Option<EventListener>,
    }
}

impl Future for AcquireToken {
    type Output = TokenGuard;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        loop {
            // 1. Try to acquire directly
            if try_acquire(&this.sampler.budget, *this.amount) {
                this.listener.set(None);
                return Poll::Ready(TokenGuard::new(Arc::clone(this.sampler), *this.amount));
            }

            // 2. Register listener BEFORE sleeping (avoid missed wakeups)
            if this.listener.is_none() {
                this.listener.set(Some(this.sampler.notifier.listen()));
            }

            // 3. Re-check after registering (avoid race condition)
            if try_acquire(&this.sampler.budget, *this.amount) {
                this.listener.set(None);
                return Poll::Ready(TokenGuard::new(Arc::clone(this.sampler), *this.amount));
            }

            // 4. Wait for notification (FIFO by event-listener)
            match this.listener.as_mut().as_pin_mut() {
                Some(listener) => match listener.poll(cx) {
                    Poll::Ready(()) => {
                        // Woken up, clear listener and retry
                        this.listener.set(None);
                        continue;
                    }
                    Poll::Pending => return Poll::Pending,
                },
                None => unreachable!("listener should be set"),
            }
        }
    }
}

pin_project_lite::pin_project! {
    /// Future for acquiring budget permit (without RAII guard)
    /// Returns start_nanos when budget is acquired.
    #[must_use = "futures do nothing unless you `.await` or poll them"]
    pub struct AcquirePermit {
        sampler: Arc<BudgetSampler>,
        amount: u64,
        #[pin]
        listener: Option<EventListener>,
    }
}

impl Future for AcquirePermit {
    type Output = u64; // start_nanos

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        loop {
            // 1. Try to acquire directly
            if try_acquire(&this.sampler.budget, *this.amount) {
                this.listener.set(None);
                return Poll::Ready(this.sampler.now_nanos());
            }

            // 2. Register listener BEFORE sleeping (avoid missed wakeups)
            if this.listener.is_none() {
                this.listener.set(Some(this.sampler.notifier.listen()));
            }

            // 3. Re-check after registering (avoid race condition)
            if try_acquire(&this.sampler.budget, *this.amount) {
                this.listener.set(None);
                return Poll::Ready(this.sampler.now_nanos());
            }

            // 4. Wait for notification (FIFO by event-listener)
            match this.listener.as_mut().as_pin_mut() {
                Some(listener) => match listener.poll(cx) {
                    Poll::Ready(()) => {
                        // Woken up, clear listener and retry
                        this.listener.set(None);
                        continue;
                    }
                    Poll::Pending => return Poll::Pending,
                },
                None => unreachable!("listener should be set"),
            }
        }
    }
}

/// RAII guard that releases budget on drop
#[must_use = "if unused, the budget will be released immediately"]
pub struct TokenGuard {
    sampler: Arc<BudgetSampler>,
    amount: u64,
    start_nanos: u64,
}

impl TokenGuard {
    fn new(sampler: Arc<BudgetSampler>, amount: u64) -> Self {
        let start_nanos = sampler.now_nanos();
        Self {
            sampler,
            amount,
            start_nanos,
        }
    }

    /// Get the amount of bytes this guard represents
    pub fn amount(&self) -> u64 {
        self.amount
    }
}

impl Drop for TokenGuard {
    fn drop(&mut self) {
        let duration_nanos = self.sampler.now_nanos().saturating_sub(self.start_nanos);
        self.sampler.on_write_complete(self.amount, duration_nanos);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_budget_config_default() {
        let config = BudgetConfig::default();
        assert_eq!(config.initial_capacity, 64 * 1024 * 1024);
        assert_eq!(config.min_capacity, 64 * 1024);
        assert_eq!(config.max_capacity, 256 * 1024 * 1024);
        assert_eq!(config.ewma_alpha_fp, 13107);
        assert_eq!(config.sample_interval, Duration::from_millis(500));
    }

    #[test]
    fn test_budget_sampler_new() {
        let sampler = BudgetSampler::with_defaults();
        assert_eq!(sampler.available_budget(), 64 * 1024 * 1024);
        assert_eq!(sampler.ewma_speed(), 0.0);
    }

    #[tokio::test]
    async fn test_acquire_immediate() {
        let sampler = BudgetSampler::with_defaults();
        let guard = sampler.acquire(1024).await;
        assert_eq!(guard.amount(), 1024);
        assert_eq!(
            sampler.available_budget(),
            64 * 1024 * 1024 - 1024
        );
    }

    #[tokio::test]
    async fn test_acquire_release() {
        let config = BudgetConfig {
            initial_capacity: 1000,
            max_capacity: 1000,
            ..Default::default()
        };
        let sampler = BudgetSampler::new(config);

        // Acquire all budget
        let guard = sampler.acquire(1000).await;
        assert_eq!(sampler.available_budget(), 0);

        // Release
        drop(guard);
        assert_eq!(sampler.available_budget(), 1000);
    }

    #[tokio::test]
    async fn test_acquire_blocks_when_insufficient() {
        let config = BudgetConfig {
            initial_capacity: 100,
            max_capacity: 100,
            ..Default::default()
        };
        let sampler = BudgetSampler::new(config);

        // Acquire most of the budget
        let guard1 = sampler.acquire(80).await;
        assert_eq!(sampler.available_budget(), 20);

        // Try to acquire more than available - should block
        let sampler_clone = Arc::clone(&sampler);
        let handle = tokio::spawn(async move {
            sampler_clone.acquire(50).await
        });

        // Give some time for the task to start
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Budget should still be 20 (blocked task hasn't acquired yet)
        assert_eq!(sampler.available_budget(), 20);

        // Release first guard
        drop(guard1);

        // Now the blocked task should complete
        let guard2 = handle.await.unwrap();
        assert_eq!(guard2.amount(), 50);
    }

    #[tokio::test]
    async fn test_fifo_fairness() {
        let config = BudgetConfig {
            initial_capacity: 100,
            max_capacity: 200,
            ..Default::default()
        };
        let sampler = BudgetSampler::new(config);

        // Acquire all budget
        let guard = sampler.acquire(100).await;

        // Spawn multiple waiters in order
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut handles = Vec::new();

        for i in 0..3 {
            let sampler_clone = Arc::clone(&sampler);
            let order_clone = Arc::clone(&order);
            handles.push(tokio::spawn(async move {
                let _guard = sampler_clone.acquire(50).await;
                order_clone.lock().unwrap().push(i);
            }));
            // Small delay to ensure ordering
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Release budget multiple times to wake waiters
        drop(guard);

        // Wait for all to complete
        for handle in handles {
            handle.await.unwrap();
        }

        // Should complete in FIFO order (event-listener guarantees this)
        let completed_order = order.lock().unwrap().clone();
        assert_eq!(completed_order, vec![0, 1, 2]);
    }
}
