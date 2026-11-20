use std::{
    collections::HashMap,
    task::{Context, Poll},
    time::Instant,
};

use super::{
    super::{
        chunk_planner::ChunkStatus,
        runner_manager::{MeterSnapshot, RunnerManager},
        strategy::TickSample,
    },
    DownloadLoop, LoopEvent,
};
use crate::{
    runtime::EventTimer,
    task::{Progress, RunnerId, Speed, instance::sampler::SpeedSampler},
};

/// Each runner's rate between strategy ticks (design §9.2, §9.3).
///
/// Sampled at every tick with the tick's `now`, from the bytes the planner
/// accepted: the task's total, and each connecting or running runner's
/// chunk. A chunk holds one attempt, so a runner's rate is the growth of
/// its chunk's downloaded part over the time between two samples: one tick,
/// 1.5 s at the strategy's interval. A runner gets a rate at its second
/// sample on the same chunk, told by the chunk's generation: a split keeps
/// it, so the runner keeps its rate; a new claim gets a new one, even by
/// the same ID at the same start.
#[derive(Debug, Default)]
pub(super) struct RunnerRates {
    last: Option<RatesSample>,
    tick: TickSample,
}

#[derive(Debug)]
struct RatesSample {
    at: Instant,
    /// The bytes accepted from every runner so far.
    total: u64,
    /// Each active runner's chunk generation and the bytes accepted into
    /// it.
    runners: HashMap<RunnerId, (u64, u64)>,
}

impl RunnerRates {
    /// Sample the runners of `manager` at `now`, returning what they did
    /// since the previous sample.
    pub(super) fn sample(&mut self, manager: &RunnerManager, now: Instant) -> &TickSample {
        let mut sample = RatesSample {
            at: now,
            total: manager.get_total_downloaded(),
            runners: HashMap::new(),
        };
        let mut all_delivering = true;
        let mut rates = HashMap::new();
        let elapsed = self
            .last
            .as_ref()
            .map(|last| now.saturating_duration_since(last.at).as_secs_f64())
            .filter(|&elapsed| elapsed > 0.0);
        for chunk in manager.active_chunks() {
            let Some(runner_id) = chunk.runner_id else {
                continue;
            };
            let accepted = chunk.downloaded.end - chunk.downloaded.start;
            // The same attempt, or a new one starting from nothing.
            let previous = self
                .last
                .as_ref()
                .and_then(|last| last.runners.get(&runner_id))
                .filter(|&&(generation, _)| generation == chunk.generation)
                .map(|&(_, previous)| previous);
            all_delivering &=
                chunk.status == ChunkStatus::Running && accepted > previous.unwrap_or(0);
            if let (Some(previous), Some(elapsed)) = (previous, elapsed) {
                rates.insert(runner_id, (accepted - previous) as f64 / elapsed);
            }
            sample
                .runners
                .insert(runner_id, (chunk.generation, accepted));
        }
        let speed = self
            .last
            .as_ref()
            .zip(elapsed)
            .map(|(last, elapsed)| (sample.total - last.total) as f64 / elapsed);
        self.last = Some(sample);
        self.tick = TickSample {
            speed,
            all_delivering,
            rates,
        };
        &self.tick
    }

    /// The rate of `runner_id` at the last sample, if it had one.
    pub(super) fn rate(&self, runner_id: RunnerId) -> Option<f64> {
        self.tick.rates.get(&runner_id).copied()
    }
}

/// Download and write throughput, sampled on its timer.
pub(super) struct ThroughputSampler {
    timer: EventTimer,
    sampler: SpeedSampler,
    write_sampler: SpeedSampler,
    /// Bytes written since the last sample.
    written_bytes: usize,
    pub(super) speed: f64,
    pub(super) per_runner_speed: f64,
}

impl ThroughputSampler {
    pub(super) fn new(timer: EventTimer) -> Self {
        Self {
            timer,
            sampler: SpeedSampler::new(),
            write_sampler: SpeedSampler::new(),
            written_bytes: 0,
            speed: 0.0,
            per_runner_speed: 0.0,
        }
    }

    #[inline]
    pub(super) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<LoopEvent> {
        self.timer
            .poll_tick(cx)
            .map(|()| LoopEvent::ThroughputTimer)
    }

    pub(super) fn record_written(&mut self, bytes: usize) {
        self.written_bytes += bytes;
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

    /// Publish the progress, unless nobody observes it.
    fn publish_progress(&self, speed: f64, write_speed: f64) {
        if !self.reporter.is_observed() {
            return;
        }
        self.reporter.report(
            Progress::new(
                Some(self.manager.total()),
                self.manager.get_total_downloaded(),
                self.manager.get_downloaded_ranges(),
            ),
            Speed::new(speed, write_speed),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use pretty_assertions::assert_eq;

    use super::*;

    const MIB: u64 = 1 << 20;
    const TICK: Duration = Duration::from_millis(1500);

    fn rates(tick: &TickSample) -> Vec<(RunnerId, f64)> {
        let mut rates: Vec<_> = tick.rates.iter().map(|(&id, &rate)| (id, rate)).collect();
        rates.sort_by_key(|&(id, _)| id);
        rates
    }

    /// A rate spans the time between two samples, from the bytes the
    /// planner accepted; the first sample has none.
    #[test]
    fn test_a_rate_spans_one_tick() {
        let mut manager = RunnerManager::new(100 * MIB, 4);
        let _slot = manager.allocate_runner_with_chunk(0..50 * MIB).unwrap();
        let mut sampler = RunnerRates::default();
        let t0 = Instant::now();
        let tick = sampler.sample(&manager, t0).clone();
        assert_eq!(tick.speed, None);
        assert!(!tick.all_delivering, "nothing accepted yet");
        assert_eq!(rates(&tick), []);

        manager.update_progress(0, 3 * MIB).unwrap();
        let tick = sampler.sample(&manager, t0 + TICK).clone();
        assert_eq!(tick.speed, Some(2.0 * MIB as f64));
        assert!(tick.all_delivering);
        assert_eq!(rates(&tick), [(0, 2.0 * MIB as f64)]);
        assert_eq!(sampler.rate(0), Some(2.0 * MIB as f64));

        // Bytes past the chunk's end are clipped, so not counted; the
        // complete chunk is no longer sampled.
        manager.update_progress(0, 100 * MIB).unwrap();
        let tick = sampler.sample(&manager, t0 + TICK * 2).clone();
        assert_eq!(tick.speed, Some(47.0 * MIB as f64 / 1.5));
        assert_eq!(rates(&tick), []);
    }

    /// A runner new since the last sample delivers when it had bytes
    /// accepted, but has no rate yet. A connecting runner, or a running
    /// one without new bytes, does not deliver.
    #[test]
    fn test_who_delivers() {
        let mut manager = RunnerManager::new(100 * MIB, 4);
        let _first = manager.allocate_runner_with_chunk(0..10 * MIB).unwrap();
        let mut sampler = RunnerRates::default();
        let t0 = Instant::now();
        manager.update_progress(0, MIB).unwrap();
        sampler.sample(&manager, t0);

        let _second = manager
            .allocate_runner_with_chunk(10 * MIB..20 * MIB)
            .unwrap();
        manager.update_progress(0, MIB).unwrap();
        manager.update_progress(1, MIB).unwrap();
        let tick = sampler.sample(&manager, t0 + TICK).clone();
        assert!(tick.all_delivering);
        assert_eq!(rates(&tick), [(0, MIB as f64 / 1.5)]);

        // Runner 1 delivers nothing in this tick.
        manager.update_progress(0, MIB).unwrap();
        let tick = sampler.sample(&manager, t0 + TICK * 2).clone();
        assert!(!tick.all_delivering);
        assert_eq!(rates(&tick), [(0, MIB as f64 / 1.5), (1, 0.0)]);

        // Runner 2 connects.
        assert!(manager.allocate_chunk(20 * MIB..30 * MIB, 2));
        manager.update_progress(0, MIB).unwrap();
        manager.update_progress(1, MIB).unwrap();
        let tick = sampler.sample(&manager, t0 + TICK * 3).clone();
        assert!(!tick.all_delivering);
        // A finished chunk is no longer sampled.
        manager.update_progress(1, 10 * MIB).unwrap();
        manager.mark_started(2);
        manager.update_progress(0, MIB).unwrap();
        manager.update_progress(2, MIB).unwrap();
        let tick = sampler.sample(&manager, t0 + TICK * 4).clone();
        assert!(tick.all_delivering);
        assert_eq!(rates(&tick), [(0, MIB as f64 / 1.5), (2, MIB as f64 / 1.5)]);
    }

    /// A rate belongs to the attempt: a split keeps it, an ID reused for
    /// another chunk starts without one.
    #[test]
    fn test_a_rate_follows_the_chunk() {
        let mut manager = RunnerManager::new(100 * MIB, 4);
        let _slot = manager.allocate_runner_with_chunk(0..50 * MIB).unwrap();
        let mut sampler = RunnerRates::default();
        let t0 = Instant::now();
        sampler.sample(&manager, t0);
        manager.update_progress(0, 3 * MIB).unwrap();
        manager.resize_runner(0, 10 * MIB).unwrap();
        let tick = sampler.sample(&manager, t0 + TICK).clone();
        assert_eq!(rates(&tick), [(0, 2.0 * MIB as f64)]);

        // Runner 0 finishes, and its ID runs another chunk.
        manager.update_progress(0, 10 * MIB).unwrap();
        manager.release_runner_full(0, true).unwrap();
        let _slot = manager
            .allocate_runner_with_chunk(50 * MIB..100 * MIB)
            .unwrap();
        manager.update_progress(0, 6 * MIB).unwrap();
        let tick = sampler.sample(&manager, t0 + TICK * 2).clone();
        assert_eq!(rates(&tick), []);
        assert!(tick.all_delivering);
        assert_eq!(tick.speed, Some(16.0 * MIB as f64 / 1.5));
    }

    /// A runner whose attempt failed without a byte accepted, and whose ID
    /// claims the same start again, is a new attempt: no rate at its first
    /// sample, one at its second.
    #[test]
    fn test_a_retry_on_the_same_start_starts_without_a_rate() {
        let mut manager = RunnerManager::new(100 * MIB, 4);
        let _first = manager.allocate_runner_with_chunk(0..50 * MIB).unwrap();
        let mut sampler = RunnerRates::default();
        let t0 = Instant::now();
        sampler.sample(&manager, t0);

        let released = manager.release_runner_full(0, false).unwrap();
        assert_eq!(released, Some(0..50 * MIB));
        let _retry = manager.allocate_runner_with_chunk(0..50 * MIB).unwrap();
        assert_eq!(manager.get_runner_state(0).unwrap().allocated, 0..50 * MIB);
        manager.update_progress(0, 3 * MIB).unwrap();
        let tick = sampler.sample(&manager, t0 + TICK).clone();
        assert!(tick.all_delivering);
        assert_eq!(rates(&tick), []);
        assert_eq!(sampler.rate(0), None);

        manager.update_progress(0, 3 * MIB).unwrap();
        let tick = sampler.sample(&manager, t0 + TICK * 2).clone();
        assert_eq!(rates(&tick), [(0, 2.0 * MIB as f64)]);
    }

    /// Two adjacent ranges fail before a byte and their cooldowns merge;
    /// runner 0 claims part of the merged range, finishes it, and claims
    /// the rest.
    /// Every claim is a new generation, and each attempt's first sample has
    /// no rate.
    #[test]
    fn test_claims_from_a_merged_cooldown_are_new_attempts() {
        let mut manager = RunnerManager::new(100 * MIB, 4);
        let _first = manager.allocate_runner_with_chunk(0..10 * MIB).unwrap();
        let _second = manager
            .allocate_runner_with_chunk(10 * MIB..20 * MIB)
            .unwrap();
        let generation = |manager: &RunnerManager| manager.get_runner_state(0).unwrap().generation;
        let mut generations = vec![generation(&manager)];
        let mut sampler = RunnerRates::default();
        let t0 = Instant::now();
        sampler.sample(&manager, t0);

        // Both fail before a byte.
        for runner_id in [0, 1] {
            let released = manager.mark_failed(runner_id).unwrap();
            manager.release_runner(runner_id);
            manager.cool_down(released.range, 1, 1, t0);
        }
        let cooldowns = manager.cooldowns();
        let [(merged, _)] = cooldowns.as_slice() else {
            panic!("expected one merged cooldown, got {cooldowns:?}");
        };
        assert_eq!(*merged, 0..20 * MIB);

        // Runner 0 claims part of it.
        let _claim = manager.allocate_runner_with_chunk(0..5 * MIB).unwrap();
        generations.push(generation(&manager));
        assert_eq!(manager.cooldowns()[0].0, 5 * MIB..20 * MIB);
        manager.update_progress(0, MIB).unwrap();
        let tick = sampler.sample(&manager, t0 + TICK).clone();
        assert_eq!(rates(&tick), []);
        manager.update_progress(0, 3 * MIB).unwrap();
        let tick = sampler.sample(&manager, t0 + TICK * 2).clone();
        assert_eq!(rates(&tick), [(0, 2.0 * MIB as f64)]);

        // It finishes, and the same ID claims the rest.
        manager.update_progress(0, MIB).unwrap();
        manager.release_runner_full(0, true).unwrap();
        let _rest = manager
            .allocate_runner_with_chunk(5 * MIB..20 * MIB)
            .unwrap();
        assert_eq!(manager.cooldowns(), []);
        generations.push(generation(&manager));
        manager.update_progress(0, MIB).unwrap();
        let tick = sampler.sample(&manager, t0 + TICK * 3).clone();
        assert_eq!(rates(&tick), []);
        manager.update_progress(0, 3 * MIB).unwrap();
        let tick = sampler.sample(&manager, t0 + TICK * 4).clone();
        assert_eq!(rates(&tick), [(0, 2.0 * MIB as f64)]);

        let mut distinct = generations.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), 3, "{generations:?}");
    }

    /// Two samples at once span no time: no speed and no rate.
    #[test]
    fn test_samples_at_the_same_instant() {
        let mut manager = RunnerManager::new(100 * MIB, 4);
        let _slot = manager.allocate_runner_with_chunk(0..50 * MIB).unwrap();
        let mut sampler = RunnerRates::default();
        let t0 = Instant::now();
        sampler.sample(&manager, t0);
        manager.update_progress(0, MIB).unwrap();
        let tick = sampler.sample(&manager, t0).clone();
        assert_eq!(tick.speed, None);
        assert_eq!(rates(&tick), []);
        assert!(tick.all_delivering);
    }
}
