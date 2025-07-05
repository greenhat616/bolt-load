use std::time::Instant;

pub const DEFAULT_SAMPLE_INTERVAL: u64 = 250; // 250 milliseconds
const DEFAULT_EMA_ALPHA: f64 = 0.33;

/// Speed sampler
///
/// Unit: B/s
pub struct SpeedSampler {
    /// Last sample time
    last_ts: Instant,
    /// EMA speed
    ema_speed: f64,
    /// EMA coefficient
    alpha: f64,
}

impl SpeedSampler {
    /// Default α = 0.33, approximately 1 s smoothing window
    pub fn new() -> Self {
        Self::with_alpha(0.33)
    }

    /// Custom α (0.0 < α ≤ 1.0)
    pub fn with_alpha(alpha: f64) -> Self {
        assert!((0.0..=1.0).contains(&alpha), "alpha must be within 0..=1");
        Self {
            last_ts: Instant::now(),
            ema_speed: 0.0,
            alpha,
        }
    }

    /// Sample: `bytes` is the number of bytes written since the last sample.
    /// After calling, `*bytes` will be set to 0, making it easier to accumulate in the next round.
    #[inline]
    pub fn sample(&mut self, bytes: &mut usize) -> (f64, f64) {
        let delta_bytes = std::mem::take(bytes) as f64;
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_ts).as_secs_f64();
        self.last_ts = now;

        // Avoid division by zero when elapsed is 0
        let inst_speed = if elapsed > 0.0 {
            delta_bytes / elapsed
        } else {
            0.0
        };

        // EMA: first frame is directly assigned, then gradually
        self.ema_speed = if self.ema_speed == 0.0 {
            inst_speed
        } else {
            self.alpha * inst_speed + (1.0 - self.alpha) * self.ema_speed
        };

        (inst_speed, self.ema_speed)
    }

    /// Growth rate; return `None` if prev is 0
    #[inline]
    pub fn growth_rate(prev: f64, next: f64) -> Option<f64> {
        if prev == 0.0 {
            None
        } else {
            Some((next - prev) / prev)
        }
    }
}
