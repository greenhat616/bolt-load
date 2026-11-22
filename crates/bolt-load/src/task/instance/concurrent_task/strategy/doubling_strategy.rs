//! The doubling growth strategy (design §9.2, §14.14): every
//! [`GROW_INTERVAL`], while every runner delivers data, double the runners
//! by splitting the largest running chunks, until a growth gains less than
//! [`MIN_GROWTH_GAIN`].
//!
//! It only adds runners: [`ConcurrencyControlStrategy`] lowers the limit,
//! and the growth never goes past it.
//!
//! [`ConcurrencyControlStrategy`]: super::concurrency_control_strategy::ConcurrencyControlStrategy

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use super::{Strategy, StrategyAction};
use crate::task::RunnerId;

/// The least time from one growth to the next. Growth is only decided at
/// strategy ticks, so it comes at the first tick at least this long after
/// the last growth: every other tick at the 1.5 s tick interval.
pub const GROW_INTERVAL: Duration = Duration::from_secs(2);
/// The factor a growth must raise the task's speed by, or the strategy
/// stops growing.
pub const MIN_GROWTH_GAIN: f64 = 1.10;
/// No growth once the task's speed reaches this share of its speed limit.
pub const SPEED_LIMIT_SHARE: f64 = 0.85;
/// The least work a running chunk must have left to be split, in time at
/// its runner's own rate (design §9.3, §14.14).
pub const SPLIT_WINDOW: Duration = Duration::from_secs(2);

/// What the runners did over the last strategy tick (design §9.2, §9.3).
///
/// Counted from the bytes the planner accepted, so bytes clipped off a
/// shrunk chunk do not count.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TickSample {
    /// The task's speed over the tick, in bytes per second; `None` at the
    /// first sample.
    pub speed: Option<f64>,
    /// Whether no runner is connecting and every running one had bytes
    /// accepted in the tick.
    pub all_delivering: bool,
    /// The rate of each runner that was already running the same chunk at
    /// the previous sample, in bytes per second.
    pub rates: HashMap<RunnerId, f64>,
}

/// The planned size of design §9.3: [`SPLIT_WINDOW`] at the split runner's
/// own rate, `donor_speed`, or at `per_runner_speed` before the runner has
/// a rate; never under `min_chunk_size`.
///
/// It is a threshold, not the size of a piece: a running chunk with at
/// least this much left is split at its half (design §14.14). It does not
/// grow with the number of runners, as the task's speed does.
pub fn planned_split_size(
    donor_speed: Option<f64>,
    per_runner_speed: f64,
    min_chunk_size: u64,
) -> u64 {
    let speed = donor_speed.unwrap_or(per_runner_speed);
    // Saturates; a NaN speed plans nothing beyond the minimum.
    let planned = (speed * SPLIT_WINDOW.as_secs_f64()) as u64;
    planned.max(min_chunk_size)
}

/// The inputs of one step.
#[derive(Debug)]
pub struct DoublingStrategyContext {
    pub now: Instant,
    /// The connecting and running runners: the connections in use.
    pub active_runners: usize,
    /// The loop's current concurrency limit: `max_connections`, unless the
    /// concurrency control lowered it.
    pub max_concurrency: usize,
    /// Whether new connections wait for the task's congestion to end
    /// (design §8.1).
    pub congested: bool,
    /// See [`TickSample::all_delivering`].
    pub all_delivering: bool,
    /// See [`TickSample::speed`].
    pub speed: Option<f64>,
    /// The task's speed limit; `None` until there is one (design §10).
    pub speed_limit: Option<f64>,
}

/// A growth whose gain is not judged yet.
#[derive(Debug, Clone, Copy)]
struct Growth {
    /// The task's speed in the tick that decided it.
    speed_before: f64,
    /// The active runners right after it.
    runners: usize,
}

/// What a step asked for, until the loop reports what it added.
#[derive(Debug, Clone, Copy)]
struct Proposal {
    at: Instant,
    speed: f64,
    active_runners: usize,
}

#[derive(Debug, Default)]
pub struct DoublingStrategy {
    /// When a growth last added a runner.
    last_grow: Option<Instant>,
    /// The last growth, until its gain is judged.
    unjudged: Option<Growth>,
    proposal: Option<Proposal>,
    /// A growth gained too little: no more growth in this download.
    plateau: bool,
}

impl DoublingStrategy {
    /// How many runners to add at `context.now`; see the module.
    fn runners_to_add(&mut self, context: &DoublingStrategyContext) -> usize {
        if self.plateau {
            return 0;
        }
        if let Some(last) = self.last_grow
            && context.now < last + GROW_INTERVAL
        {
            return 0;
        }
        let active = context.active_runners;
        if active == 0 || active >= context.max_concurrency {
            return 0;
        }
        // Splits open connections (design §8.1).
        if context.congested {
            return 0;
        }
        // Mend a stalled or connecting runner before adding more.
        if !context.all_delivering {
            return 0;
        }
        let Some(speed) = context.speed else {
            return 0;
        };
        if let Some(limit) = context.speed_limit
            && speed >= SPEED_LIMIT_SHARE * limit
        {
            return 0;
        }
        // Judged once, in the first tick that passes the gates above.
        if let Some(growth) = self.unjudged.take() {
            // With fewer runners than the growth made, the tick does not
            // measure it: no verdict.
            if active >= growth.runners && speed < growth.speed_before * MIN_GROWTH_GAIN {
                self.plateau = true;
                return 0;
            }
        }
        let target = active.saturating_mul(2).min(context.max_concurrency);
        self.proposal = Some(Proposal {
            at: context.now,
            speed,
            active_runners: active,
        });
        target - active
    }

    /// The loop added `added` runners for the last step's
    /// [`StrategyAction::Grow`]. A growth whose splits were all refused,
    /// for want of a slot or of a chunk large enough, is no growth.
    ///
    /// Only a full doubling, as many runners added as were active, is
    /// judged against [`MIN_GROWTH_GAIN`]. A growth cut short by the limit
    /// or by refused splits still waits out [`GROW_INTERVAL`], but a
    /// smaller step cannot be expected to gain 10%, so it is not judged.
    pub fn on_grown(&mut self, added: usize) {
        let Some(proposal) = self.proposal.take() else {
            return;
        };
        if added == 0 {
            return;
        }
        self.last_grow = Some(proposal.at);
        if added >= proposal.active_runners {
            self.unjudged = Some(Growth {
                speed_before: proposal.speed,
                runners: proposal.active_runners + added,
            });
        }
    }
}

impl Strategy for DoublingStrategy {
    type Context<'a> = DoublingStrategyContext;

    fn name() -> &'static str {
        "doubling"
    }

    fn step(&mut self, context: &mut Self::Context<'_>) -> Vec<StrategyAction> {
        self.proposal = None;
        match self.runners_to_add(context) {
            0 => Vec::new(),
            runners => vec![StrategyAction::Grow(runners)],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: f64 = 1024.0 * 1024.0;
    const TICK: Duration = Duration::from_millis(1500);
    const NANO: Duration = Duration::from_nanos(1);

    /// A tick at `now` where every one of `active_runners` delivers, the
    /// task at `speed`, below a limit of 8.
    fn context(now: Instant, active_runners: usize, speed: f64) -> DoublingStrategyContext {
        DoublingStrategyContext {
            now,
            active_runners,
            max_concurrency: 8,
            congested: false,
            all_delivering: true,
            speed: Some(speed),
            speed_limit: None,
        }
    }

    /// Step, and add every runner asked for.
    fn grow(strategy: &mut DoublingStrategy, mut context: DoublingStrategyContext) -> usize {
        let runners = asked(strategy, &mut context);
        strategy.on_grown(runners);
        runners
    }

    /// The runners a step asks for.
    fn asked(strategy: &mut DoublingStrategy, context: &mut DoublingStrategyContext) -> usize {
        match strategy.step(context).as_slice() {
            [] => 0,
            [StrategyAction::Grow(runners)] => *runners,
            other => panic!("unexpected actions: {other:?}"),
        }
    }

    /// Steady gains double the runners at every other tick: 1, 2, 4, 8,
    /// then the limit stops it.
    #[test]
    fn test_steady_growth_doubles_up_to_the_limit() {
        let mut strategy = DoublingStrategy::default();
        let t0 = Instant::now();
        let mut runners = 1;
        let mut levels = vec![runners];
        for tick in 0..12 {
            let now = t0 + TICK * tick;
            // Each runner adds 1 MiB/s.
            runners += grow(&mut strategy, context(now, runners, runners as f64 * MIB));
            if levels.last() != Some(&runners) {
                levels.push(runners);
            }
        }
        assert_eq!(levels, [1, 2, 4, 8]);
    }

    /// A limit that is not a power of two caps the last doubling.
    #[test]
    fn test_the_target_is_capped_at_the_limit() {
        let mut strategy = DoublingStrategy::default();
        let t0 = Instant::now();
        let at = |n: u32, runners: usize| DoublingStrategyContext {
            max_concurrency: 6,
            ..context(t0 + GROW_INTERVAL * n, runners, runners as f64 * MIB)
        };
        assert_eq!(grow(&mut strategy, at(0, 1)), 1);
        assert_eq!(grow(&mut strategy, at(1, 2)), 2);
        assert_eq!(grow(&mut strategy, at(2, 4)), 2);
        assert_eq!(grow(&mut strategy, at(3, 6)), 0);
    }

    /// A lowered limit holds: the strategy grows up to it, never past it.
    #[test]
    fn test_no_growth_at_the_limit() {
        let t0 = Instant::now();
        for (runners, limit) in [(1, 1), (4, 4), (8, 8), (5, 3)] {
            let mut strategy = DoublingStrategy::default();
            let mut context = DoublingStrategyContext {
                max_concurrency: limit,
                ..context(t0, runners, MIB)
            };
            assert_eq!(asked(&mut strategy, &mut context), 0, "{runners}/{limit}");
        }
        let mut strategy = DoublingStrategy::default();
        let mut context = DoublingStrategyContext {
            max_concurrency: 3,
            ..context(t0, 2, MIB)
        };
        assert_eq!(asked(&mut strategy, &mut context), 1);
    }

    /// The gain is judged against 1.10 times the speed before the growth:
    /// exactly that keeps growing, any less stops the growth for good.
    #[test]
    fn test_growth_stops_below_a_ten_percent_gain() {
        let t0 = Instant::now();
        let before = 10.0 * MIB;
        let after = |gain: f64| {
            let mut strategy = DoublingStrategy::default();
            assert_eq!(grow(&mut strategy, context(t0, 2, before)), 2);
            let added = grow(&mut strategy, context(t0 + GROW_INTERVAL, 4, before * gain));
            (strategy, added)
        };
        assert_eq!(after(MIN_GROWTH_GAIN).1, 4);
        let (mut strategy, added) = after(1.0999);
        assert_eq!(added, 0);
        // For good, however fast the task gets.
        for n in 2..10 {
            let now = t0 + GROW_INTERVAL * n;
            assert_eq!(grow(&mut strategy, context(now, 4, 100.0 * MIB)), 0);
        }
    }

    /// A speed that drops after a growth is a plateau too.
    #[test]
    fn test_no_growth_when_the_speed_drops() {
        let mut strategy = DoublingStrategy::default();
        let t0 = Instant::now();
        assert_eq!(grow(&mut strategy, context(t0, 2, 10.0 * MIB)), 2);
        assert_eq!(
            grow(&mut strategy, context(t0 + GROW_INTERVAL, 4, 8.0 * MIB)),
            0
        );
        assert_eq!(
            grow(
                &mut strategy,
                context(t0 + GROW_INTERVAL * 2, 4, 40.0 * MIB)
            ),
            0
        );
    }

    /// Runners that stopped since the growth leave the tick short of it:
    /// no verdict, and the strategy grows on.
    #[test]
    fn test_a_tick_with_fewer_runners_does_not_judge_the_growth() {
        let mut strategy = DoublingStrategy::default();
        let t0 = Instant::now();
        let at = |n: u32, runners: usize, speed: f64| DoublingStrategyContext {
            max_concurrency: 16,
            ..context(t0 + GROW_INTERVAL * n, runners, speed)
        };
        assert_eq!(grow(&mut strategy, at(0, 4, 40.0 * MIB)), 4);
        // Two of the eight finished; the six are slower than the four were.
        assert_eq!(grow(&mut strategy, at(1, 6, 30.0 * MIB)), 6);
        // The growth to twelve is judged: no gain, a plateau.
        assert_eq!(grow(&mut strategy, at(2, 12, 30.0 * MIB)), 0);
        assert_eq!(
            grow(&mut strategy, at(3, 10, 30.0 * MIB)),
            0,
            "the plateau holds"
        );
    }

    /// A growth up to the limit is not judged while the limit holds; once
    /// runners stopped, the tick no longer measures it, so the strategy
    /// adds runners back up to the limit.
    #[test]
    fn test_runners_that_stopped_at_the_limit_are_replaced() {
        let mut strategy = DoublingStrategy::default();
        let t0 = Instant::now();
        assert_eq!(grow(&mut strategy, context(t0, 4, 40.0 * MIB)), 4);
        // No faster with eight, but at the limit.
        assert_eq!(
            grow(&mut strategy, context(t0 + GROW_INTERVAL, 8, 40.0 * MIB)),
            0
        );
        assert_eq!(
            grow(
                &mut strategy,
                context(t0 + GROW_INTERVAL * 2, 7, 35.0 * MIB)
            ),
            1
        );
    }

    /// No growth while a runner connects or delivers nothing; the gain of
    /// the last growth waits for a tick where all deliver.
    #[test]
    fn test_no_growth_while_a_runner_is_inactive() {
        let mut strategy = DoublingStrategy::default();
        let t0 = Instant::now();
        let mut inactive = DoublingStrategyContext {
            all_delivering: false,
            ..context(t0, 1, MIB)
        };
        assert_eq!(asked(&mut strategy, &mut inactive), 0);
        assert_eq!(grow(&mut strategy, context(t0, 1, MIB)), 1);

        // Past the interval, the new runner still connects.
        for n in 1..4 {
            let mut inactive = DoublingStrategyContext {
                all_delivering: false,
                ..context(t0 + GROW_INTERVAL * n, 2, MIB)
            };
            assert_eq!(asked(&mut strategy, &mut inactive), 0);
        }
        // Judged once both deliver.
        assert_eq!(
            grow(&mut strategy, context(t0 + GROW_INTERVAL * 4, 2, 2.0 * MIB)),
            2
        );
    }

    /// No growth before the congestion ends; the next tick after it grows.
    #[test]
    fn test_no_growth_while_congested() {
        let mut strategy = DoublingStrategy::default();
        let t0 = Instant::now();
        let mut congested = DoublingStrategyContext {
            congested: true,
            ..context(t0, 2, MIB)
        };
        assert_eq!(asked(&mut strategy, &mut congested), 0);
        // Not counted as a growth: the next tick may grow at once.
        assert_eq!(grow(&mut strategy, context(t0 + TICK, 2, MIB)), 2);
    }

    /// The first tick has no speed to judge by.
    #[test]
    fn test_no_growth_without_a_speed() {
        let mut strategy = DoublingStrategy::default();
        let mut first = DoublingStrategyContext {
            speed: None,
            ..context(Instant::now(), 1, 0.0)
        };
        assert_eq!(asked(&mut strategy, &mut first), 0);
    }

    /// The next growth comes at the first tick at least 2 s after the last.
    #[test]
    fn test_the_grow_interval() {
        let mut strategy = DoublingStrategy::default();
        let t0 = Instant::now();
        assert_eq!(grow(&mut strategy, context(t0, 1, MIB)), 1);
        assert_eq!(
            grow(
                &mut strategy,
                context(t0 + GROW_INTERVAL - NANO, 2, 2.0 * MIB)
            ),
            0
        );
        assert_eq!(
            grow(&mut strategy, context(t0 + GROW_INTERVAL, 2, 2.0 * MIB)),
            2
        );
        // At the strategy's ticks: one later, too early; two later, due.
        assert_eq!(
            grow(
                &mut strategy,
                context(t0 + GROW_INTERVAL + TICK, 4, 4.0 * MIB)
            ),
            0
        );
        assert_eq!(
            grow(
                &mut strategy,
                context(t0 + GROW_INTERVAL + TICK * 2, 4, 4.0 * MIB)
            ),
            4
        );
    }

    /// A growth whose splits were all refused is no growth: the next tick
    /// tries again. One that added fewer runners than asked still is, and
    /// the interval runs from it.
    #[test]
    fn test_a_refused_split_is_no_growth() {
        let mut strategy = DoublingStrategy::default();
        let t0 = Instant::now();
        assert_eq!(asked(&mut strategy, &mut context(t0, 2, 2.0 * MIB)), 2);
        strategy.on_grown(0);
        assert_eq!(
            asked(&mut strategy, &mut context(t0 + TICK, 2, 2.0 * MIB)),
            2
        );
        strategy.on_grown(1);
        // Three runners now; the interval runs from the partial growth.
        let next = t0 + TICK + GROW_INTERVAL;
        assert_eq!(
            asked(&mut strategy, &mut context(next - NANO, 3, 3.0 * MIB)),
            0
        );
        // Then up to six.
        assert_eq!(asked(&mut strategy, &mut context(next, 3, 3.0 * MIB)), 3);
        // Without a report, the proposal lapses with the next step.
        assert_eq!(
            asked(&mut strategy, &mut context(next + TICK, 3, 3.0 * MIB)),
            3
        );
    }

    /// Only a full doubling is judged: a growth cut short by refused
    /// splits or by the limit gains nothing, and the strategy still grows.
    #[test]
    fn test_a_truncated_growth_is_not_judged() {
        let t0 = Instant::now();
        // Refused: two of four asked.
        let mut strategy = DoublingStrategy::default();
        assert_eq!(asked(&mut strategy, &mut context(t0, 4, 4.0 * MIB)), 4);
        strategy.on_grown(2);
        assert_eq!(
            grow(&mut strategy, context(t0 + GROW_INTERVAL, 6, 4.0 * MIB)),
            2
        );

        // The limit: 11 to 12, then the limit recovers to 13.
        let mut strategy = DoublingStrategy::default();
        let at = |n: u32, runners: usize, limit: usize| DoublingStrategyContext {
            max_concurrency: limit,
            ..context(t0 + GROW_INTERVAL * n, runners, runners as f64 * MIB)
        };
        assert_eq!(grow(&mut strategy, at(0, 11, 12)), 1);
        assert_eq!(grow(&mut strategy, at(1, 12, 13)), 1);
        assert_eq!(grow(&mut strategy, at(2, 13, 16)), 3);

        // A full doubling is judged still: no faster, a plateau.
        assert_eq!(grow(&mut strategy, at(3, 16, 32)), 16);
        let no_faster = DoublingStrategyContext {
            speed: Some(16.0 * MIB),
            ..at(4, 32, 64)
        };
        assert_eq!(grow(&mut strategy, no_faster), 0);
        assert_eq!(grow(&mut strategy, at(5, 32, 64)), 0);
    }

    /// The accepted blind spot (design §14.14): at the limit, a runner that
    /// stops and the one split to refill it, 8, 7, 8 again and again, are
    /// never judged, so even a task no faster with eight finds no plateau
    /// and refills every time.
    #[test]
    fn test_refills_at_the_limit_are_never_judged() {
        let mut strategy = DoublingStrategy::default();
        let t0 = Instant::now();
        assert_eq!(grow(&mut strategy, context(t0, 4, 40.0 * MIB)), 4);
        for cycle in 1..10 {
            let at = t0 + GROW_INTERVAL * (2 * cycle);
            assert_eq!(
                grow(&mut strategy, context(at - TICK, 8, 40.0 * MIB)),
                0,
                "at the limit"
            );
            assert_eq!(
                grow(&mut strategy, context(at, 7, 40.0 * MIB)),
                1,
                "refill {cycle}"
            );
        }
    }

    /// The speed limit gate: no growth at 85% of the limit, growth below.
    #[test]
    fn test_no_growth_near_the_speed_limit() {
        let t0 = Instant::now();
        let limited = |speed: f64| DoublingStrategyContext {
            speed_limit: Some(10.0 * MIB),
            ..context(t0, 2, speed)
        };
        let mut strategy = DoublingStrategy::default();
        assert_eq!(asked(&mut strategy, &mut limited(8.5 * MIB)), 0);
        assert_eq!(asked(&mut strategy, &mut limited(8.4 * MIB)), 2);
    }

    /// The planned size follows the split runner's own rate, not how many
    /// runners share the task's speed.
    #[test]
    fn test_planned_split_size_follows_the_donor() {
        const MIN: u64 = 1 << 20;
        let donor = Some(3.0 * MIB);
        // 16 MiB/s over 2 or over 8 runners.
        let two = planned_split_size(donor, 8.0 * MIB, MIN);
        let eight = planned_split_size(donor, 2.0 * MIB, MIN);
        assert_eq!(two, 6 << 20);
        assert_eq!(two, eight);
        // Without a rate yet, the average rate.
        assert_eq!(planned_split_size(None, 2.0 * MIB, MIN), 4 << 20);
        // Never under the smallest chunk.
        assert_eq!(planned_split_size(Some(1024.0), 0.0, MIN), MIN);
        assert_eq!(planned_split_size(None, 0.0, MIN), MIN);
        assert_eq!(planned_split_size(Some(f64::NAN), 0.0, MIN), MIN);
        assert_eq!(planned_split_size(Some(f64::INFINITY), 0.0, MIN), u64::MAX);
    }
}
