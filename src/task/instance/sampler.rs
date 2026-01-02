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

#[cfg(test)]
mod test {
    use std::time::{Duration, Instant};

    use futures::StreamExt;

    use crate::{
        adapter::{BoltLoadAdapter, tests::SimpleTestAdapter},
        task::instance::sampler::{DEFAULT_SAMPLE_INTERVAL, SpeedSampler},
        utils::logging::*,
    };
    #[tokio::test(flavor = "multi_thread")]
    async fn test_speed_sampler() {
        crate::utils::test::init_tracing().await;

        let file_size = 1024 * 1024; // 1MB for reasonable test time

        // Set up adapter with range support for concurrent downloading
        let adapter = SimpleTestAdapter::new(file_size)
            .with_range_support(true)
            .with_delay_per_chunk(std::time::Duration::from_millis(10));
        info!("adapter: {:?}", adapter);
        let mut stream = adapter.full_stream().await.expect("Failed to get full stream");

        let mut sampler = SpeedSampler::new();
        let mut bytes: usize = 0;

        let sample_every = Duration::from_millis(DEFAULT_SAMPLE_INTERVAL);
        let mut last_sample_check = Instant::now();

        let mut prev_ema: Option<f64> = None;
        let mut sample_count = 0usize;

        while let Some(item) = stream.next().await {
            // `full_stream()` is typically `Stream<Item = Result<Bytes, _>>`
            let chunk = item.expect("stream error");
            bytes += chunk.len();

            if last_sample_check.elapsed() >= sample_every {
                let (inst, ema) = sampler.sample(&mut bytes);

                info!("speed: inst={:.2} B/s, ema={:.2} B/s", inst, ema);

                // Basic sanity: speeds should be non-negative and EMA should be non-negative.
                assert!(inst >= 0.0);
                assert!(ema >= 0.0);

                // EMA should lie between previous EMA and current instantaneous speed (convex combination),
                // except for the first sample where ema == inst.
                if let Some(prev) = prev_ema {
                    let lo = prev.min(inst) - 1e-9;
                    let hi = prev.max(inst) + 1e-9;
                    assert!(
                        (lo..=hi).contains(&ema),
                        "ema={} not within [{}, {}] (prev_ema={}, inst={})",
                        ema,
                        lo,
                        hi,
                        prev,
                        inst
                    );

                    // growth_rate should be defined when prev != 0
                    let gr =
                        SpeedSampler::growth_rate(prev, ema).expect("growth_rate should be Some");
                    assert!(gr.is_finite());
                } else {
                    // growth_rate(None) case: prev == 0
                    assert!(SpeedSampler::growth_rate(0.0, ema).is_none());
                }

                prev_ema = Some(ema);
                sample_count += 1;
                last_sample_check = Instant::now();
            }
        }

        // Flush any remaining accumulated bytes in a final sample.
        let (inst, ema) = sampler.sample(&mut bytes);
        info!("final speed: inst={:.2} B/s, ema={:.2} B/s", inst, ema);
        assert!(inst >= 0.0);
        assert!(ema >= 0.0);

        // We expect to have sampled multiple times given the per-chunk delay.
        assert!(
            sample_count >= 2,
            "expected at least 2 samples, got {}",
            sample_count
        );
    }
}
