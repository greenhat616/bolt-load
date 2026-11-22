//! What ended attempts tell about the connections the server admits
//! (design §8.3).
//!
//! Only attempts that failed before `Started` say anything about the
//! connection limit: a refused connect, or a connect that timed out. A
//! congestion (429/503) counts in any phase. Everything after `Started` is
//! left out: a timeout there may be the loop's drain timeout, and a reset
//! cannot be told from bytes still queued.
//!
//! The evidence is a fixed set of values, not a log, so nothing overflows
//! or is evicted: besides a few counts and instants, at most one value per
//! runner ID. The manager records events, but has no clock: an event's
//! instant is marked pending until the loop [stamps](AdmissionEvidence::stamp)
//! it with its `now`. A pending instant reads as unknown, and an unknown
//! value leads to no action.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use bolt_load_core::adapter::{AdapterError, RetryableError};

use super::failure_policy::congestion_retry_after;
use crate::{runner::TaskError, runtime::capped_wait, task::RunnerId};

/// A streak of refusals ends when no refusal arrives for this long, given
/// that a connect may take `connect_timeout`: long enough for three connect
/// timeouts in a row. The connect timeout counts as the runners wait for
/// it, capped at 30 years.
pub fn streak_gap(connect_timeout: Duration) -> Duration {
    capped_wait(connect_timeout)
        .saturating_mul(2)
        .saturating_add(Duration::from_secs(30))
}

/// A runner ID no attempt holds: an attempt never seen connecting.
#[cfg(test)]
pub const UNSEEN: RunnerId = RunnerId::MAX;

/// Where an attempt was when it ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptPhase {
    /// Before its runner reported `Started`.
    Connecting,
    /// After `Started`.
    Streaming,
}

/// What an ended attempt says about the server's admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionClass {
    /// The server refused the connection, or did not answer it in time.
    Refused,
    /// The server rejected the connection count (429/503).
    Congestion,
    /// Nothing about the connection limit.
    NoEvidence,
}

/// Classify an attempt that ended in `phase` with `error`.
///
/// Before `Started` a [`TaskError::Timeout`] can only be the connect
/// timeout: no frame exists yet, so the drain cannot time out. A refusal
/// cannot be told from a DNS or TLS failure, which the adapters report the
/// same way; the corroboration of [`AdmissionEvidence::on_attempt_ended`]
/// covers most of it.
pub fn classify(phase: AttemptPhase, error: &TaskError) -> AdmissionClass {
    if congestion_retry_after(error).is_some() {
        return AdmissionClass::Congestion;
    }
    match (phase, error) {
        (AttemptPhase::Connecting, TaskError::Timeout)
        | (
            AttemptPhase::Connecting,
            TaskError::StreamError {
                source:
                    AdapterError::Retryable {
                        source: RetryableError::Io { .. },
                    },
            },
        ) => AdmissionClass::Refused,
        _ => AdmissionClass::NoEvidence,
    }
}

/// An instant the manager marked, which the loop has stamped or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stamp {
    Pending,
    At(Instant),
}

impl Stamp {
    fn known(self) -> Option<Instant> {
        match self {
            Self::Pending => None,
            Self::At(at) => Some(at),
        }
    }

    fn stamp(&mut self, now: Instant) {
        if *self == Self::Pending {
            *self = Self::At(now);
        }
    }
}

/// Corroborated refusals in a row, as of the latest stamp.
#[derive(Clone, Copy, Debug)]
struct Streak {
    count: usize,
    /// The least other attempts in flight at any of them.
    level: usize,
    /// The latest refusal.
    last: Instant,
    /// The third refusal, once there was one.
    third: Option<Instant>,
}

/// Corroborated refusals classified since the latest stamp, which joins
/// them to the streak once their time is known.
#[derive(Clone, Copy, Debug)]
struct Unstamped {
    count: usize,
    level: usize,
}

/// The marks of a running step, which follow program order: only what is
/// classified after [`AdmissionEvidence::begin_step`] sets one.
#[derive(Clone, Copy, Debug)]
struct StepMarks {
    /// The connected runners the step tries: one above the limit.
    trial: usize,
    /// A corroborated refusal or a congestion.
    rejected: bool,
    /// An uncorroborated refusal.
    unclear: bool,
}

/// What ended attempts tell about the connections the server admits.
#[derive(Debug)]
pub struct AdmissionEvidence {
    /// The runners between the handling of their `Started` and their
    /// release.
    connected: HashSet<RunnerId>,
    streak: Option<Streak>,
    unstamped: Option<Unstamped>,
    /// See [`streak_gap`].
    streak_gap: Duration,
    /// A congestion the limit controller has not taken yet.
    congestion_pending: bool,
    step: Option<StepMarks>,
    /// The most attempts proven at once, as seen at strategy ticks and at
    /// failures, since the limit was last lowered.
    proven_window: usize,
    /// Since when `connected` has reached the running step's trial.
    occupied_since: Option<Stamp>,
    /// For each attempt still connecting that was seen beside a proven
    /// one: the most other attempts in flight at such a moment. Removed
    /// at its `Started` or its classification, and forgotten when it ends
    /// unclassified or its runner ID is reserved again.
    overlaps: HashMap<RunnerId, usize>,
}

impl Default for AdmissionEvidence {
    fn default() -> Self {
        Self::with_streak_gap(streak_gap(crate::runner::DEFAULT_CONNECT_TIMEOUT))
    }
}

impl AdmissionEvidence {
    /// Evidence whose streaks of refusals end after `streak_gap` without
    /// one.
    pub fn with_streak_gap(streak_gap: Duration) -> Self {
        Self {
            connected: HashSet::new(),
            streak: None,
            unstamped: None,
            streak_gap,
            congestion_pending: false,
            step: None,
            proven_window: 0,
            occupied_since: None,
            overlaps: HashMap::new(),
        }
    }

    /// The least other attempts in flight at any refusal of the streak,
    /// stamped or not.
    fn streak_level_any(&self) -> Option<usize> {
        let stamped = self.streak.map(|streak| streak.level);
        let unstamped = self.unstamped.map(|unstamped| unstamped.level);
        stamped.into_iter().chain(unstamped).min()
    }

    /// Runner `runner_id` reported `Started`, with `others` other attempts
    /// in flight.
    ///
    /// The server admitted a connection at that level, which ends a streak
    /// of refusals at or below it.
    pub fn on_started(&mut self, runner_id: RunnerId, others: usize) {
        self.connected.insert(runner_id);
        self.overlaps.remove(&runner_id);
        if self.streak_level_any().is_some_and(|level| others >= level) {
            self.reset_streak();
        }
        if let Some(step) = self.step
            && self.connected.len() >= step.trial
            && self.occupied_since.is_none()
        {
            self.occupied_since = Some(Stamp::Pending);
        }
    }

    /// Runner `runner_id` was released, whatever ended it.
    ///
    /// A release that drops the connected runners below the trial breaks
    /// the occupancy, even one not stamped yet.
    pub fn on_ended(&mut self, runner_id: RunnerId) {
        if !self.connected.remove(&runner_id) {
            return;
        }
        if self
            .step
            .is_some_and(|step| self.connected.len() < step.trial)
        {
            self.occupied_since = None;
        }
    }

    /// Forget what the attempt of `runner_id` was seen beside: it ended
    /// without a classification (cancelled, or beaten in its pair), or
    /// its ID was reserved for a new attempt.
    pub fn forget(&mut self, runner_id: RunnerId) {
        self.overlaps.remove(&runner_id);
    }

    /// Whether the attempt of `runner_id` was seen connecting beside a
    /// proven one.
    #[cfg(test)]
    pub fn is_seen(&self, runner_id: RunnerId) -> bool {
        self.overlaps.contains_key(&runner_id)
    }

    /// The attempts of `connecting` are connecting while `proven` attempts
    /// are proven, each with `others` other attempts in flight.
    ///
    /// Beside a proven attempt, each keeps the most `others` seen, which
    /// [corroborates](Self::on_attempt_ended) its refusal even once no
    /// attempt is proven any more.
    pub fn observe_connecting(
        &mut self,
        connecting: impl IntoIterator<Item = RunnerId>,
        proven: usize,
        others: usize,
    ) {
        if proven == 0 {
            return;
        }
        for runner_id in connecting {
            let level = self.overlaps.entry(runner_id).or_insert(others);
            *level = (*level).max(others);
        }
    }

    /// The attempt of runner `runner_id` ended as `class`, with `proven`
    /// other attempts proven and `others` other attempts in flight.
    ///
    /// A refusal is corroborated when another attempt is proven now, or
    /// was [seen](Self::observe_connecting) proven while it connected: a
    /// connect that hangs until its timeout may outlast every proven
    /// attempt. Its level is the most other attempts in flight at any of
    /// those moments, so the reconnects of the ranges it held, at a lower
    /// level, do not end the streak. An uncorroborated refusal only makes a
    /// running step unclear.
    pub fn on_attempt_ended(
        &mut self,
        runner_id: RunnerId,
        class: AdmissionClass,
        proven: usize,
        others: usize,
    ) {
        self.observe_proven(proven);
        let overlap = self.overlaps.remove(&runner_id);
        let beside_now = (proven > 0).then_some(others);
        let level = beside_now.into_iter().chain(overlap).max();
        match (class, level) {
            (AdmissionClass::Refused, Some(level)) => {
                let unstamped = self.unstamped.get_or_insert(Unstamped { count: 0, level });
                unstamped.count += 1;
                unstamped.level = unstamped.level.min(level);
                if let Some(step) = &mut self.step {
                    step.rejected = true;
                }
            }
            (AdmissionClass::Refused, None) => {
                if let Some(step) = &mut self.step {
                    step.unclear = true;
                }
            }
            (AdmissionClass::Congestion, _) => {
                self.congestion_pending = true;
                if let Some(step) = &mut self.step {
                    step.rejected = true;
                }
            }
            (AdmissionClass::NoEvidence, _) => {}
        }
    }

    /// Stamp every pending instant with `now`.
    ///
    /// A streak whose latest refusal is `streak_gap` or more before `now`
    /// has ended: refusals stamped now start a new one instead of
    /// extending it.
    pub fn stamp(&mut self, now: Instant) {
        if self
            .streak
            .is_some_and(|streak| now.saturating_duration_since(streak.last) >= self.streak_gap)
        {
            self.streak = None;
        }
        if let Some(unstamped) = self.unstamped.take() {
            let streak = self.streak.get_or_insert(Streak {
                count: 0,
                level: unstamped.level,
                last: now,
                third: None,
            });
            let before = streak.count;
            streak.count += unstamped.count;
            streak.level = streak.level.min(unstamped.level);
            streak.last = now;
            if before < 3 && streak.count >= 3 {
                streak.third = Some(now);
            }
        }
        if let Some(since) = &mut self.occupied_since {
            since.stamp(now);
        }
    }

    /// `proven` attempts are proven now.
    pub fn observe_proven(&mut self, proven: usize) {
        self.proven_window = self.proven_window.max(proven);
    }

    /// The limit was lowered to `limit`.
    pub fn on_lowered(&mut self, limit: usize) {
        self.proven_window = self.proven_window.min(limit);
        self.reset_streak();
    }

    /// Forget the streak of refusals.
    pub fn reset_streak(&mut self) {
        self.streak = None;
        self.unstamped = None;
    }

    /// Take the congestion classified since the last call.
    pub fn take_congestion(&mut self) -> bool {
        std::mem::take(&mut self.congestion_pending)
    }

    /// Whether a congestion waits for the limit controller.
    pub fn congestion_pending(&self) -> bool {
        self.congestion_pending
    }

    /// A step that tries `trial` connected runners begins at `now`.
    ///
    /// Its occupancy starts now if `trial` runners are already connected.
    pub fn begin_step(&mut self, trial: usize, now: Instant) {
        self.step = Some(StepMarks {
            trial,
            rejected: false,
            unclear: false,
        });
        self.occupied_since = (self.connected.len() >= trial).then_some(Stamp::At(now));
        self.reset_streak();
    }

    /// The running step ended.
    pub fn end_step(&mut self) {
        self.step = None;
        self.occupied_since = None;
        self.reset_streak();
    }

    /// The runners connected now.
    pub fn connected(&self) -> usize {
        self.connected.len()
    }

    /// Whether runner `runner_id` is connected.
    #[cfg(test)]
    pub fn is_connected(&self, runner_id: RunnerId) -> bool {
        self.connected.contains(&runner_id)
    }

    /// The corroborated refusals in a row, stamped or not.
    ///
    /// Refusals not stamped yet may start a new streak at their stamp, so
    /// the count may fall then; the third refusal's time is unknown until
    /// it is stamped.
    pub fn streak_count(&self) -> usize {
        self.streak.map_or(0, |streak| streak.count)
            + self.unstamped.map_or(0, |unstamped| unstamped.count)
    }

    /// The least other attempts in flight at any refusal of the streak.
    #[cfg(test)]
    pub fn streak_level(&self) -> Option<usize> {
        self.streak_level_any()
    }

    /// When the streak's third refusal was classified; `None` before it,
    /// or while unknown.
    pub fn third_refusal_at(&self) -> Option<Instant> {
        self.streak?.third
    }

    /// When the streak's latest refusal was classified; `None` without a
    /// streak, or while unknown.
    #[cfg(test)]
    pub fn last_refusal_at(&self) -> Option<Instant> {
        if self.unstamped.is_some() {
            return None;
        }
        Some(self.streak?.last)
    }

    /// Whether a corroborated refusal or a congestion was classified since
    /// the running step began.
    pub fn step_rejected(&self) -> bool {
        self.step.is_some_and(|step| step.rejected)
    }

    /// Whether an uncorroborated refusal was classified since the running
    /// step began.
    pub fn step_unclear(&self) -> bool {
        self.step.is_some_and(|step| step.unclear)
    }

    /// Since when the running step's trial has been connected without a
    /// break; `None` without one, or while unknown.
    pub fn occupied_since(&self) -> Option<Instant> {
        self.occupied_since?.known()
    }

    /// The most attempts proven at once since the limit was last lowered.
    pub fn proven_window(&self) -> usize {
        self.proven_window
    }

    /// Whether an attempt that ended as `class`, with `proven` other
    /// attempts proven and `others` other attempts in flight, is exempt
    /// from its range's budget (design §14.13a): a refusal beside a proven
    /// attempt, with at least `max(proven_window, 1)` others in flight, is
    /// above what the server was seen to admit.
    ///
    /// Decided by the state of this moment alone: a refusal corroborated
    /// only by the proven attempts its connect was seen beside is charged.
    ///
    /// Read after [`Self::on_attempt_ended`] recorded the same attempt.
    pub fn exempts(&self, class: AdmissionClass, proven: usize, others: usize) -> bool {
        class == AdmissionClass::Refused && proven > 0 && others >= self.proven_window().max(1)
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use super::*;
    use crate::adapter::UnretryableError;

    const SECOND: Duration = Duration::from_secs(1);

    fn io() -> TaskError {
        TaskError::StreamError {
            source: AdapterError::Retryable {
                source: RetryableError::Io {
                    source: Arc::new(std::io::Error::other("refused")),
                },
            },
        }
    }

    fn congestion() -> TaskError {
        TaskError::StreamError {
            source: AdapterError::Unretryable {
                source: UnretryableError::Congestion {
                    status: 429,
                    retry_after: None,
                },
            },
        }
    }

    /// The classification of design §8.3, phase by phase.
    #[test]
    fn test_only_failed_connects_and_congestion_are_evidence() {
        use AdmissionClass::{Congestion, NoEvidence, Refused};
        use AttemptPhase::{Connecting, Streaming};

        let status = TaskError::StreamError {
            source: AdapterError::Retryable {
                source: RetryableError::Status { status: 502 },
            },
        };
        let unretryable = TaskError::StreamError {
            source: AdapterError::Unretryable {
                source: UnretryableError::RangeIgnored,
            },
        };
        let spawn = TaskError::Other {
            message: "runtime is shut down".into(),
        };
        for (phase, error, class) in [
            (Connecting, io(), Refused),
            (Connecting, TaskError::Timeout, Refused),
            (Connecting, congestion(), Congestion),
            (Streaming, congestion(), Congestion),
            (Connecting, status.clone(), NoEvidence),
            (Connecting, TaskError::ChannelClosed, NoEvidence),
            (Connecting, spawn, NoEvidence),
            (Connecting, unretryable.clone(), NoEvidence),
            (Streaming, io(), NoEvidence),
            (Streaming, TaskError::Timeout, NoEvidence),
            (Streaming, TaskError::SmallerThanTotalSize, NoEvidence),
            (Streaming, TaskError::ChannelClosed, NoEvidence),
            (Streaming, status, NoEvidence),
            (Streaming, unretryable, NoEvidence),
        ] {
            assert_eq!(classify(phase, &error), class, "{phase:?} {error:?}");
        }
    }

    /// Three corroborated refusals, each with four others in flight.
    fn streak_of_three(evidence: &mut AdmissionEvidence) {
        for _ in 0..3 {
            evidence.on_attempt_ended(UNSEEN, AdmissionClass::Refused, 4, 4);
        }
    }

    #[test]
    fn test_a_connect_at_the_level_ends_the_streak() {
        let mut evidence = AdmissionEvidence::default();
        evidence.on_attempt_ended(UNSEEN, AdmissionClass::Refused, 4, 5);
        streak_of_three(&mut evidence);
        assert_eq!(evidence.streak_count(), 4);
        assert_eq!(evidence.streak_level(), Some(4));
        evidence.on_started(9, 4);
        assert_eq!(evidence.streak_count(), 0);
        assert!(evidence.is_connected(9));
    }

    #[test]
    fn test_a_connect_below_it_does_not() {
        let mut evidence = AdmissionEvidence::default();
        streak_of_three(&mut evidence);
        evidence.on_started(9, 3);
        assert_eq!(evidence.streak_count(), 3);
        // Connects before the streak began do not matter either.
        let mut evidence = AdmissionEvidence::default();
        for id in 0..20 {
            evidence.on_started(id, 7);
        }
        streak_of_three(&mut evidence);
        assert_eq!(evidence.streak_count(), 3);
    }

    #[test]
    fn test_uncorroborated_refusals_start_no_streak() {
        let mut evidence = AdmissionEvidence::default();
        for _ in 0..5 {
            evidence.on_attempt_ended(UNSEEN, AdmissionClass::Refused, 0, 4);
        }
        assert_eq!(evidence.streak_count(), 0);
        evidence.on_attempt_ended(UNSEEN, AdmissionClass::NoEvidence, 4, 4);
        assert_eq!(evidence.streak_count(), 0);
        assert!(!evidence.congestion_pending());
        evidence.on_attempt_ended(UNSEEN, AdmissionClass::Congestion, 0, 0);
        assert!(evidence.take_congestion());
        assert!(!evidence.take_congestion());
    }

    #[test]
    fn test_instants_wait_for_the_stamp() {
        let t0 = Instant::now();
        let mut evidence = AdmissionEvidence::default();
        evidence.on_attempt_ended(UNSEEN, AdmissionClass::Refused, 1, 1);
        evidence.on_attempt_ended(UNSEEN, AdmissionClass::Refused, 1, 1);
        assert_eq!(evidence.last_refusal_at(), None);
        evidence.stamp(t0);
        assert_eq!(evidence.last_refusal_at(), Some(t0));
        assert_eq!(evidence.third_refusal_at(), None);

        evidence.on_attempt_ended(UNSEEN, AdmissionClass::Refused, 1, 1);
        assert_eq!(evidence.third_refusal_at(), None, "unknown until stamped");
        assert_eq!(evidence.last_refusal_at(), None);
        evidence.stamp(t0 + SECOND);
        assert_eq!(evidence.third_refusal_at(), Some(t0 + SECOND));
        // A stamp never moves a known instant.
        evidence.on_attempt_ended(UNSEEN, AdmissionClass::Refused, 1, 1);
        evidence.stamp(t0 + 2 * SECOND);
        assert_eq!(evidence.third_refusal_at(), Some(t0 + SECOND));
        assert_eq!(evidence.last_refusal_at(), Some(t0 + 2 * SECOND));

        // The occupancy of a step that a `Started` completes.
        evidence.on_started(0, 0);
        evidence.begin_step(2, t0);
        assert_eq!(evidence.occupied_since(), None);
        evidence.on_started(1, 1);
        assert_eq!(evidence.occupied_since(), None, "unknown until stamped");
        evidence.stamp(t0 + 3 * SECOND);
        assert_eq!(evidence.occupied_since(), Some(t0 + 3 * SECOND));
    }

    /// Two refusals at 0.1 and 0.2 s, stamped as the loop handles them; a
    /// third at 90.3 s, after the gap of 90 s since the second, starts a new
    /// streak, though no tick came in between.
    #[test]
    fn test_a_refusal_after_the_gap_starts_a_new_streak() {
        let t0 = Instant::now();
        let mut evidence = AdmissionEvidence::default();
        assert_eq!(evidence.streak_gap, Duration::from_secs(90));
        for at in [100, 200] {
            evidence.on_attempt_ended(UNSEEN, AdmissionClass::Refused, 1, 4);
            evidence.stamp(t0 + Duration::from_millis(at));
        }
        evidence.on_attempt_ended(UNSEEN, AdmissionClass::Refused, 1, 4);
        assert_eq!(evidence.streak_count(), 3, "unknown until stamped");
        assert_eq!(evidence.third_refusal_at(), None);
        evidence.stamp(t0 + Duration::from_millis(90_300));
        assert_eq!(evidence.streak_count(), 1);
        assert_eq!(evidence.third_refusal_at(), None);

        // Just inside the gap, it extends the streak.
        let mut evidence = AdmissionEvidence::default();
        for at in [100, 200, 90_199] {
            evidence.on_attempt_ended(UNSEEN, AdmissionClass::Refused, 1, 4);
            evidence.stamp(t0 + Duration::from_millis(at));
        }
        assert_eq!(evidence.streak_count(), 3);
        assert_eq!(
            evidence.third_refusal_at(),
            Some(t0 + Duration::from_millis(90_199))
        );
        // A stamp after the gap without a refusal ends it.
        evidence.stamp(t0 + Duration::from_millis(180_199));
        assert_eq!(evidence.streak_count(), 0);
    }

    /// The gap of a connect timeout over 30 years counts with the 30 years
    /// the runners wait for it.
    #[test]
    fn test_the_streak_gap_counts_the_capped_connect_timeout() {
        let thirty_years = Duration::from_secs(86400 * 365 * 30);
        let capped = 2 * thirty_years + Duration::from_secs(30);
        assert_eq!(streak_gap(Duration::MAX), capped);
        assert_eq!(streak_gap(Duration::MAX / 2), capped);
        assert_eq!(
            streak_gap(thirty_years + Duration::from_secs(86400 * 365)),
            capped
        );
        assert_eq!(streak_gap(Duration::from_secs(30)), Duration::from_secs(90));
        let t0 = Instant::now();
        let mut evidence = AdmissionEvidence::with_streak_gap(Duration::MAX);
        evidence.on_attempt_ended(UNSEEN, AdmissionClass::Refused, 1, 4);
        evidence.stamp(t0);
        evidence.stamp(t0 + Duration::from_secs(1 << 30));
        assert_eq!(evidence.streak_count(), 1);
    }

    #[test]
    fn test_a_loss_clears_an_unstamped_occupancy() {
        let t0 = Instant::now();
        let mut evidence = AdmissionEvidence::default();
        evidence.on_started(0, 0);
        evidence.begin_step(2, t0);
        evidence.on_started(1, 1);
        evidence.on_ended(0);
        evidence.stamp(t0 + SECOND);
        assert_eq!(evidence.occupied_since(), None);
        assert_eq!(evidence.connected(), 1);
        // A replacement starts the occupancy again, from its own stamp.
        evidence.on_started(2, 1);
        evidence.stamp(t0 + 2 * SECOND);
        assert_eq!(evidence.occupied_since(), Some(t0 + 2 * SECOND));
    }

    /// A step that begins with its trial connected is occupied from its
    /// start; an end that leaves the trial connected keeps it.
    #[test]
    fn test_a_step_begun_at_its_trial_is_occupied_from_its_start() {
        let t0 = Instant::now();
        let mut evidence = AdmissionEvidence::default();
        for id in 0..3 {
            evidence.on_started(id, id);
        }
        evidence.begin_step(2, t0);
        assert_eq!(evidence.occupied_since(), Some(t0));
        evidence.on_ended(2);
        assert_eq!(evidence.occupied_since(), Some(t0));
        evidence.on_ended(7);
        assert_eq!(evidence.occupied_since(), Some(t0), "never connected");
        evidence.on_ended(1);
        assert_eq!(evidence.occupied_since(), None);
        evidence.end_step();
        evidence.on_started(1, 1);
        assert_eq!(evidence.occupied_since(), None, "no step runs");
    }

    /// The marks follow program order: what was classified before the step
    /// began does not mark it.
    #[test]
    fn test_only_what_follows_the_step_marks_it() {
        let t0 = Instant::now();
        let mut evidence = AdmissionEvidence::default();
        evidence.on_attempt_ended(UNSEEN, AdmissionClass::Refused, 1, 4);
        evidence.on_attempt_ended(UNSEEN, AdmissionClass::Refused, 0, 4);
        evidence.on_attempt_ended(UNSEEN, AdmissionClass::Congestion, 1, 4);
        evidence.begin_step(5, t0);
        assert!(!evidence.step_rejected() && !evidence.step_unclear());
        assert_eq!(evidence.streak_count(), 0, "a step ends the streak");

        evidence.on_attempt_ended(UNSEEN, AdmissionClass::Refused, 0, 4);
        assert!(evidence.step_unclear() && !evidence.step_rejected());
        evidence.on_attempt_ended(UNSEEN, AdmissionClass::Refused, 1, 4);
        assert!(evidence.step_rejected());
        evidence.end_step();
        assert!(!evidence.step_rejected() && !evidence.step_unclear());

        evidence.begin_step(5, t0);
        evidence.on_attempt_ended(UNSEEN, AdmissionClass::Congestion, 0, 0);
        assert!(evidence.step_rejected());
    }

    /// Only a corroborated refusal with at least `max(window, 1)` others in
    /// flight is exempt from its range's budget.
    #[test]
    fn test_only_a_refusal_above_the_window_is_exempt() {
        use AdmissionClass::{Congestion, NoEvidence, Refused};

        let mut evidence = AdmissionEvidence::default();
        assert!(!evidence.exempts(Refused, 0, 4), "uncorroborated");
        assert!(
            evidence.exempts(Refused, 1, 1),
            "a window of none counts as one"
        );
        assert!(!evidence.exempts(Refused, 1, 0));
        evidence.observe_proven(4);
        assert!(!evidence.exempts(Refused, 3, 3), "inside the window");
        assert!(evidence.exempts(Refused, 3, 4));
        assert!(!evidence.exempts(NoEvidence, 4, 4));
        assert!(!evidence.exempts(Congestion, 4, 4));
        evidence.on_lowered(3);
        assert!(evidence.exempts(Refused, 3, 3), "clamped at the lowering");
    }

    /// A connect seen beside a proven attempt, refused once none is
    /// proven any more, is corroborated at the most others it was seen
    /// with: the reconnects of its range, at a lower level, keep the
    /// streak, and a connect at the level ends it. One seen only while
    /// none was proven is not corroborated.
    #[test]
    fn test_a_connect_seen_beside_a_proven_attempt_is_corroborated() {
        use AdmissionClass::Refused;

        let mut evidence = AdmissionEvidence::default();
        evidence.observe_connecting([4, 5, 6, 7], 0, 7);
        evidence.on_attempt_ended(4, Refused, 0, 3);
        assert_eq!(evidence.streak_count(), 0, "never beside a proven one");

        evidence.observe_connecting([5], 4, 4);
        evidence.observe_connecting([5, 6, 7], 4, 7);
        evidence.observe_connecting([5, 6, 7], 1, 5);
        for (id, others) in [(5, 2), (6, 1), (7, 0)] {
            evidence.on_attempt_ended(id, Refused, 0, others);
        }
        assert_eq!(evidence.streak_count(), 3);
        assert_eq!(evidence.streak_level(), Some(7));
        for (id, others) in [(5, 0), (6, 1), (7, 2), (8, 6)] {
            evidence.on_started(id, others);
        }
        assert_eq!(evidence.streak_count(), 3);
        evidence.on_started(9, 7);
        assert_eq!(evidence.streak_count(), 0);
    }

    /// What a connect was seen beside belongs to its attempt: it outlives
    /// the release of the slot until the classification, and is gone once
    /// the attempt connected, once it was classified, and when its ID is
    /// reserved for another attempt.
    #[test]
    fn test_an_attempt_keeps_what_it_was_seen_beside_until_it_ends() {
        use AdmissionClass::Refused;

        let mut evidence = AdmissionEvidence::default();
        evidence.observe_connecting([1, 2, 3], 2, 4);
        evidence.on_ended(1);
        evidence.on_attempt_ended(1, Refused, 0, 2);
        assert_eq!(evidence.streak_count(), 1, "released, then classified");
        evidence.on_attempt_ended(1, Refused, 0, 2);
        assert_eq!(evidence.streak_count(), 1, "classified once");
        // Below the streak's level of four.
        evidence.on_started(2, 0);
        evidence.on_attempt_ended(2, Refused, 0, 2);
        evidence.forget(3);
        evidence.on_attempt_ended(3, Refused, 0, 2);
        assert_eq!(evidence.streak_count(), 1);
    }

    /// A refusal corroborated now and by what it was seen beside takes
    /// the larger level; one corroborated only by what it was seen beside
    /// rejects a running step, and is not exempt from its range's budget.
    #[test]
    fn test_a_refusal_seen_beside_a_proven_attempt_rejects_a_step() {
        use AdmissionClass::Refused;

        let t0 = Instant::now();
        let mut evidence = AdmissionEvidence::default();
        evidence.observe_connecting([1], 2, 6);
        evidence.on_attempt_ended(1, Refused, 2, 3);
        assert_eq!(evidence.streak_level(), Some(6));
        evidence.on_attempt_ended(2, Refused, 2, 3);
        assert_eq!(evidence.streak_level(), Some(3));

        evidence.observe_connecting([3], 4, 4);
        evidence.begin_step(5, t0);
        evidence.on_attempt_ended(3, Refused, 0, 4);
        assert!(evidence.step_rejected() && !evidence.step_unclear());
        assert!(!evidence.exempts(Refused, 0, 4));
        assert_eq!(evidence.streak_count(), 1);
    }

    #[test]
    fn test_the_proven_window_is_clamped_at_a_lowering() {
        let mut evidence = AdmissionEvidence::default();
        evidence.observe_proven(3);
        evidence.on_attempt_ended(UNSEEN, AdmissionClass::NoEvidence, 6, 6);
        evidence.observe_proven(2);
        assert_eq!(evidence.proven_window(), 6);
        streak_of_three(&mut evidence);
        evidence.on_lowered(4);
        assert_eq!(evidence.proven_window(), 4);
        assert_eq!(evidence.streak_count(), 0);
        evidence.observe_proven(5);
        assert_eq!(evidence.proven_window(), 5);
        evidence.reset_streak();
    }
}
