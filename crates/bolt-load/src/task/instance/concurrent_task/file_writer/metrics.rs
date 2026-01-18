//! File writer metrics for monitoring and backpressure control

use std::{
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

/// Metrics for monitoring file writer performance and queue depth
#[derive(Clone, Debug)]
pub struct FileWriterMetrics {
    /// Current depth of the write command queue
    queue_depth: Arc<AtomicUsize>,
    /// Total bytes written since creation
    bytes_written: Arc<AtomicU64>,
    /// Bytes written at last sample time
    last_sample_bytes: Arc<AtomicU64>,
    /// Last sample timestamp
    last_sample_time: Arc<Mutex<Instant>>,
}

impl FileWriterMetrics {
    /// Create a new metrics instance
    pub fn new() -> Self {
        Self {
            queue_depth: Arc::new(AtomicUsize::new(0)),
            bytes_written: Arc::new(AtomicU64::new(0)),
            last_sample_bytes: Arc::new(AtomicU64::new(0)),
            last_sample_time: Arc::new(Mutex::new(Instant::now())),
        }
    }

    /// Get the current queue depth
    #[inline]
    pub fn get_queue_depth(&self) -> usize {
        self.queue_depth.load(Ordering::Relaxed)
    }

    /// Set the current queue depth
    #[inline]
    pub fn set_queue_depth(&self, depth: usize) {
        self.queue_depth.store(depth, Ordering::Relaxed);
    }

    /// Increment bytes written counter
    #[inline]
    pub fn add_bytes_written(&self, bytes: u64) {
        self.bytes_written.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Get total bytes written
    #[inline]
    pub fn get_total_bytes_written(&self) -> u64 {
        self.bytes_written.load(Ordering::Relaxed)
    }

    /// Calculate current write speed (bytes/sec)
    ///
    /// Returns the write speed since last sample and updates the sample timestamp
    pub fn sample_write_speed(&self) -> f64 {
        let current_bytes = self.get_total_bytes_written();
        let mut last_time = self.last_sample_time.lock().unwrap();
        let now = Instant::now();
        let elapsed = now.duration_since(*last_time).as_secs_f64();

        // Avoid division by zero
        if elapsed < 0.001 {
            return 0.0;
        }

        let last_bytes = self.last_sample_bytes.load(Ordering::Relaxed);
        let bytes_delta = current_bytes.saturating_sub(last_bytes);
        let speed = bytes_delta as f64 / elapsed;

        // Update for next sample
        self.last_sample_bytes.store(current_bytes, Ordering::Relaxed);
        *last_time = now;

        speed
    }

    /// Get a snapshot of current metrics
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            queue_depth: self.get_queue_depth(),
            total_bytes_written: self.get_total_bytes_written(),
            write_speed: self.sample_write_speed(),
        }
    }
}

impl Default for FileWriterMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// A point-in-time snapshot of file writer metrics
#[derive(Debug, Clone, Copy)]
pub struct MetricsSnapshot {
    /// Current queue depth
    pub queue_depth: usize,
    /// Total bytes written
    pub total_bytes_written: u64,
    /// Current write speed in bytes/sec
    pub write_speed: f64,
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Duration};

    use super::*;

    #[test]
    fn test_metrics_new() {
        let metrics = FileWriterMetrics::new();
        assert_eq!(metrics.get_queue_depth(), 0);
        assert_eq!(metrics.get_total_bytes_written(), 0);
    }

    #[test]
    fn test_queue_depth() {
        let metrics = FileWriterMetrics::new();
        metrics.set_queue_depth(10);
        assert_eq!(metrics.get_queue_depth(), 10);

        metrics.set_queue_depth(42);
        assert_eq!(metrics.get_queue_depth(), 42);
    }

    #[test]
    fn test_bytes_written() {
        let metrics = FileWriterMetrics::new();
        metrics.add_bytes_written(100);
        assert_eq!(metrics.get_total_bytes_written(), 100);

        metrics.add_bytes_written(50);
        assert_eq!(metrics.get_total_bytes_written(), 150);
    }

    #[test]
    fn test_write_speed() {
        let metrics = FileWriterMetrics::new();

        // Initial speed should be 0
        let speed1 = metrics.sample_write_speed();
        assert_eq!(speed1, 0.0);

        // Write some bytes and wait a bit
        metrics.add_bytes_written(1000);
        thread::sleep(Duration::from_millis(100));

        let speed2 = metrics.sample_write_speed();
        // Speed should be approximately 10000 bytes/sec (1000 bytes / 0.1 sec)
        assert!(speed2 > 5000.0 && speed2 < 15000.0);
    }

    #[test]
    fn test_snapshot() {
        let metrics = FileWriterMetrics::new();
        metrics.set_queue_depth(5);
        metrics.add_bytes_written(200);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.queue_depth, 5);
        assert_eq!(snapshot.total_bytes_written, 200);
    }

    #[test]
    fn test_metrics_clone() {
        let metrics1 = FileWriterMetrics::new();
        metrics1.set_queue_depth(7);
        metrics1.add_bytes_written(300);

        let metrics2 = metrics1.clone();
        assert_eq!(metrics2.get_queue_depth(), 7);
        assert_eq!(metrics2.get_total_bytes_written(), 300);

        // Both should share the same underlying data
        metrics1.set_queue_depth(15);
        assert_eq!(metrics2.get_queue_depth(), 15);
    }
}
