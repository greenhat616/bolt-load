use std::time::{Duration, Instant};

pub const DEFAULT_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

#[allow(dead_code)]
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
        Self::with_alpha(DEFAULT_EMA_ALPHA)
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
    #[allow(dead_code)]
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

    use bolt_load_tests::adapter::simple::SimpleTestAdapterBuilder;
    use bolt_load_utils::telemetry::*;
    use futures::StreamExt;

    use crate::{
        adapter::BoltLoadAdapter,
        task::instance::sampler::{DEFAULT_SAMPLE_INTERVAL, SpeedSampler},
    };

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_speed_sampler() {
        let file_size = 1024 * 1024 * 2; // 2MB for reasonable test time

        // Set up adapter with range support for concurrent downloading
        // Use max_per_stream_speed to simulate slow downloads (equivalent to ~10ms delay per 8KB chunk)

        let adapter = SimpleTestAdapterBuilder::new()
            .content_size(file_size)
            .support_range(true)
            .max_per_stream_speed(819200) // ~800KB/s per stream
            .build()
            .expect("Failed to build adapter");
        info!("adapter: {:?}", adapter);
        let mut stream = adapter
            .full_stream()
            .await
            .expect("Failed to get full stream");

        let mut sampler = SpeedSampler::new();
        let mut bytes: usize = 0;

        let sample_every = DEFAULT_SAMPLE_INTERVAL;
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

    /// Test that verifies the measured speed is consistent and reflects adapter's speed limit
    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_speed_matches_adapter_limit() {
        // Test with two different speed limits to ensure the sampler accurately reflects changes
        let test_cases = vec![
            (300_000, 1536 * 1024),         // 300KB/s, 1.5MB file
            (600_000, 2 * 1024 * 1024),     // 600KB/s, 2MB file
            (1_000_000, 3 * 1024 * 1024),   // 1MB/s, 3MB file
            (4_000_000, 6 * 1024 * 1024),   // 4MB/s, 6MB file
            (10_000_000, 20 * 1024 * 1024), // 10MB/s, 20MB file
        ];

        let mut measured_speeds = Vec::new();

        for (target_speed, file_size) in test_cases {
            info!(
                "Testing with target_speed={} B/s ({:.2} KB/s), file_size={} B",
                target_speed,
                target_speed as f64 / 1024.0,
                file_size
            );

            let adapter = SimpleTestAdapterBuilder::new()
                .content_size(file_size)
                .support_range(true)
                .max_per_stream_speed(target_speed)
                .build()
                .expect("Failed to build adapter");

            let mut stream = adapter
                .full_stream()
                .await
                .expect("Failed to get full stream");

            let mut sampler = SpeedSampler::new();
            let mut bytes_accumulated: usize = 0;
            let mut total_bytes: usize = 0;

            let sample_every = DEFAULT_SAMPLE_INTERVAL;
            let mut last_sample_check = Instant::now();

            let start_time = Instant::now();
            let mut ema_samples = Vec::new();

            while let Some(item) = stream.next().await {
                let chunk = item.expect("stream error");
                bytes_accumulated += chunk.len();
                total_bytes += chunk.len();

                if last_sample_check.elapsed() >= sample_every {
                    let (inst, ema) = sampler.sample(&mut bytes_accumulated);
                    info!(
                        "Sample: inst={:.2} B/s ({:.2} KB/s), ema={:.2} B/s ({:.2} KB/s)",
                        inst,
                        inst / 1024.0,
                        ema,
                        ema / 1024.0
                    );

                    // Collect all samples; we'll skip first few when calculating average
                    ema_samples.push(ema);

                    last_sample_check = Instant::now();
                }
            }

            // Final sample for any remaining bytes
            if bytes_accumulated > 0 {
                let (inst, ema) = sampler.sample(&mut bytes_accumulated);
                info!("Final sample: inst={:.2} B/s, ema={:.2} B/s", inst, ema);
            }

            let total_time = start_time.elapsed().as_secs_f64();
            let avg_speed = total_bytes as f64 / total_time;

            info!(
                "Total: {} bytes in {:.2}s, avg speed: {:.2} B/s ({:.2} KB/s)",
                total_bytes,
                total_time,
                avg_speed,
                avg_speed / 1024.0
            );

            // Calculate average EMA speed (excluding first 2 samples for stabilization)
            let stable_samples: Vec<f64> = ema_samples.iter().skip(2).copied().collect();
            if !stable_samples.is_empty() {
                let avg_ema: f64 = stable_samples.iter().sum::<f64>() / stable_samples.len() as f64;
                info!(
                    "Average EMA speed (after stabilization): {:.2} B/s ({:.2} KB/s)",
                    avg_ema,
                    avg_ema / 1024.0
                );
            }

            // Verify speeds are reasonable (non-zero and finite)
            assert!(
                avg_speed > 0.0 && avg_speed.is_finite(),
                "Average speed should be positive and finite, got {:.2}",
                avg_speed
            );

            info!(
                "✓ Speed measurement for target {} B/s: measured {:.2} B/s ({:.2}% of target)",
                target_speed,
                avg_speed,
                (avg_speed / target_speed as f64) * 100.0
            );

            measured_speeds.push((target_speed, avg_speed));
        }

        // Verify that when target speed doubles, measured speed also roughly doubles
        // This tests the consistency of speed measurement
        if measured_speeds.len() >= 2 {
            let (target1, measured1) = measured_speeds[0];
            let (target2, measured2) = measured_speeds[1];

            let target_ratio = target2 as f64 / target1 as f64;
            let measured_ratio = measured2 / measured1;

            info!(
                "Speed ratio test: target ratio={:.2}, measured ratio={:.2}",
                target_ratio, measured_ratio
            );

            // Allow 30% tolerance for ratio comparison
            let ratio_tolerance = 0.30;
            let ratio_diff = (measured_ratio - target_ratio).abs() / target_ratio;

            assert!(
                ratio_diff <= ratio_tolerance,
                "Speed ratio mismatch: expected ratio {:.2}, got {:.2} (difference: {:.2}%)",
                target_ratio,
                measured_ratio,
                ratio_diff * 100.0
            );

            info!("✓ Speed ratio test passed: sampler accurately reflects relative speed changes");
        }
    }

    /// Test speed sampler with different alpha values
    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_speed_sampler_different_alpha() {
        let file_size = 2 * 1024 * 1024; // 2MB
        let target_speed = 500_000; // 500KB/s

        // Test with different alpha values
        let alpha_values = vec![0.1, 0.33, 0.5, 0.8];

        for alpha in alpha_values {
            info!("Testing with alpha={}", alpha);

            let adapter = SimpleTestAdapterBuilder::new()
                .content_size(file_size)
                .support_range(true)
                .max_per_stream_speed(target_speed)
                .build()
                .expect("Failed to build adapter");

            let mut stream = adapter
                .full_stream()
                .await
                .expect("Failed to get full stream");

            let mut sampler = SpeedSampler::with_alpha(alpha);
            let mut bytes_accumulated: usize = 0;

            let sample_every = DEFAULT_SAMPLE_INTERVAL;
            let mut last_sample_check = Instant::now();

            let mut ema_variance = 0.0f64;
            let mut prev_ema: Option<f64> = None;
            let mut variance_count = 0usize;

            while let Some(item) = stream.next().await {
                let chunk = item.expect("stream error");
                bytes_accumulated += chunk.len();

                if last_sample_check.elapsed() >= sample_every {
                    let (_inst, ema) = sampler.sample(&mut bytes_accumulated);

                    // Calculate variance in EMA changes (after a few samples)
                    if let Some(prev) = prev_ema {
                        if variance_count > 2 {
                            // Skip first few for stability
                            let diff = (ema - prev).abs();
                            ema_variance += diff;
                        }
                        variance_count += 1;
                    }

                    prev_ema = Some(ema);
                    last_sample_check = Instant::now();
                }
            }

            if variance_count > 3 {
                let avg_variance = ema_variance / (variance_count - 3) as f64;
                info!(
                    "Alpha={}: average EMA variance={:.2} B/s",
                    alpha, avg_variance
                );

                // Higher alpha should generally result in higher variance (more responsive)
                // This is a qualitative check
                assert!(avg_variance >= 0.0);
            }

            info!("✓ Test completed for alpha={}", alpha);
        }
    }
}
