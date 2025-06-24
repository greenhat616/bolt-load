pub const SAMPLE_INTERVAL: u64 = 2; // 2 seconds

pub struct Sampler {
    sampler_interval: u64,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new(SAMPLE_INTERVAL)
    }
}

impl Sampler {
    pub fn new(sampler_interval: u64) -> Self {
        Self { sampler_interval }
    }

    /// Calculate the speed
    #[inline]
    pub fn sample(&self, meters: &mut usize) -> f64 {
        let total_meters = std::mem::take(meters) as f64;
        
        total_meters / self.sampler_interval as f64
    }

    #[inline]
    pub fn growth_rate(speed_a: f64, speed_b: f64) -> f64 {
        
        (speed_b - speed_a) / speed_a
    }
}
