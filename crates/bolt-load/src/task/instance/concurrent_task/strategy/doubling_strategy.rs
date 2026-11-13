//! The doubling growth strategy (design §9.2, §14.14, §14.33): every
//! [`GROW_INTERVAL`], while every runner delivers data, double the runners
//! by splitting the largest running chunks, until a doubling gains less
//! than [`MIN_GROWTH_GAIN`]; then hold the runners it doubled from, and
//! probe that hold again later.
//!
//! A full doubling is judged at the first tick that measures it, the
//! connection limit included. One that reached the limit holds the growth
//! until then, so runners that finish are not replaced after it; one below
//! the limit that loses runners first is not judged (design §14.18). Every
//! pending growth ends by its deadline, [`growth_deadline`]: a doubling
//! still unjudged then is dropped without a verdict.
//!
//! A hold of `h` runners wants `h` connections: it refills up to them by
//! splits and adds nothing while more are active. After a wait it probes
//! again from the `b ≥ h` runners active then, by doubling towards
//! `min(2b, limit)`. A gain resumes the ramp; no gain holds `b` and doubles
//! the wait, up to [`MAX_REPROBE_WAIT`]. The runners a gainless doubling
//! added are not stopped: they finish their chunks, and a probe may start
//! from them.
//!
//! It only adds runners: [`ConcurrencyControlStrategy`] lowers the limit,
//! and the growth never goes past it. What the growth wants open, see
//! [`DoublingStrategy::want`], is what lets the limit controller step above
//! its limit.
//!
//! [`ConcurrencyControlStrategy`]: super::concurrency_control_strategy::ConcurrencyControlStrategy

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use bolt_load_utils::telemetry::*;

use super::{Strategy, StrategyAction};
use crate::task::RunnerId;

/// The least time from one growth to the next. Growth is only decided at
/// strategy ticks, so it comes at the first tick at least this long after
/// the last growth: every other tick at the 1.5 s tick interval.
pub const GROW_INTERVAL: Duration = Duration::from_secs(2);
/// The factor a growth must raise the task's speed by, or the strategy
/// holds.
pub const MIN_GROWTH_GAIN: f64 = 1.10;
/// No growth once the task's speed reaches this share of its speed limit.
pub const SPEED_LIMIT_SHARE: f64 = 0.85;
/// The least work a running chunk must have left to be split, in time at
/// its runner's own rate (design §9.3, §14.14).
pub const SPLIT_WINDOW: Duration = Duration::from_secs(2);
/// The wait before a hold is probed again, while no probe gained.
pub const BASE_REPROBE_WAIT: Duration = Duration::from_secs(30);
/// The longest wait before a hold is probed again, however many probes
/// gained nothing.
pub const MAX_REPROBE_WAIT: Duration = Duration::from_secs(300);

/// How long a growth may stay unjudged, from the tick that decided it,
/// given runners that may take `connect_timeout` to connect: by then its
/// runners have connected or failed. Saturates rather than overflow.
pub fn growth_deadline(connect_timeout: Duration) -> Duration {
    connect_timeout.saturating_add(GROW_INTERVAL.saturating_mul(5))
}

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
    /// accepted in the tick, or is silent: none accepted for at least the
    /// hedge's idle time (design §9.2, §14.19).
    pub all_delivering: bool,
    /// Whether a running runner had no bytes accepted in the tick and was
    /// let off as silent: the tick's speed leaves its share out.
    pub silent: bool,
    /// Whether the same attempts were running at the previous sample and
    /// at this one, and no other runner was active at either: none
    /// finished, failed, was added or connected in the tick, so its speed
    /// is the speed of the runners active now. Only such a tick judges a
    /// growth or measures the speed a probe starts from (design §14.33).
    pub stable: bool,
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
    /// The connections the limit controller allows: `max_connections`,
    /// unless it lowered the limit, one more while it steps. Whatever the
    /// growth wants, see [`DoublingStrategy::want`].
    pub max_concurrency: usize,
    /// Whether an unassigned range may be claimed now: the limit
    /// controller creates a runner for it, so a hold does not split for
    /// that slot.
    pub eligible_ranges: bool,
    /// Whether new connections wait for the task's congestion to end
    /// (design §8.1).
    pub congested: bool,
    /// See [`TickSample::all_delivering`].
    pub all_delivering: bool,
    /// See [`TickSample::silent`].
    pub silent_runners: bool,
    /// See [`TickSample::stable`].
    pub stable: bool,
    /// See [`TickSample::speed`].
    pub speed: Option<f64>,
    /// The task's speed limit; `None` until there is one (design §10).
    pub speed_limit: Option<f64>,
}

/// Where the growth is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Doubling while each doubling gains.
    Ramp,
    /// A full doubling gained too little: hold `h` runners, probe again at
    /// `reprobe_at`.
    Hold { h: usize, reprobe_at: Instant },
}

/// A growth whose gain is not judged yet.
#[derive(Debug, Clone, Copy)]
struct Growth {
    /// The tick that decided it: its deadline counts from here.
    started_at: Instant,
    /// The task's speed in the tick that decided it.
    speed_before: f64,
    /// The active runners right before it.
    runners_before: usize,
    /// The active runners right after it.
    runners: usize,
    /// Whether it reached the concurrency limit of the tick that decided
    /// it.
    at_limit: bool,
    /// The connections a probe of a hold wants open while it is pending;
    /// `None` for a doubling of the ramp.
    probe_goal: Option<usize>,
}

/// What a step asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ask {
    /// A doubling of the ramp, or a growth the limit cut short.
    Ramp,
    /// Splits to bring a hold back to its runners; never judged.
    Refill,
    /// A probe of a hold, towards `goal` connections.
    Probe { goal: usize },
}

/// What a step asked for, until the loop reports what it added.
#[derive(Debug, Clone, Copy)]
struct Proposal {
    at: Instant,
    speed: f64,
    active_runners: usize,
    /// The concurrency limit of the tick, kept: the limit may change
    /// before the growth is judged.
    max_concurrency: usize,
    ask: Ask,
}

#[derive(Debug)]
pub struct DoublingStrategy {
    mode: Mode,
    /// The wait from a hold's start, or from its last probe, to its next
    /// probe.
    reprobe_wait: Duration,
    /// See [`growth_deadline`].
    growth_deadline: Duration,
    /// When a growth last added a runner.
    last_grow: Option<Instant>,
    /// The last full doubling or judged probe, until a tick measures its
    /// gain.
    unjudged: Option<Growth>,
    proposal: Option<Proposal>,
    /// The first tick a hold's probe was due and no tick had measured the
    /// runners active then; cleared whenever the mode is set (design
    /// §14.33).
    baseline_since: Option<Instant>,
}

impl Default for DoublingStrategy {
    fn default() -> Self {
        Self::new(crate::runner::DEFAULT_CONNECT_TIMEOUT)
    }
}

impl DoublingStrategy {
    /// A strategy for runners that may take `connect_timeout` to connect.
    pub fn new(connect_timeout: Duration) -> Self {
        Self {
            mode: Mode::Ramp,
            reprobe_wait: BASE_REPROBE_WAIT,
            growth_deadline: growth_deadline(connect_timeout),
            last_grow: None,
            unjudged: None,
            proposal: None,
            baseline_since: None,
        }
    }

    /// The connections the growth wants open, of `max_connections`: all
    /// of them while ramping; `h` in a hold; a pending probe's goal.
    pub fn want(&self, max_connections: usize) -> usize {
        match self.mode {
            Mode::Ramp => max_connections,
            Mode::Hold { h, .. } => self
                .unjudged
                .and_then(|growth| growth.probe_goal)
                .or(match self.proposal {
                    Some(Proposal {
                        ask: Ask::Probe { goal },
                        ..
                    }) => Some(goal),
                    _ => None,
                })
                .unwrap_or(h),
        }
    }

    /// End a growth still unjudged at its deadline, at `now`: a doubling
    /// of the ramp is dropped without a verdict, a probe is inconclusive.
    /// A search for a tick to probe from ends at its deadline too.
    ///
    /// The step calls it first; the loop also calls it before the limit
    /// controller steps, so that a pending probe's goal is not wanted past
    /// its deadline, and a tick the limit controller acts in still ends
    /// what is due.
    pub fn expire(&mut self, now: Instant) {
        self.expire_baseline(now);
        let Some(growth) = self.unjudged else {
            return;
        };
        if growth
            .started_at
            .checked_add(self.growth_deadline)
            .is_none_or(|deadline| now < deadline)
        {
            return;
        }
        self.unjudged = None;
        if growth.probe_goal.is_some() {
            debug!(
                "[STRATEGY] the probe to {} runners is unjudged at its deadline",
                growth.runners
            );
            self.inconclusive(now);
        } else {
            debug!(
                "[STRATEGY] the doubling to {} runners is unjudged at its deadline: dropped",
                growth.runners
            );
        }
    }

    /// End a search for a tick to probe from, see [`Self::hold`], that has
    /// gone on for [`growth_deadline`] at `now`: the probe is
    /// inconclusive. Checked with the growth's deadline, so before the
    /// limit controller acts and before any other gate (design §14.33).
    fn expire_baseline(&mut self, now: Instant) {
        let Some(since) = self.baseline_since else {
            return;
        };
        if since
            .checked_add(self.growth_deadline)
            .is_none_or(|deadline| now < deadline)
        {
            return;
        }
        self.baseline_since = None;
        if let Mode::Hold { h, .. } = self.mode {
            debug!(
                "[STRATEGY] no tick measured the hold of {h} runners since {:?}: the probe is                  inconclusive",
                now - since
            );
            self.inconclusive(now);
        }
    }

    /// A probe ended without a verdict at `now`: the hold stays, with the
    /// same wait.
    fn inconclusive(&mut self, now: Instant) {
        if let Mode::Hold { h, .. } = self.mode {
            self.set_mode(Mode::Hold {
                h,
                reprobe_at: now + self.reprobe_wait,
            });
        }
    }

    /// Enter `mode`, with no baseline sought.
    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.baseline_since = None;
    }

    /// How many runners to add at `context.now`; see the module.
    fn runners_to_add(&mut self, context: &DoublingStrategyContext) -> usize {
        let now = context.now;
        self.expire(now);
        if let Some(last) = self.last_grow
            && now < last + GROW_INTERVAL
        {
            return 0;
        }
        let active = context.active_runners;
        if active == 0 {
            return 0;
        }
        // Splits open connections (design §8.1).
        if context.congested {
            return 0;
        }
        if let Mode::Hold { h, reprobe_at } = self.mode
            && self.unjudged.is_none()
        {
            return self.hold(context, h, reprobe_at);
        }
        let Some(speed) = self.measured_speed(context) else {
            return 0;
        };
        // Judged once, in the first tick that passes the gates above, at
        // the limit too (design §14.18).
        if let Some(growth) = self.unjudged.take() {
            // With fewer runners than the growth made, the tick does not
            // measure it. A doubling to the limit and a probe wait for
            // them until their deadline, and nothing refills meanwhile;
            // one below the limit has no verdict, and the strategy grows
            // on.
            //
            // A silent runner adds nothing to the speed, so a tick with one
            // does not measure the growth either, and the same rules apply
            // (design §14.19).
            if active < growth.runners || context.silent_runners {
                if growth.at_limit || growth.probe_goal.is_some() {
                    self.unjudged = Some(growth);
                    return 0;
                }
            } else if !context.stable {
                // A runner finished, failed, was added or connected in the
                // tick: its speed is not the speed of the runners active
                // now. Any growth waits for a tick that is, until its
                // deadline (design §14.33).
                self.unjudged = Some(growth);
                return 0;
            } else if speed < growth.speed_before * MIN_GROWTH_GAIN {
                self.hold_after(&growth, speed, now);
                return 0;
            } else {
                debug!(
                    "[STRATEGY] doubling to {} runners gained: {speed:.0} B/s after, {:.0} B/s \
                     before",
                    growth.runners, growth.speed_before
                );
                self.set_mode(Mode::Ramp);
                self.reprobe_wait = BASE_REPROBE_WAIT;
            }
        }
        if active >= context.max_concurrency {
            return 0;
        }
        let target = active.saturating_mul(2).min(context.max_concurrency);
        self.propose(context, speed, Ask::Ramp);
        target - active
    }

    /// The task's speed, if the tick may grow by it: every runner
    /// delivers, and the task is short of its speed limit.
    fn measured_speed(&self, context: &DoublingStrategyContext) -> Option<f64> {
        // Mend a stalled or connecting runner before adding more.
        if !context.all_delivering {
            return None;
        }
        let speed = context.speed?;
        if let Some(limit) = context.speed_limit
            && speed >= SPEED_LIMIT_SHARE * limit
        {
            return None;
        }
        Some(speed)
    }

    /// A doubling or a probe that gained too little, judged at `now`: hold
    /// the runners it doubled from. A probe doubles the wait.
    fn hold_after(&mut self, growth: &Growth, speed: f64, now: Instant) {
        if growth.probe_goal.is_some() {
            self.reprobe_wait = self.reprobe_wait.saturating_mul(2).min(MAX_REPROBE_WAIT);
        }
        let h = growth.runners_before;
        self.set_mode(Mode::Hold {
            h,
            reprobe_at: now + self.reprobe_wait,
        });
        debug!(
            "[STRATEGY] doubling to {} runners gained too little: {speed:.0} B/s after, {:.0} B/s \
             before; hold {h}, probe again in {:?}",
            growth.runners, growth.speed_before, self.reprobe_wait
        );
    }

    /// The runners a hold of `h` adds at `context.now`: splits up to `h`
    /// within the limit, for slots no unassigned range takes; at
    /// `reprobe_at`, with `b ≥ h` active, a probe from `b` towards
    /// `min(2b, limit)`, made only if it adds at least half of `b`
    /// (design §14.33, D31).
    ///
    /// The probe's speed before is the tick's, so it is made only at a
    /// tick that measured the `b` runners: stable, see
    /// [`TickSample::stable`], with none silent and the other gates of
    /// [`Self::measured_speed`] passed. A probe due without such a tick is
    /// sought for at most [`growth_deadline`] from the first due tick;
    /// then it is inconclusive, and the next is due a wait later.
    fn hold(&mut self, context: &DoublingStrategyContext, h: usize, reprobe_at: Instant) -> usize {
        let active = context.active_runners;
        let target = h.min(context.max_concurrency);
        if active < target {
            if context.eligible_ranges {
                return 0;
            }
            self.propose(context, context.speed.unwrap_or(0.0), Ask::Refill);
            return target - active;
        }
        let now = context.now;
        if active < h || now < reprobe_at {
            return 0;
        }
        // From the runners active now: those a gainless doubling added may
        // run to the end of the download (D31).
        let base = active;
        let goal = base.saturating_mul(2).min(context.max_concurrency);
        if goal.saturating_sub(base) < base.div_ceil(2) {
            return 0;
        }
        let measured = self
            .measured_speed(context)
            .filter(|_| context.stable && !context.silent_runners);
        let Some(speed) = measured else {
            // The search ends at its deadline, in `expire_baseline`.
            self.baseline_since.get_or_insert(now);
            return 0;
        };
        self.baseline_since = None;
        debug!("[STRATEGY] probing the hold of {h} runners from {base} towards {goal}");
        self.propose(context, speed, Ask::Probe { goal });
        goal - base
    }

    fn propose(&mut self, context: &DoublingStrategyContext, speed: f64, ask: Ask) {
        self.proposal = Some(Proposal {
            at: context.now,
            speed,
            active_runners: context.active_runners,
            max_concurrency: context.max_concurrency,
            ask,
        });
    }

    /// Hold `h` runners as a gainless doubling would, probed again at
    /// `reprobe_at`; drops a pending growth.
    #[cfg(test)]
    pub fn set_hold(&mut self, h: usize, reprobe_at: Instant) {
        self.unjudged = None;
        self.set_mode(Mode::Hold { h, reprobe_at });
    }

    /// When the hold is probed next; `None` while ramping.
    #[cfg(test)]
    pub fn reprobe_at(&self) -> Option<Instant> {
        match self.mode {
            Mode::Ramp => None,
            Mode::Hold { reprobe_at, .. } => Some(reprobe_at),
        }
    }

    /// The loop stopped a runner for being slow or silent, to restart its
    /// range (design §9.5), at `now`: a pending doubling, at the limit or
    /// below it, is dropped without a verdict, and a pending probe is
    /// inconclusive. The tick has a runner fewer, or one the loop gave up
    /// on, and a doubling held at the limit would keep the restarted range
    /// from being split (design §14.18, §14.19).
    pub fn on_restarted(&mut self, now: Instant) {
        if let Some(growth) = self.unjudged.take() {
            debug!(
                "[STRATEGY] a restart drops the doubling to {} runners unjudged",
                growth.runners
            );
            if growth.probe_goal.is_some() {
                self.inconclusive(now);
            }
        }
    }

    /// The loop added `added` runners for the last step's
    /// [`StrategyAction::Grow`]. A growth whose splits were all refused,
    /// for want of a slot or of a chunk large enough, is no growth.
    ///
    /// Only a full doubling, as many runners added as were active, is
    /// judged against [`MIN_GROWTH_GAIN`]. A growth cut short by the limit
    /// or by refused splits still waits out [`GROW_INTERVAL`], but a
    /// smaller step cannot be expected to gain 10%, so it is not judged.
    /// A probe is judged when it added at least half of the hold; with
    /// fewer it is inconclusive at once. A refill is never judged.
    pub fn on_grown(&mut self, added: usize) {
        let Some(proposal) = self.proposal.take() else {
            return;
        };
        let active = proposal.active_runners;
        let judged = match proposal.ask {
            Ask::Ramp => added >= active,
            Ask::Refill => false,
            Ask::Probe { .. } => added >= active.div_ceil(2),
        };
        if added > 0 {
            self.last_grow = Some(proposal.at);
        }
        if !judged {
            if let Ask::Probe { .. } = proposal.ask {
                debug!("[STRATEGY] the probe of {active} runners added {added}: inconclusive");
                self.inconclusive(proposal.at);
            }
            return;
        }
        let runners = active + added;
        self.unjudged = Some(Growth {
            started_at: proposal.at,
            speed_before: proposal.speed,
            runners_before: active,
            runners,
            at_limit: runners >= proposal.max_concurrency,
            probe_goal: match proposal.ask {
                Ask::Probe { goal } => Some(goal),
                Ask::Ramp | Ask::Refill => None,
            },
        });
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
            eligible_ranges: false,
            congested: false,
            all_delivering: true,
            silent_runners: false,
            stable: true,
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
    /// exactly that keeps growing; any less holds the two runners it
    /// doubled from, so nothing is added while four run, however fast the
    /// task gets (design §14.33).
    #[test]
    fn test_growth_holds_below_a_ten_percent_gain() {
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
        assert_eq!(strategy.want(8), 2);
        // Up to the probe, however fast the task gets.
        for n in 2..10 {
            let now = t0 + GROW_INTERVAL * n;
            assert_eq!(grow(&mut strategy, context(now, 4, 100.0 * MIB)), 0);
        }
    }

    /// A speed that drops after a growth holds too; the hold is probed
    /// again once it has its two runners and its wait has passed.
    #[test]
    fn test_no_growth_when_the_speed_drops() {
        let mut strategy = DoublingStrategy::default();
        let t0 = Instant::now();
        assert_eq!(grow(&mut strategy, context(t0, 2, 10.0 * MIB)), 2);
        let judged = t0 + GROW_INTERVAL;
        assert_eq!(grow(&mut strategy, context(judged, 4, 8.0 * MIB)), 0);
        assert_eq!(
            grow(
                &mut strategy,
                context(t0 + GROW_INTERVAL * 2, 4, 40.0 * MIB)
            ),
            0
        );
        let reprobe_at = judged + BASE_REPROBE_WAIT;
        assert_eq!(
            grow(&mut strategy, context(reprobe_at - NANO, 2, 8.0 * MIB)),
            0
        );
        assert_eq!(grow(&mut strategy, context(reprobe_at, 2, 8.0 * MIB)), 2);
    }

    /// A doubling to the limit that loses runners before a tick measures
    /// it waits for them: no verdict and no growth, so nothing refills; it
    /// is judged once a tick has its runners again (design §14.18). A
    /// gainless one holds the four: runners above them are not replaced.
    /// One whose runners never come back is dropped at its deadline,
    /// without a verdict, and the strategy grows on.
    #[test]
    fn test_a_doubling_to_the_limit_waits_for_its_runners() {
        let t0 = Instant::now();
        let at = |n: u32, runners: usize, speed: f64| DoublingStrategyContext {
            max_concurrency: 8,
            ..context(t0 + GROW_INTERVAL * n, runners, speed * MIB)
        };
        for (speed, refill) in [(40.0, 0), (80.0, 1)] {
            let mut strategy = DoublingStrategy::default();
            assert_eq!(grow(&mut strategy, at(0, 4, 40.0)), 4);
            // Two of the eight finished: not measured, not refilled.
            assert_eq!(grow(&mut strategy, at(1, 6, 30.0)), 0);
            assert_eq!(grow(&mut strategy, at(2, 7, 35.0)), 0);
            // Eight again, a retried range claimed: judged, at the limit.
            assert_eq!(grow(&mut strategy, at(3, 8, speed)), 0);
            // A hold of four refills nothing above it; a gain refills a
            // stopped runner.
            assert_eq!(
                grow(&mut strategy, at(4, 7, 100.0)),
                refill,
                "{speed} MiB/s"
            );
        }

        // Seven of the eight until the deadline.
        let mut strategy = DoublingStrategy::default();
        assert_eq!(grow(&mut strategy, at(0, 4, 40.0)), 4);
        let deadline = t0 + growth_deadline(crate::runner::DEFAULT_CONNECT_TIMEOUT);
        let seven = |now: Instant| DoublingStrategyContext {
            max_concurrency: 8,
            ..context(now, 7, 35.0 * MIB)
        };
        assert_eq!(grow(&mut strategy, seven(deadline - NANO)), 0);
        assert_eq!(grow(&mut strategy, seven(deadline)), 1);
    }

    /// A restart drops the doubling to the limit that waited for its
    /// runners, without a verdict, and the strategy grows on; one that
    /// lost a runner that finished waits on (design §14.18, §14.19). Below
    /// the limit, a restart drops the doubling too, even with as many
    /// runners as it made.
    #[test]
    fn test_a_restart_drops_the_doubling_it_waits_for() {
        let t0 = Instant::now();
        let at =
            |n: u32, runners: usize, speed: f64, max_concurrency: usize| DoublingStrategyContext {
                max_concurrency,
                ..context(t0 + GROW_INTERVAL * n, runners, speed * MIB)
            };
        let mut strategy = DoublingStrategy::default();
        assert_eq!(grow(&mut strategy, at(0, 4, 40.0, 8)), 4);
        // One of the eight finished: the doubling waits.
        assert_eq!(grow(&mut strategy, at(1, 7, 35.0, 8)), 0);
        // One is restarted: the doubling is dropped, and the slot refilled.
        strategy.on_restarted(t0 + GROW_INTERVAL);
        assert_eq!(grow(&mut strategy, at(2, 6, 30.0, 8)), 2);

        // Below the limit, with its eight, no faster: dropped, not judged.
        let mut strategy = DoublingStrategy::default();
        assert_eq!(grow(&mut strategy, at(0, 4, 40.0, 16)), 4);
        strategy.on_restarted(t0);
        assert_eq!(grow(&mut strategy, at(1, 8, 40.0, 16)), 8);
    }

    /// A doubling below the limit that loses runners before a tick
    /// measures it gets no verdict, and the strategy grows on (design
    /// §14.14, §14.18): runners that finished leave no range for a new
    /// runner to claim, so only a split brings their slots back.
    #[test]
    fn test_a_doubling_below_the_limit_that_loses_runners_is_not_judged() {
        let mut strategy = DoublingStrategy::default();
        let t0 = Instant::now();
        let at = |n: u32, runners: usize, speed: f64| DoublingStrategyContext {
            max_concurrency: 16,
            ..context(t0 + GROW_INTERVAL * n, runners, speed * MIB)
        };
        assert_eq!(grow(&mut strategy, at(0, 4, 40.0)), 4);
        // Two of the eight finished; the six are no faster than the four.
        assert_eq!(grow(&mut strategy, at(1, 6, 30.0)), 6);
        // The growth to twelve is judged: no gain, a hold of six.
        assert_eq!(grow(&mut strategy, at(2, 12, 30.0)), 0);
        assert_eq!(grow(&mut strategy, at(3, 10, 30.0)), 0, "the hold holds");
    }

    /// Whether a doubling reached the limit is the limit of the tick that
    /// decided it, not the limit when it is judged: the concurrency
    /// control may move it in between.
    #[test]
    fn test_the_limit_of_a_doubling_is_the_one_it_was_made_under() {
        let t0 = Instant::now();
        let at = |n: u32, runners: usize, limit: usize| DoublingStrategyContext {
            max_concurrency: limit,
            ..context(t0 + GROW_INTERVAL * n, runners, runners as f64 * MIB)
        };
        // Below a limit of 16, which then drops to 8: no hold, a refill.
        let mut strategy = DoublingStrategy::default();
        assert_eq!(grow(&mut strategy, at(0, 4, 16)), 4);
        assert_eq!(grow(&mut strategy, at(1, 7, 8)), 1);

        // Up to a limit of 8, which then rises to 16: held.
        let mut strategy = DoublingStrategy::default();
        assert_eq!(grow(&mut strategy, at(0, 4, 8)), 4);
        assert_eq!(grow(&mut strategy, at(1, 7, 16)), 0);
        assert_eq!(grow(&mut strategy, at(2, 7, 16)), 0);
        // Judged once eight deliver, and the risen limit lets it grow.
        assert_eq!(grow(&mut strategy, at(3, 8, 16)), 8);
    }

    /// A tick with a silent runner, let off by the sample, does not measure
    /// a doubling: its speed leaves the silent runner's share out. Below
    /// the limit the doubling is dropped without a verdict and the strategy
    /// grows on; one that reached the limit holds until a tick without one
    /// (design §14.19).
    #[test]
    fn test_a_tick_with_a_silent_runner_judges_nothing() {
        let t0 = Instant::now();
        let at = |n: u32, runners: usize, speed: f64, silent: bool| DoublingStrategyContext {
            max_concurrency: 8,
            silent_runners: silent,
            ..context(t0 + GROW_INTERVAL * n, runners, speed * MIB)
        };
        // Below the limit: no verdict, however slow, and the growth goes on.
        let mut strategy = DoublingStrategy::default();
        assert_eq!(
            grow(&mut strategy, at(0, 2, 20.0, true)),
            2,
            "the first growth"
        );
        assert_eq!(grow(&mut strategy, at(1, 4, 20.0, true)), 4, "no plateau");

        // To the limit: held through silent ticks, judged on a clean one.
        for (speed, refill) in [(40.0, 0), (80.0, 1)] {
            let mut strategy = DoublingStrategy::default();
            assert_eq!(grow(&mut strategy, at(0, 4, 40.0, false)), 4);
            assert_eq!(grow(&mut strategy, at(1, 8, 100.0, true)), 0, "held");
            assert_eq!(grow(&mut strategy, at(2, 7, 100.0, true)), 0, "held");
            assert_eq!(grow(&mut strategy, at(3, 8, speed, false)), 0, "judged");
            assert_eq!(
                grow(&mut strategy, at(4, 7, 100.0, false)),
                refill,
                "{speed}"
            );
        }
    }

    /// A loop that reports more runners than asked, past the limit of the
    /// tick, still records a doubling that reached that limit: it holds
    /// while runners are missing, then is judged.
    #[test]
    fn test_a_doubling_reported_past_the_limit_reached_it() {
        let mut strategy = DoublingStrategy::default();
        let t0 = Instant::now();
        let at = |n: u32, runners: usize, limit: usize| DoublingStrategyContext {
            max_concurrency: limit,
            ..context(t0 + GROW_INTERVAL * n, runners, runners as f64 * MIB)
        };
        assert_eq!(asked(&mut strategy, &mut at(0, 4, 6)), 2);
        strategy.on_grown(4);
        // Seven of the eight, under a limit since risen: held.
        assert_eq!(grow(&mut strategy, at(1, 7, 16)), 0);
        // Eight: judged, a gain, and the risen limit lets it grow.
        assert_eq!(grow(&mut strategy, at(2, 8, 16)), 8);
    }

    /// A doubling up to the limit is judged there. Without a gain it holds
    /// the four, also when the limit rises again, until the hold is probed;
    /// with one, the limit stops it, and a runner that finishes is
    /// replaced.
    #[test]
    fn test_a_doubling_to_the_limit_is_judged_there() {
        let t0 = Instant::now();
        let at = |n: u32, runners: usize, speed: f64, limit: usize| DoublingStrategyContext {
            max_concurrency: limit,
            ..context(t0 + GROW_INTERVAL * n, runners, speed * MIB)
        };
        // No faster with eight: a hold of four, even once the limit rises.
        let mut strategy = DoublingStrategy::default();
        assert_eq!(grow(&mut strategy, at(0, 4, 40.0, 8)), 4);
        assert_eq!(grow(&mut strategy, at(1, 8, 40.0, 8)), 0);
        assert_eq!(grow(&mut strategy, at(2, 8, 80.0, 16)), 0);
        assert_eq!(grow(&mut strategy, at(3, 7, 70.0, 8)), 0, "no refill");
        assert_eq!(strategy.want(16), 4);
        // Four again, at the probe: up to the risen limit's eight.
        let reprobe_at = t0 + GROW_INTERVAL + BASE_REPROBE_WAIT;
        let mut four = DoublingStrategyContext {
            max_concurrency: 16,
            ..context(reprobe_at, 4, 40.0 * MIB)
        };
        assert_eq!(asked(&mut strategy, &mut four), 4);

        // Faster with eight: judged, held by the limit, then refilled.
        let mut strategy = DoublingStrategy::default();
        assert_eq!(grow(&mut strategy, at(0, 4, 40.0, 8)), 4);
        assert_eq!(grow(&mut strategy, at(1, 8, 80.0, 8)), 0);
        assert_eq!(grow(&mut strategy, at(2, 7, 70.0, 8)), 1);

        // Past a lowered limit, the doubling is judged all the same.
        let mut strategy = DoublingStrategy::default();
        assert_eq!(grow(&mut strategy, at(0, 4, 40.0, 8)), 4);
        assert_eq!(grow(&mut strategy, at(1, 8, 40.0, 6)), 0);
        assert_eq!(grow(&mut strategy, at(2, 8, 80.0, 16)), 0, "a hold");
    }

    /// A tick at `now` where every one of `runners` delivers, the task at
    /// `speed` MiB/s, the limit controller allowing `limit`.
    fn tick(now: Instant, runners: usize, speed: f64, limit: usize) -> DoublingStrategyContext {
        DoublingStrategyContext {
            max_concurrency: limit,
            ..context(now, runners, speed * MIB)
        }
    }

    /// A strategy that doubled `h` runners at `t0`, under a limit of
    /// `limit`, and gained nothing: a hold of `h`. Returns it and when the
    /// hold is probed first.
    fn held(t0: Instant, h: usize, limit: usize) -> (DoublingStrategy, Instant) {
        let mut strategy = DoublingStrategy::default();
        assert_eq!(grow(&mut strategy, tick(t0, h, 40.0, limit)), h);
        let judged = t0 + GROW_INTERVAL;
        assert_eq!(grow(&mut strategy, tick(judged, 2 * h, 40.0, limit)), 0);
        assert_eq!(strategy.want(limit), h);
        let reprobe_at = judged + BASE_REPROBE_WAIT;
        assert_eq!(strategy.reprobe_at(), Some(reprobe_at));
        (strategy, reprobe_at)
    }

    /// The hold of four of [`held`] probed at `at` under a limit of 16, and
    /// the probe's four runners added.
    fn probe(strategy: &mut DoublingStrategy, at: Instant) {
        assert_eq!(grow(strategy, tick(at, 4, 40.0, 16)), 4);
        assert_eq!(strategy.want(16), 8);
    }

    /// A gainless doubling from four holds four; the four it added finish
    /// their chunks and are not replaced, and the hold is probed from its
    /// four at its time, not a nanosecond before (design §14.33).
    #[test]
    fn test_a_plateau_is_probed_again() {
        let t0 = Instant::now();
        let (mut strategy, reprobe_at) = held(t0, 4, 16);
        let mut now = t0 + GROW_INTERVAL;
        for runners in [8, 7, 6, 5, 4] {
            now += TICK;
            assert_eq!(
                grow(&mut strategy, tick(now, runners, 40.0, 16)),
                0,
                "{runners} runners"
            );
        }
        assert_eq!(grow(&mut strategy, tick(reprobe_at - NANO, 4, 40.0, 16)), 0);
        probe(&mut strategy, reprobe_at);
    }

    /// A hold is probed from the runners active at its time when more
    /// than it holds still run (D31): from eight, towards 16, with a hold
    /// of four. A gain resumes the ramp; no gain holds the eight. Under a
    /// limit, the probe must still add half of the runners it starts
    /// from: from six or eight under eight it is not made, from five it
    /// is.
    #[test]
    fn test_a_hold_is_probed_from_the_runners_active_then() {
        let t0 = Instant::now();
        for (speed, want) in [(80.0, 16), (40.0, 8)] {
            let (mut strategy, reprobe_at) = held(t0, 4, 16);
            assert_eq!(grow(&mut strategy, tick(reprobe_at - NANO, 8, 40.0, 16)), 0);
            assert_eq!(grow(&mut strategy, tick(reprobe_at, 8, 40.0, 16)), 8);
            assert_eq!(strategy.want(16), 16);
            let judged = reprobe_at + GROW_INTERVAL;
            assert_eq!(grow(&mut strategy, tick(judged, 16, speed, 16)), 0);
            assert_eq!(strategy.want(16), want, "{speed} MiB/s");
            if want == 8 {
                assert_eq!(strategy.reprobe_at(), Some(judged + 2 * BASE_REPROBE_WAIT));
            }
        }
        for (runners, asked_for) in [(8, 0), (6, 0), (5, 3)] {
            let (mut strategy, reprobe_at) = held(t0, 4, 16);
            assert_eq!(
                grow(&mut strategy, tick(reprobe_at, runners, 40.0, 8)),
                asked_for,
                "{runners} runners"
            );
        }
    }

    /// A tick whose runners changed in it, see [`TickSample::stable`],
    /// neither judges a pending growth nor starts a probe: both wait for
    /// the next tick that measured the runners active then (design
    /// §14.33).
    #[test]
    fn test_an_unstable_tick_judges_and_probes_nothing() {
        let t0 = Instant::now();
        let unstable = |context: DoublingStrategyContext| DoublingStrategyContext {
            stable: false,
            ..context
        };
        // A doubling to the limit: no verdict, no refill, until a stable
        // tick finds no gain.
        let mut strategy = DoublingStrategy::default();
        assert_eq!(grow(&mut strategy, tick(t0, 4, 40.0, 8)), 4);
        let judged = t0 + GROW_INTERVAL;
        assert_eq!(grow(&mut strategy, unstable(tick(judged, 8, 40.0, 8))), 0);
        assert_eq!(strategy.reprobe_at(), None);
        assert_eq!(strategy.want(8), 8);
        assert_eq!(grow(&mut strategy, tick(judged + TICK, 8, 40.0, 8)), 0);
        assert_eq!(strategy.want(8), 4);
        assert_eq!(
            strategy.reprobe_at(),
            Some(judged + TICK + BASE_REPROBE_WAIT)
        );

        // A probe of a hold: not at an unstable tick, at the next one.
        let (mut strategy, reprobe_at) = held(t0, 4, 16);
        assert_eq!(
            grow(&mut strategy, unstable(tick(reprobe_at, 4, 40.0, 16))),
            0
        );
        assert_eq!(strategy.want(16), 4);
        assert_eq!(grow(&mut strategy, tick(reprobe_at + TICK, 4, 40.0, 16)), 4);
        // Its verdict waits for a stable tick too.
        let judged = reprobe_at + TICK + GROW_INTERVAL;
        assert_eq!(grow(&mut strategy, unstable(tick(judged, 8, 80.0, 16))), 0);
        assert_eq!(strategy.want(16), 8);
        assert_eq!(grow(&mut strategy, tick(judged + TICK, 8, 80.0, 16)), 8);
        assert_eq!(strategy.want(16), 16);
    }

    /// Each probe without a gain doubles the wait to the next, up to
    /// 300 s: 30 s after the doubling that began the hold, then 60, 120,
    /// 240, 300 and 300 s after each gainless probe's verdict.
    #[test]
    fn test_a_probe_without_gain_backs_off() {
        let t0 = Instant::now();
        let (mut strategy, mut reprobe_at) = held(t0, 4, 16);
        let mut judged = t0 + GROW_INTERVAL;
        let mut waits = Vec::new();
        for _ in 0..6 {
            waits.push((reprobe_at - judged).as_secs());
            assert_eq!(grow(&mut strategy, tick(reprobe_at - NANO, 4, 40.0, 16)), 0);
            probe(&mut strategy, reprobe_at);
            // Judged at the first tick 2 s later, with its eight: no faster.
            judged = reprobe_at + GROW_INTERVAL;
            assert_eq!(grow(&mut strategy, tick(judged, 8, 40.0, 16)), 0);
            assert_eq!(strategy.want(16), 4);
            reprobe_at = strategy.reprobe_at().unwrap();
        }
        assert_eq!(waits, [30, 60, 120, 240, 300, 300]);
        assert_eq!(reprobe_at - judged, MAX_REPROBE_WAIT);
    }

    /// A probe that gains resumes the ramp, which doubles on to the limit;
    /// the hold its next gainless doubling begins waits the base time
    /// again.
    #[test]
    fn test_a_gain_resumes_the_ramp() {
        let t0 = Instant::now();
        let (mut strategy, reprobe_at) = held(t0, 4, 16);
        // A probe without a gain: the wait doubles.
        probe(&mut strategy, reprobe_at);
        let judged = reprobe_at + GROW_INTERVAL;
        assert_eq!(grow(&mut strategy, tick(judged, 8, 40.0, 16)), 0);
        let next = strategy.reprobe_at().unwrap();
        assert_eq!(next - judged, 2 * BASE_REPROBE_WAIT);

        // The next one gains: the ramp doubles the eight at once.
        probe(&mut strategy, next);
        let judged = next + GROW_INTERVAL;
        assert_eq!(grow(&mut strategy, tick(judged, 8, 80.0, 16)), 8);
        assert_eq!(strategy.want(16), 16);
        assert_eq!(strategy.reprobe_at(), None);
        // No faster with sixteen: a hold of eight, at the base wait.
        let judged = judged + GROW_INTERVAL;
        assert_eq!(grow(&mut strategy, tick(judged, 16, 80.0, 16)), 0);
        assert_eq!(strategy.want(16), 8);
        assert_eq!(strategy.reprobe_at(), Some(judged + BASE_REPROBE_WAIT));
    }

    /// A probe ends inconclusive, and the hold keeps its wait, counted from
    /// then: when its splits are all refused or add fewer than half of the
    /// hold, at once; when its runners do not come back, at its deadline;
    /// when the loop restarts a runner.
    #[test]
    fn test_an_inconclusive_probe_keeps_its_wait() {
        let t0 = Instant::now();
        // A hold of four whose last probe gained nothing: a wait of 60 s.
        let after_a_gainless_probe = || {
            let (mut strategy, reprobe_at) = held(t0, 4, 16);
            probe(&mut strategy, reprobe_at);
            let judged = reprobe_at + GROW_INTERVAL;
            assert_eq!(grow(&mut strategy, tick(judged, 8, 40.0, 16)), 0);
            let next = strategy.reprobe_at().unwrap();
            (strategy, next)
        };
        let wait = 2 * BASE_REPROBE_WAIT;

        // Splits refused, or one added: fewer than half of the four.
        for added in [0, 1] {
            let (mut strategy, at) = after_a_gainless_probe();
            assert_eq!(asked(&mut strategy, &mut tick(at, 4, 40.0, 16)), 4);
            strategy.on_grown(added);
            assert_eq!(strategy.reprobe_at(), Some(at + wait), "{added} added");
            assert_eq!(strategy.want(16), 4);
        }
        // Two added: half of the four, judged.
        let (mut strategy, at) = after_a_gainless_probe();
        assert_eq!(asked(&mut strategy, &mut tick(at, 4, 40.0, 16)), 4);
        strategy.on_grown(2);
        assert_eq!(strategy.want(16), 8);
        assert_eq!(
            grow(&mut strategy, tick(at + GROW_INTERVAL, 6, 80.0, 16)),
            6
        );

        // Seven of the eight until the deadline.
        let (mut strategy, at) = after_a_gainless_probe();
        probe(&mut strategy, at);
        let deadline = at + growth_deadline(crate::runner::DEFAULT_CONNECT_TIMEOUT);
        assert_eq!(grow(&mut strategy, tick(deadline - NANO, 7, 80.0, 16)), 0);
        assert_eq!(strategy.want(16), 8);
        assert_eq!(grow(&mut strategy, tick(deadline, 7, 80.0, 16)), 0);
        assert_eq!(strategy.want(16), 4);
        assert_eq!(strategy.reprobe_at(), Some(deadline + wait));

        // A restart.
        let (mut strategy, at) = after_a_gainless_probe();
        probe(&mut strategy, at);
        let restarted = at + TICK;
        strategy.on_restarted(restarted);
        assert_eq!(strategy.want(16), 4);
        assert_eq!(strategy.reprobe_at(), Some(restarted + wait));
    }

    /// A hold splits up to its runners for slots no unassigned range takes,
    /// whether or not every runner delivers; such a refill is never judged.
    /// It adds nothing while the congestion holds, nor at or above its
    /// runners.
    #[test]
    fn test_a_hold_refills_up_to_its_target_and_adds_nothing_above_it() {
        let t0 = Instant::now();
        let (mut strategy, reprobe_at) = held(t0, 4, 16);
        let now = t0 + GROW_INTERVAL * 3;
        let mut three = tick(now, 3, 40.0, 16);
        three.eligible_ranges = true;
        assert_eq!(asked(&mut strategy, &mut three), 0, "a range to claim");
        three.eligible_ranges = false;
        three.congested = true;
        assert_eq!(asked(&mut strategy, &mut three), 0, "congested");
        three.congested = false;
        three.all_delivering = false;
        assert_eq!(asked(&mut strategy, &mut three), 1);
        strategy.on_grown(1);
        // The interval runs from the refill.
        assert_eq!(
            grow(&mut strategy, tick(now + GROW_INTERVAL - NANO, 2, 40.0, 16)),
            0
        );
        assert_eq!(
            grow(&mut strategy, tick(now + GROW_INTERVAL, 2, 40.0, 16)),
            2
        );
        // Never judged, however slow; nothing at or above the four.
        for (n, runners) in (2..).zip([4, 5, 8, 4]) {
            assert_eq!(
                grow(
                    &mut strategy,
                    tick(now + GROW_INTERVAL * n, runners, 1.0, 16)
                ),
                0,
                "{runners} runners"
            );
        }
        assert_eq!(strategy.reprobe_at(), Some(reprobe_at));
        assert_eq!(strategy.want(16), 4);
    }

    /// A hold of four under a limit of six probes up to the six: two more
    /// is half of the four, so the probe is judged. A gain ramps at the
    /// limit; no gain holds four.
    #[test]
    fn test_a_hold_below_the_limit_probes_up_to_it() {
        let t0 = Instant::now();
        for (speed, want) in [(45.0, 16), (43.0, 4)] {
            let (mut strategy, reprobe_at) = held(t0, 4, 16);
            assert_eq!(grow(&mut strategy, tick(reprobe_at, 4, 40.0, 6)), 2);
            assert_eq!(strategy.want(16), 6);
            let judged = reprobe_at + GROW_INTERVAL;
            assert_eq!(grow(&mut strategy, tick(judged, 6, speed, 6)), 0);
            assert_eq!(strategy.want(16), want, "{speed} MiB/s");
        }
    }

    /// A hold of eight under a limit of six wants its eight, which asks the
    /// limit controller for steps (design §8.3). It adds nothing at the
    /// limit; a step's trial slot it refills, and no probe comes while
    /// fewer than eight run.
    #[test]
    fn test_a_hold_above_the_limit_asks_for_steps() {
        let t0 = Instant::now();
        let (mut strategy, reprobe_at) = held(t0, 8, 16);
        assert_eq!(strategy.want(16), 8);
        let now = t0 + GROW_INTERVAL * 3;
        assert_eq!(grow(&mut strategy, tick(now, 6, 40.0, 6)), 0, "the limit");
        assert_eq!(
            grow(&mut strategy, tick(now + TICK, 6, 40.0, 7)),
            1,
            "a step"
        );
        assert_eq!(grow(&mut strategy, tick(reprobe_at, 7, 40.0, 7)), 0);
        assert_eq!(strategy.want(16), 8);
    }

    /// A hold of eight whose limit was lowered to four under six running
    /// runners adds nothing, at its probe time too.
    #[test]
    fn test_a_lowered_limit_under_running_runners_adds_nothing() {
        let t0 = Instant::now();
        let (mut strategy, reprobe_at) = held(t0, 8, 16);
        for now in [t0 + GROW_INTERVAL * 3, reprobe_at] {
            assert_eq!(grow(&mut strategy, tick(now, 6, 40.0, 4)), 0);
        }
        assert_eq!(strategy.want(16), 8);
    }

    /// A hold is probed only towards at least half of it more: a hold of
    /// eight under a limit of 9 or 11 is not probed, under 12 it is.
    #[test]
    fn test_a_hold_too_close_to_the_limit_is_not_probed() {
        let t0 = Instant::now();
        for (limit, asked_for) in [(9, 0), (11, 0), (12, 4)] {
            let (mut strategy, reprobe_at) = held(t0, 8, 16);
            assert_eq!(
                grow(&mut strategy, tick(reprobe_at, 8, 40.0, limit)),
                asked_for,
                "limit {limit}"
            );
        }
    }

    /// A growth whose runners cannot all run, one of them failed for good
    /// and the splits to replace it refused, ends at its deadline: a
    /// doubling to the limit is dropped and the strategy asks again; a
    /// probe is inconclusive and adds nothing above the hold.
    #[test]
    fn test_a_growth_that_cannot_fill_ends_at_its_deadline() {
        let t0 = Instant::now();
        let deadline = growth_deadline(crate::runner::DEFAULT_CONNECT_TIMEOUT);

        let mut strategy = DoublingStrategy::default();
        assert_eq!(grow(&mut strategy, tick(t0, 4, 40.0, 8)), 4);
        let mut now = t0 + TICK;
        while now < t0 + deadline {
            assert_eq!(grow(&mut strategy, tick(now, 7, 70.0, 8)), 0);
            now += TICK;
        }
        assert_eq!(asked(&mut strategy, &mut tick(now, 7, 70.0, 8)), 1);
        strategy.on_grown(0);
        assert_eq!(asked(&mut strategy, &mut tick(now + TICK, 7, 70.0, 8)), 1);

        let (mut strategy, reprobe_at) = held(t0, 4, 16);
        probe(&mut strategy, reprobe_at);
        let mut now = reprobe_at + TICK;
        while now < reprobe_at + deadline {
            assert_eq!(grow(&mut strategy, tick(now, 7, 70.0, 16)), 0);
            now += TICK;
        }
        assert_eq!(grow(&mut strategy, tick(now, 7, 70.0, 16)), 0);
        assert_eq!(strategy.want(16), 4);
        assert_eq!(strategy.reprobe_at(), Some(now + BASE_REPROBE_WAIT));
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

        // A full doubling is judged still: no faster, a hold of sixteen.
        assert_eq!(grow(&mut strategy, at(3, 16, 32)), 16);
        let no_faster = DoublingStrategyContext {
            speed: Some(16.0 * MIB),
            ..at(4, 32, 64)
        };
        assert_eq!(grow(&mut strategy, no_faster), 0);
        assert_eq!(grow(&mut strategy, at(5, 32, 64)), 0);
    }

    /// The accepted blind spot (design §14.14, §14.18): a growth cut short
    /// by the limit is not judged, so at the limit a runner that stops and
    /// the one split to refill it, 6, 5, 6 again and again, find no plateau
    /// even when the task is no faster.
    #[test]
    fn test_refills_after_a_truncated_growth_are_never_judged() {
        let mut strategy = DoublingStrategy::default();
        let t0 = Instant::now();
        let at = |now: Instant, runners: usize| DoublingStrategyContext {
            max_concurrency: 6,
            ..context(now, runners, 40.0 * MIB)
        };
        // Four to six: cut short by the limit.
        assert_eq!(grow(&mut strategy, at(t0, 4)), 2);
        for cycle in 1..10 {
            let now = t0 + GROW_INTERVAL * (2 * cycle);
            assert_eq!(grow(&mut strategy, at(now - TICK, 6)), 0, "at the limit");
            assert_eq!(grow(&mut strategy, at(now, 5)), 1, "refill {cycle}");
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
