use std::{
    collections::{HashMap, VecDeque},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use super::{
    super::{
        chunk_planner::ChunkStatus,
        runner_manager::{MeterSnapshot, RunnerManager},
        strategy::{SLOW_WINDOW, TickSample},
    },
    DownloadLoop, LoopEvent,
};
use crate::{
    runtime::EventTimer,
    task::{Progress, RunnerId, Speed, instance::sampler::SpeedSampler},
};

/// Each runner's rate between strategy ticks (design §9.2, §9.3), and over
/// the slow window (design §9.5).
///
/// Sampled at every tick with the tick's `now`, from the bytes the planner
/// accepted: the task's total, and each connecting or running runner's
/// chunk. A chunk holds one attempt, so a runner's rate is the growth of
/// its chunk's downloaded part over the time between two samples: one tick,
/// 1.5 s at the strategy's interval. A runner gets a rate at its second
/// sample on the same chunk, told by the chunk's generation: a split keeps
/// it, so the runner keeps its rate; a new claim gets a new one, even by
/// the same ID at the same start.
///
/// The samples of the last `window` are kept for the bytes over it, see
/// [`Self::window_bytes`].
#[derive(Debug)]
pub(super) struct RunnerRates {
    /// The samples from the latest one at least `window` before the last
    /// one, oldest first.
    history: VecDeque<RatesSample>,
    tick: TickSample,
    window: Duration,
}

impl Default for RunnerRates {
    fn default() -> Self {
        Self {
            history: VecDeque::new(),
            tick: TickSample::default(),
            window: SLOW_WINDOW,
        }
    }
}

#[derive(Debug)]
struct RatesSample {
    at: Instant,
    /// The bytes accepted from every runner so far.
    total: u64,
    /// Each active runner's attempt.
    runners: HashMap<RunnerId, RunnerSample>,
}

/// One runner's attempt at a sample.
#[derive(Debug, Clone, Copy)]
struct RunnerSample {
    /// The generation of its chunk.
    generation: u64,
    /// The bytes accepted into its chunk.
    accepted: u64,
    /// Whether it had connected.
    running: bool,
}

impl RunnerRates {
    /// Sample the runners of `manager` at `now`, returning what they did
    /// since the previous sample; no runner is let off as silent.
    #[cfg(test)]
    pub(super) fn sample(&mut self, manager: &RunnerManager, now: Instant) -> &TickSample {
        self.sample_at(manager, now, None)
    }

    /// Sample the runners of `manager` at `now`, returning what they did
    /// since the previous sample.
    ///
    /// A running runner that had no bytes accepted in the tick does not
    /// count against [`TickSample::all_delivering`] when it is silent at
    /// `silent_since`: it neither delivered a frame nor read its body
    /// since then, see [`RunnerManager::silent_since`], it has no frame
    /// queued that the loop has not taken, and it was already running at a
    /// sample at or before it, so the time it took to connect does not
    /// count. The restart and the hedge
    /// deal with it (design §9.2, §14.19). `None` lets none off, as while
    /// the writer is full.
    pub(super) fn sample_at(
        &mut self,
        manager: &RunnerManager,
        now: Instant,
        silent_since: Option<Instant>,
    ) -> &TickSample {
        let mut sample = RatesSample {
            at: now,
            total: manager.get_total_downloaded(),
            runners: HashMap::new(),
        };
        let last = self.history.back();
        let mut all_delivering = true;
        let mut silent = false;
        let mut rates = HashMap::new();
        let elapsed = last
            .map(|last| now.saturating_duration_since(last.at).as_secs_f64())
            .filter(|&elapsed| elapsed > 0.0);
        for chunk in manager.active_chunks() {
            let Some(runner_id) = chunk.runner_id else {
                continue;
            };
            let accepted = chunk.downloaded.end - chunk.downloaded.start;
            let running = chunk.status == ChunkStatus::Running;
            // The same attempt, or a new one starting from nothing.
            let previous = last
                .and_then(|last| last.runners.get(&runner_id))
                .filter(|previous| previous.generation == chunk.generation)
                .map(|previous| previous.accepted);
            let delivered = accepted > previous.unwrap_or(0);
            let let_off = running
                && !delivered
                && silent_since.is_some_and(|since| {
                    !manager.has_queued_data(runner_id)
                        && manager
                            .silent_since(runner_id, now)
                            .is_some_and(|last| last <= since)
                        && self.was_running(runner_id, chunk.generation, since)
                });
            silent |= let_off;
            all_delivering &= running && delivered || let_off;
            if let (Some(previous), Some(elapsed)) = (previous, elapsed) {
                rates.insert(runner_id, (accepted - previous) as f64 / elapsed);
            }
            sample.runners.insert(
                runner_id,
                RunnerSample {
                    generation: chunk.generation,
                    accepted,
                    running,
                },
            );
        }
        let speed = last
            .zip(elapsed)
            .map(|(last, elapsed)| (sample.total - last.total) as f64 / elapsed);
        self.history.push_back(sample);
        // The next window starts at the latest sample a window back.
        while self
            .history
            .get(1)
            .is_some_and(|next| next.at + self.window <= now)
        {
            self.history.pop_front();
        }
        self.tick = TickSample {
            speed,
            all_delivering,
            silent,
            rates,
        };
        &self.tick
    }

    /// What the runners did over the tick that ended at the last sample.
    pub(super) fn tick(&self) -> &TickSample {
        &self.tick
    }

    /// Whether the attempt `runner_id` is on at the last sample was
    /// already running at the latest sample at or before `at`: it has been
    /// connected since then at least (design §14.19).
    pub(super) fn running_since(&self, runner_id: RunnerId, at: Instant) -> bool {
        self.history
            .back()
            .and_then(|last| last.runners.get(&runner_id))
            .is_some_and(|current| self.was_running(runner_id, current.generation, at))
    }

    /// Whether `runner_id` was running the attempt of `generation` at the
    /// latest sample at or before `at`.
    fn was_running(&self, runner_id: RunnerId, generation: u64, at: Instant) -> bool {
        self.history
            .iter()
            .rev()
            .find(|sample| sample.at <= at)
            .and_then(|sample| sample.runners.get(&runner_id))
            .is_some_and(|then| then.running && then.generation == generation)
    }

    /// The rate of `runner_id` at the last sample, if it had one.
    pub(super) fn rate(&self, runner_id: RunnerId) -> Option<f64> {
        self.tick.rates.get(&runner_id).copied()
    }

    /// The bytes the planner accepted from each runner over a full window
    /// at the last sample (design §9.5).
    ///
    /// The window starts at the latest sample at least `window` before the
    /// last one, the same for every runner; a runner has a full window
    /// when it was running the same attempt at both samples. A runner
    /// younger than the window, one that was still connecting at its
    /// start, and one on a new attempt since have none. At 1.5 s ticks a
    /// 10 s window spans 10.5 s.
    pub(super) fn window_bytes(&self) -> Vec<(RunnerId, u64)> {
        let Some(last) = self.history.back() else {
            return Vec::new();
        };
        let Some(start) = self
            .history
            .iter()
            .rev()
            .find(|sample| sample.at + self.window <= last.at)
        else {
            return Vec::new();
        };
        last.runners
            .iter()
            .filter_map(|(&runner_id, now)| {
                let then = start.runners.get(&runner_id)?;
                (then.running && then.generation == now.generation)
                    .then(|| (runner_id, now.accepted - then.accepted))
            })
            .collect()
    }
}

/// Download and write throughput, sampled on its timer.
pub(super) struct ThroughputSampler {
    timer: EventTimer,
    sampler: SpeedSampler,
    write_sampler: SpeedSampler,
    /// Bytes written since the last sample.
    written_bytes: usize,
    pub(super) per_runner_speed: f64,
}

impl ThroughputSampler {
    pub(super) fn new(timer: EventTimer) -> Self {
        Self {
            timer,
            sampler: SpeedSampler::new(),
            write_sampler: SpeedSampler::new(),
            written_bytes: 0,
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
                self.manager.written().ranges().to_vec(),
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

    /// A runner has been running since `at` when it ran the same attempt at
    /// the latest sample at or before `at` (design §14.19).
    #[test]
    fn test_running_since() {
        let mut manager = RunnerManager::new(100 * MIB, 4);
        let _a = manager.allocate_runner_with_chunk(0..20 * MIB).unwrap();
        let _b = manager
            .allocate_runner_with_chunk(20 * MIB..40 * MIB)
            .unwrap();
        assert!(manager.allocate_chunk(40 * MIB..60 * MIB, 2));
        let mut sampler = RunnerRates::default();
        let t0 = Instant::now();
        sampler.sample(&manager, t0);
        // Runner 2 connects after the first sample; 0 delivers 3 MiB a
        // tick, 1 a MiB in the first tick only, 2 nothing.
        manager.mark_started(2);
        for n in 1..=3 {
            manager.update_progress(0, 3 * MIB).unwrap();
            if n == 1 {
                manager.update_progress(1, MIB).unwrap();
            }
            sampler.sample(&manager, t0 + TICK * n);
        }
        let nano = Duration::from_nanos(1);
        assert!(sampler.running_since(0, t0));
        assert!(!sampler.running_since(0, t0 - nano), "no sample that early");
        assert!(
            !sampler.running_since(2, t0 + TICK - nano),
            "connecting then"
        );
        assert!(sampler.running_since(2, t0 + TICK));
        assert!(!sampler.running_since(3, t0 + TICK), "no such runner");
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

    /// A runner has bytes over a window once it ran the same attempt a full
    /// window before the last sample, not a nanosecond earlier. The window
    /// starts at the latest sample a window back, so at 1.5 s ticks it
    /// spans 10.5 s, and only the samples since are kept.
    #[test]
    fn test_window_bytes_need_a_full_window() {
        const NANO: Duration = Duration::from_nanos(1);
        let mut manager = RunnerManager::new(100 * MIB, 4);
        let _slot = manager.allocate_runner_with_chunk(0..50 * MIB).unwrap();
        let mut sampler = RunnerRates::default();
        let t0 = Instant::now();
        sampler.sample(&manager, t0);
        assert_eq!(sampler.window_bytes(), []);
        manager.update_progress(0, 5 * MIB).unwrap();
        sampler.sample(&manager, t0 + SLOW_WINDOW - NANO);
        assert_eq!(sampler.window_bytes(), []);
        manager.update_progress(0, 5 * MIB).unwrap();
        sampler.sample(&manager, t0 + SLOW_WINDOW);
        assert_eq!(sampler.window_bytes(), [(0, 10 * MIB)]);

        let mut sampler = RunnerRates::default();
        for n in 0..=20 {
            manager.update_progress(0, MIB).unwrap();
            sampler.sample(&manager, t0 + TICK * n);
            let bytes = sampler.window_bytes();
            if n < 7 {
                assert_eq!(bytes, [], "tick {n}");
            } else {
                assert_eq!(bytes, [(0, 7 * MIB)], "tick {n}");
                assert_eq!(sampler.history.len(), 8, "tick {n}");
            }
        }
    }

    /// Only a running attempt has bytes over a window: not a runner that was
    /// still connecting at the start of the window, nor one on a new
    /// attempt since. A split keeps it.
    #[test]
    fn test_window_bytes_belong_to_a_running_attempt() {
        let mut manager = RunnerManager::new(100 * MIB, 4);
        let _first = manager.allocate_runner_with_chunk(0..40 * MIB).unwrap();
        let _second = manager
            .allocate_runner_with_chunk(40 * MIB..60 * MIB)
            .unwrap();
        // Runner 2 connects.
        assert!(manager.allocate_chunk(60 * MIB..80 * MIB, 2));
        let mut sampler = RunnerRates::default();
        let t0 = Instant::now();
        sampler.sample(&manager, t0);
        manager.mark_started(2);
        sampler.sample(&manager, t0 + TICK);

        // Runner 0 is split; runner 1 fails before a byte, and its ID
        // claims the same start again.
        manager.resize_runner(0, 10 * MIB).unwrap();
        manager.release_runner_full(1, false).unwrap();
        let _retry = manager
            .allocate_runner_with_chunk(40 * MIB..60 * MIB)
            .unwrap();
        for runner_id in 0..3 {
            manager.update_progress(runner_id, 2 * MIB).unwrap();
        }
        sampler.sample(&manager, t0 + SLOW_WINDOW);
        let mut bytes = sampler.window_bytes();
        assert_eq!(bytes, [(0, 2 * MIB)]);

        // Runner 2 was running at the start of this window.
        sampler.sample(&manager, t0 + TICK + SLOW_WINDOW);
        bytes = sampler.window_bytes();
        bytes.sort_by_key(|&(runner_id, _)| runner_id);
        assert_eq!(bytes, [(0, 2 * MIB), (2, 2 * MIB)]);
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
