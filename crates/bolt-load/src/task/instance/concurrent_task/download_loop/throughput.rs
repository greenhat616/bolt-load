use super::{super::runner_manager::MeterSnapshot, DownloadLoop};
use crate::task::{
    Progress,
    instance::{ProgressWithSpeed, TaskEvent, sampler::SpeedSampler},
};

/// Download and write throughput, sampled on the throughput timer.
pub(super) struct Throughput {
    sampler: SpeedSampler,
    write_sampler: SpeedSampler,
    /// Bytes written since the last sample.
    pub(super) written_bytes: usize,
    pub(super) speed: f64,
    pub(super) per_runner_speed: f64,
}

impl Throughput {
    pub(super) fn new() -> Self {
        Self {
            sampler: SpeedSampler::new(),
            write_sampler: SpeedSampler::new(),
            written_bytes: 0,
            speed: 0.0,
            per_runner_speed: 0.0,
        }
    }

    /// Returns the download and write speed.
    fn sample(&mut self, meters: MeterSnapshot) -> (f64, f64) {
        let MeterSnapshot { runners, mut bytes } = meters;
        let (_, speed) = self.sampler.sample(&mut bytes);
        let (_, write_speed) = self.write_sampler.sample(&mut self.written_bytes);
        self.speed = speed;
        self.per_runner_speed = if runners == 0 {
            0.0
        } else {
            speed / runners as f64
        };
        (speed, write_speed)
    }
}

impl DownloadLoop {
    /// Sample the throughput and publish a periodic progress event.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    pub(super) fn on_throughput_tick(&mut self) {
        let meters = self.manager.take_meters_snapshot();
        let (speed, write_speed) = self.throughput.sample(meters);
        self.publish_progress(speed, write_speed);
    }

    /// Publish the progress; the event is dropped when the channel is full.
    fn publish_progress(&self, speed: f64, write_speed: f64) {
        let _ = self
            .event_tx
            .try_send(TaskEvent::Downloading(ProgressWithSpeed::new(
                Progress {
                    total: Some(self.manager.total()),
                    downloaded: self.manager.get_total_downloaded(),
                    downloaded_chunks: self.manager.get_downloaded_ranges(),
                },
                speed,
                write_speed,
            )));
    }
}
