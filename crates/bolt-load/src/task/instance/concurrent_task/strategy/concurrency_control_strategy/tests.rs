//! The limit controller with injected time (design §8.3).

use super::{super::admission_evidence::AdmissionClass, *};

const TICK: Duration = DEFAULT_STRATEGY_TICK_INTERVAL;
const SECOND: Duration = Duration::from_secs(1);

fn context(
    admission: &mut AdmissionEvidence,
    active_runners: usize,
    available_chunks: Vec<Range<u64>>,
    now: Instant,
) -> ConcurrencyControlContext<'_> {
    ConcurrencyControlContext {
        want: usize::MAX,
        active_runners,
        proven_runners: active_runners,
        admission,
        available_chunks,
        congestion_until: None,
        last_progress_at: now,
        writer_ready: true,
        now,
    }
}

fn congest(admission: &mut AdmissionEvidence) {
    admission.on_attempt_ended(AdmissionClass::Congestion, 1, 1);
}

#[test]
#[expect(clippy::single_range_in_vec_init, reason = "a list holding one range")]
fn test_immediate_degradation_never_drops_to_zero() {
    let mut strategy = ConcurrencyControlStrategy::with_default_config(4);
    let mut admission = AdmissionEvidence::default();
    let mut context = context(&mut admission, 0, vec![0..100], Instant::now());
    // The first step only schedules the initial runners.
    strategy.step(&mut context);

    congest(context.admission);
    context.now = Instant::now();
    let actions = strategy.step(&mut context);
    assert!(
        matches!(actions.as_slice(), [StrategyAction::ChangeMaxConcurrency(n)] if *n >= 1),
        "unexpected actions: {actions:?}"
    );
}

fn created(actions: &[StrategyAction]) -> Vec<Range<u64>> {
    actions
        .iter()
        .map(|action| match action {
            StrategyAction::CreateTask(range) => range.clone(),
            other => panic!("unexpected action: {other:?}"),
        })
        .collect()
}

/// Between steps, runners are created only as a step would create them,
/// and only once the step lowered the limit for a congestion.
#[test]
#[expect(clippy::single_range_in_vec_init, reason = "a list holding one range")]
fn test_create_tasks_waits_for_the_first_step_and_the_degradation() {
    let mut strategy = ConcurrencyControlStrategy::with_default_config(4);
    let mut admission = AdmissionEvidence::default();
    let mut context = context(
        &mut admission,
        1,
        vec![0..10, 20..30, 40..50, 60..70],
        Instant::now(),
    );
    assert!(strategy.create_tasks(&context).is_empty());
    strategy.step(&mut context);

    // Three free slots.
    assert_eq!(
        created(&strategy.create_tasks(&context)),
        [0..10, 20..30, 40..50]
    );
    context.active_runners = 4;
    assert!(strategy.create_tasks(&context).is_empty());

    context.active_runners = 1;
    congest(context.admission);
    context.now = Instant::now();
    assert!(strategy.create_tasks(&context).is_empty());
    // Still pending: `create_tasks` left it to the step.
    let actions = strategy.step(&mut context);
    assert!(
        matches!(
            actions.as_slice(),
            [StrategyAction::ChangeMaxConcurrency(1)]
        ),
        "unexpected actions: {actions:?}"
    );
    context.active_runners = 0;
    assert_eq!(created(&strategy.create_tasks(&context)), [0..10]);
}

/// A strategy and its evidence, driven tick by tick.
struct Ctl {
    strategy: ConcurrencyControlStrategy,
    admission: AdmissionEvidence,
    next_id: usize,
    /// Runners connecting.
    connecting: usize,
    /// The proven runners, when not every connected one.
    proven: Option<usize>,
    /// The last progress, when not the time of the tick.
    progress: Option<Instant>,
    writer_ready: bool,
    congestion_until: Option<Instant>,
}

impl Ctl {
    /// Past its first step at `t0`, with a limit of `limit`, and
    /// `connected` runners connected.
    fn new(limit: usize, connected: usize, t0: Instant) -> Self {
        let mut ctl = Self {
            strategy: ConcurrencyControlStrategy::with_default_config(limit),
            admission: AdmissionEvidence::default(),
            next_id: 0,
            connecting: 0,
            proven: None,
            progress: None,
            writer_ready: true,
            congestion_until: None,
        };
        for _ in 0..connected {
            ctl.connect();
        }
        ctl.tick(t0);
        ctl
    }

    fn limit(&self) -> usize {
        self.strategy.limit
    }

    fn cap(&self) -> usize {
        self.strategy.cap(usize::MAX)
    }

    fn stepping(&self) -> bool {
        self.strategy.step.is_some()
    }

    /// One more runner connects; returns its ID.
    fn connect(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        let others = self.admission.connected() + self.connecting;
        self.admission.on_started(id, others);
        id
    }

    fn release(&mut self, id: usize) {
        self.admission.on_ended(id);
    }

    /// `n` refusals, each beside the connected runners, one proven.
    fn refuse(&mut self, n: usize) {
        let others = self.admission.connected();
        for _ in 0..n {
            self.admission
                .on_attempt_ended(AdmissionClass::Refused, 1, others);
        }
    }

    fn refuse_uncorroborated(&mut self) {
        let others = self.admission.connected();
        self.admission
            .on_attempt_ended(AdmissionClass::Refused, 0, others);
    }

    /// The step at `now`, with `want` wanted; returns the new cap, if the
    /// step changed it.
    fn tick_wanting(&mut self, now: Instant, want: usize) -> Option<usize> {
        let connected = self.admission.connected();
        let mut context = ConcurrencyControlContext {
            want,
            active_runners: connected + self.connecting,
            proven_runners: self.proven.unwrap_or(connected),
            admission: &mut self.admission,
            available_chunks: Vec::new(),
            congestion_until: self.congestion_until,
            last_progress_at: self.progress.unwrap_or(now),
            writer_ready: self.writer_ready,
            now,
        };
        changed_limit(&self.strategy.step(&mut context))
    }

    fn tick(&mut self, now: Instant) -> Option<usize> {
        self.tick_wanting(now, usize::MAX)
    }

    /// Tick every interval from `from` while not past `until`; returns the
    /// ticks that changed the cap.
    fn ticks(&mut self, from: Instant, until: Instant) -> Vec<(Instant, usize)> {
        let mut changes = Vec::new();
        let mut now = from;
        while now <= until {
            if let Some(cap) = self.tick(now) {
                changes.push((now, cap));
            }
            now += TICK;
        }
        changes
    }
}

fn changed_limit(actions: &[StrategyAction]) -> Option<usize> {
    actions.iter().find_map(|action| match action {
        StrategyAction::ChangeMaxConcurrency(n) => Some(*n),
        _ => None,
    })
}

#[test]
fn test_three_refusals_and_progress_lower_the_limit() {
    let t0 = Instant::now();
    let mut ctl = Ctl::new(16, 4, t0);
    ctl.refuse(2);
    ctl.admission.stamp(t0);
    // Two are not enough, whatever the progress.
    assert_eq!(ctl.tick(t0 + TICK), None);
    ctl.refuse(1);
    let third = t0 + TICK + SECOND;
    ctl.admission.stamp(third);
    // Progress before one tick interval after the third does not confirm
    // it.
    ctl.progress = Some(third + TICK - SECOND / 2);
    for now in [third + SECOND, third + TICK] {
        assert_eq!(ctl.tick(now), None, "at {:?}", now - third);
    }
    ctl.progress = Some(third + TICK);
    assert_eq!(ctl.tick(third + 2 * TICK), Some(4));
}

#[test]
fn test_a_lowered_limit_is_lowered_again() {
    let t0 = Instant::now();
    let mut ctl = Ctl::new(16, 4, t0);
    ctl.refuse(3);
    assert_eq!(
        ctl.tick(t0 + TICK),
        None,
        "the stamp of this step is the third refusal's"
    );
    assert_eq!(ctl.tick(t0 + 2 * TICK), Some(4));
    // Two runners are lost, and their reconnects are refused.
    ctl.release(0);
    ctl.release(1);
    ctl.refuse(3);
    let third = t0 + 3 * TICK;
    ctl.admission.stamp(third);
    assert_eq!(ctl.tick(third + TICK), Some(2));
    // Not below what is connected.
    ctl.refuse(3);
    let third = third + 2 * TICK;
    ctl.admission.stamp(third);
    assert_eq!(ctl.tick(third + TICK), None);
}

#[test]
fn test_old_successes_do_not_dilute_a_streak() {
    let t0 = Instant::now();
    let mut ctl = Ctl::new(8, 0, t0);
    // Twenty connects with seven others in flight, then the server goes
    // from eight to seven.
    for id in 0..20 {
        ctl.admission.on_started(id, 7);
        if id >= 7 {
            ctl.release(id);
        }
    }
    assert_eq!(ctl.admission.connected(), 7);
    ctl.refuse(3);
    ctl.admission.stamp(t0);
    assert_eq!(ctl.tick(t0 + TICK), Some(7));
}

#[test]
fn test_no_progress_after_the_refusals_keeps_the_limit() {
    let t0 = Instant::now();
    let mut ctl = Ctl::new(16, 4, t0);
    ctl.refuse(5);
    ctl.admission.stamp(t0);
    ctl.progress = Some(t0);
    assert_eq!(ctl.ticks(t0 + TICK, t0 + TICK * 40), []);
    assert_eq!(ctl.limit(), 16);
}

#[test]
fn test_a_streak_expires_before_it_is_confirmed() {
    let t0 = Instant::now();
    let mut ctl = Ctl::new(16, 4, t0);
    ctl.refuse(3);
    ctl.admission.stamp(t0);
    let gap = Duration::from_secs(90);
    ctl.progress = Some(t0);
    assert_eq!(ctl.ticks(t0 + TICK, t0 + gap), []);
    assert_eq!(ctl.admission.streak_count(), 0);
    ctl.progress = None;
    assert_eq!(ctl.tick(t0 + gap + TICK), None);
}

/// Two refusals at 0.1 and 0.2 s, ticks every 1.5 s, and a third refusal
/// at 90.3 s, after the 90 s gap but before the next tick: it starts a new
/// streak, and the progress after it lowers nothing.
#[test]
fn test_a_refusal_after_the_gap_does_not_revive_the_streak() {
    let t0 = Instant::now();
    let ms = Duration::from_millis;
    let mut ctl = Ctl::new(16, 4, t0);
    for at in [100, 200] {
        ctl.refuse(1);
        ctl.admission.stamp(t0 + ms(at));
    }
    ctl.progress = Some(t0);
    let mut now = t0;
    while now + TICK <= t0 + ms(90_300) {
        now += TICK;
        assert_eq!(ctl.tick(now), None);
    }
    // Handled by the loop at its own time, before the next tick.
    ctl.refuse(1);
    ctl.admission.stamp(t0 + ms(90_300));
    ctl.progress = None;
    assert_eq!(ctl.ticks(now + TICK, now + 4 * TICK), []);
    assert_eq!(ctl.admission.streak_count(), 1);
}

/// A connect timeout too long for the deadline arithmetic saturates: no
/// panic, and a step then ends only by its marks or its acceptance.
#[test]
fn test_a_huge_connect_timeout_saturates_the_step_deadline() {
    let config = ConcurrencyControlConfig::with_connect_timeout(Duration::MAX);
    assert_eq!(config.step_deadline, Duration::MAX);
    let config = ConcurrencyControlConfig::with_connect_timeout(Duration::MAX - SECOND);
    assert_eq!(config.step_deadline, Duration::MAX);

    let t0 = Instant::now();
    let mut ctl = Ctl::new(8, 4, t0 - TICK);
    ctl.strategy = ConcurrencyControlStrategy::new(config, 8);
    ctl.tick(t0 - TICK);
    ctl.refuse(3);
    ctl.admission.stamp(t0 - TICK);
    assert_eq!(ctl.tick(t0), Some(4));
    let due = start_step(&mut ctl, t0);
    // The trial connects but never delivers: no deadline ends the step.
    ctl.connect();
    ctl.proven = Some(4);
    assert_eq!(ctl.ticks(due + TICK, due + 600 * SECOND), []);
    assert!(ctl.stepping());
    ctl.refuse(1);
    assert_eq!(ctl.tick(due + 601 * SECOND), Some(4));
}

#[test]
fn test_uncorroborated_refusals_count_for_nothing() {
    let t0 = Instant::now();
    let mut ctl = Ctl::new(16, 4, t0);
    for _ in 0..5 {
        ctl.refuse_uncorroborated();
    }
    ctl.admission.stamp(t0);
    assert_eq!(ctl.tick(t0 + 2 * TICK), None);
}

/// A stream that ends short after its bytes is no evidence, so it lowers
/// nothing; the old rule degraded at once for it.
#[test]
fn test_a_short_stream_with_bytes_does_not_lower_the_limit() {
    use super::super::admission_evidence::{AttemptPhase, classify};
    use crate::runner::TaskError;

    let t0 = Instant::now();
    let mut ctl = Ctl::new(16, 4, t0);
    let class = classify(AttemptPhase::Streaming, &TaskError::SmallerThanTotalSize);
    for _ in 0..5 {
        ctl.admission.on_attempt_ended(class, 4, 4);
    }
    assert_eq!(ctl.ticks(t0 + TICK, t0 + 3 * TICK), []);
}

#[test]
fn test_a_congestion_never_raises_the_limit() {
    let t0 = Instant::now();
    let mut ctl = Ctl::new(4, 4, t0);
    // Six active, which a limit of four allowed before it was set.
    ctl.connecting = 2;
    congest(&mut ctl.admission);
    assert_eq!(ctl.tick(t0 + TICK), Some(4));
    ctl.connecting = 0;
    ctl.release(0);
    ctl.release(1);
    congest(&mut ctl.admission);
    assert_eq!(ctl.tick(t0 + 2 * TICK), Some(2));
}

/// At a late tick, queued events may be about to change what the evidence
/// shows: the silent rule waits for the next tick.
#[test]
fn test_a_late_tick_lowers_nothing() {
    let t0 = Instant::now();
    let mut ctl = Ctl::new(16, 4, t0);
    ctl.refuse(3);
    ctl.admission.stamp(t0);
    let late = t0 + 2 * TICK + SECOND;
    assert_eq!(ctl.tick(late), None);
    ctl.progress = Some(late);
    assert_eq!(ctl.tick(late + TICK), Some(4));
}

/// While the writer takes no data, the progress it would accept is
/// unknown: the silent rule waits.
#[test]
fn test_a_full_writer_lowers_nothing() {
    let t0 = Instant::now();
    let mut ctl = Ctl::new(16, 4, t0);
    ctl.refuse(3);
    ctl.admission.stamp(t0);
    ctl.writer_ready = false;
    assert_eq!(ctl.tick(t0 + TICK), None);
    ctl.writer_ready = true;
    assert_eq!(ctl.tick(t0 + 2 * TICK), Some(4));
}

/// A limit of 4 of 8, lowered at `t0` by three refusals.
fn lowered_to_four(t0: Instant) -> Ctl {
    let mut ctl = Ctl::new(8, 4, t0 - TICK);
    ctl.refuse(3);
    ctl.admission.stamp(t0 - TICK);
    assert_eq!(ctl.tick(t0), Some(4));
    ctl
}

/// Tick every interval after `t0` until the first step begins, when it
/// is due; returns that time.
fn start_step(ctl: &mut Ctl, t0: Instant) -> Instant {
    let due = t0 + base_wait();
    assert_eq!(ctl.ticks(t0 + TICK, due - SECOND), []);
    assert_eq!(ctl.tick(due), Some(ctl.limit() + 1));
    due
}

fn base_wait() -> Duration {
    ConcurrencyControlConfig::default().base_wait
}

fn observation() -> Duration {
    ConcurrencyControlConfig::default().observation
}

#[test]
fn test_a_step_needs_want_a_due_time_and_a_connected_limit() {
    let t0 = Instant::now();
    let due = t0 + base_wait();

    // Not before it is due, then at the first tick from then.
    let mut ctl = lowered_to_four(t0);
    assert_eq!(ctl.ticks(t0 + TICK, due - SECOND), []);
    assert_eq!(ctl.tick(due), Some(5));
    assert!(ctl.stepping());

    // Not while fewer than the limit are connected.
    let mut ctl = lowered_to_four(t0);
    ctl.release(3);
    assert_eq!(ctl.ticks(t0 + TICK, due), []);
    ctl.connect();
    assert_eq!(ctl.tick(due + TICK), Some(5));

    // Not while a congestion holds.
    let mut ctl = lowered_to_four(t0);
    ctl.congestion_until = Some(due + TICK);
    assert_eq!(ctl.ticks(t0 + TICK, due), []);
    assert_eq!(ctl.tick(due + TICK), Some(5));

    // Not when no more is wanted.
    let mut ctl = lowered_to_four(t0);
    assert_eq!(ctl.ticks(t0 + TICK, due - SECOND), []);
    assert_eq!(ctl.tick_wanting(due, 4), None);
    assert_eq!(ctl.strategy.cap(4), 4);
    assert_eq!(ctl.tick_wanting(due + TICK, 5), Some(5));
}

/// The trial is credited from its own `Started`, not from the step's
/// start.
#[test]
fn test_a_step_is_accepted_after_unbroken_occupancy() {
    let t0 = Instant::now();
    let mut ctl = lowered_to_four(t0);
    let due = start_step(&mut ctl, t0);
    // The trial connects two ticks later.
    assert_eq!(ctl.tick(due + TICK), None);
    let connected_at = due + 2 * TICK;
    ctl.connect();
    ctl.admission.stamp(connected_at);
    let changes = ctl.ticks(connected_at, connected_at + observation() - SECOND);
    assert_eq!(changes, []);
    assert!(ctl.stepping());
    // Accepted at the first tick 15 s after the trial connected.
    assert_eq!(ctl.tick(connected_at + observation()), None);
    assert!(!ctl.stepping());
    assert_eq!((ctl.limit(), ctl.cap()), (5, 5));
    // Due again at once, so the next step begins at the next tick.
    assert_eq!(ctl.tick(connected_at + observation() + TICK), Some(6));
}

#[test]
fn test_a_replacement_between_two_ticks_resets_the_occupancy() {
    let t0 = Instant::now();
    let mut ctl = lowered_to_four(t0);
    let due = start_step(&mut ctl, t0);
    let trial = ctl.connect();
    ctl.admission.stamp(due);
    assert_eq!(ctl.ticks(due + TICK, due + 5 * TICK), []);
    // It ends just after a tick; its replacement starts just before the
    // next.
    ctl.release(trial);
    ctl.connect();
    let replaced = due + 6 * TICK - SECOND / 10;
    ctl.admission.stamp(replaced);
    // Not accepted 15 s after the first trial connected.
    let changes = ctl.ticks(due + 6 * TICK, replaced + observation() - SECOND / 10);
    assert_eq!(changes, []);
    assert!(ctl.stepping());
    // But 15 s after its replacement.
    assert_eq!(ctl.tick(replaced + observation() + TICK), None);
    assert_eq!(ctl.limit(), 5);
}

#[test]
fn test_a_loss_while_the_writer_is_full_resets_the_occupancy() {
    let t0 = Instant::now();
    let mut ctl = lowered_to_four(t0);
    let due = start_step(&mut ctl, t0);
    let trial = ctl.connect();
    ctl.admission.stamp(due);
    ctl.writer_ready = false;
    ctl.ticks(due + TICK, due + 4 * TICK);
    ctl.release(trial);
    ctl.ticks(due + 5 * TICK, due + 6 * TICK);
    ctl.connect();
    let reconnected = due + 7 * TICK;
    ctl.admission.stamp(reconnected);
    ctl.writer_ready = true;
    assert_eq!(
        ctl.ticks(reconnected, reconnected + observation() - TICK),
        []
    );
    assert!(ctl.stepping(), "accepted before 15 s of the new trial");
}

#[test]
fn test_a_late_tick_accepts_nothing() {
    let t0 = Instant::now();
    let mut ctl = lowered_to_four(t0);
    let due = start_step(&mut ctl, t0);
    ctl.connect();
    ctl.admission.stamp(due);
    assert_eq!(ctl.ticks(due + TICK, due + observation() - TICK), []);
    let late = due + observation() + 2 * TICK;
    assert_eq!(ctl.tick(late), None);
    assert!(ctl.stepping());
    assert_eq!(ctl.tick(late + TICK), None);
    assert!(!ctl.stepping());
    assert_eq!(ctl.limit(), 5);
}

/// A trial that connects and never delivers: the step ends inconclusive
/// at the first tick from its deadline, with the wait unchanged.
#[test]
fn test_a_step_ends_at_its_deadline() {
    let t0 = Instant::now();
    let config = ConcurrencyControlConfig::default();
    assert_eq!(config.step_deadline, Duration::from_secs(48));
    let mut ctl = lowered_to_four(t0);
    let due = start_step(&mut ctl, t0);
    ctl.connect();
    ctl.proven = Some(4);
    ctl.writer_ready = false;
    let deadline = due + config.step_deadline;
    assert_eq!(ctl.ticks(due + TICK, deadline - SECOND), []);
    let first_from = due + TICK * 32;
    assert!(first_from >= deadline && first_from - TICK < deadline);
    assert_eq!(ctl.tick(first_from), Some(4));
    assert_eq!(
        ctl.strategy.state(),
        (
            4,
            None,
            Some(first_from + config.base_wait),
            config.base_wait
        )
    );
}

/// Seven long-lived runners at a limit of six, after an inconclusive step:
/// the next step opens nothing, counts the seventh, and is accepted.
#[test]
#[expect(clippy::single_range_in_vec_init, reason = "a list holding one range")]
fn test_the_next_step_counts_a_trial_that_stayed() {
    let t0 = Instant::now();
    let config = ConcurrencyControlConfig::default();
    let mut ctl = Ctl::new(8, 6, t0 - TICK);
    ctl.refuse(3);
    ctl.admission.stamp(t0 - TICK);
    assert_eq!(ctl.tick(t0), Some(6));
    let due = t0 + config.base_wait;
    ctl.ticks(t0 + TICK, due - SECOND);
    assert_eq!(ctl.tick(due), Some(7));
    // The trial connects, but delivers nothing before the deadline.
    ctl.connect();
    ctl.proven = Some(6);
    let changes = ctl.ticks(due + TICK, due + config.step_deadline + TICK);
    let [(ended, 6)] = changes[..] else {
        panic!("{changes:?}");
    };
    // Seven run and deliver; nothing is added at a cap of six.
    ctl.proven = None;
    assert_eq!(ctl.cap(), 6);
    let next = ended + config.base_wait;
    ctl.ticks(ended + TICK, next - SECOND);
    let mut context = context(&mut ctl.admission, 7, vec![0..10], next);
    let actions = ctl.strategy.step(&mut context);
    assert!(
        matches!(actions[..], [StrategyAction::ChangeMaxConcurrency(7)]),
        "the step opens nothing: {actions:?}"
    );
    // Occupied since the step's start: accepted 15 s later.
    assert_eq!(ctl.ticks(next + TICK, next + config.observation), []);
    assert_eq!(ctl.limit(), 7);
}

#[test]
fn test_a_refusal_before_the_step_at_the_same_instant_does_not_reject_it() {
    let t0 = Instant::now();
    let mut ctl = lowered_to_four(t0);
    let due = t0 + base_wait();
    assert_eq!(ctl.ticks(t0 + TICK, due - SECOND), []);
    // Handled before the step at the same `now`; it alone does not lower.
    ctl.refuse(1);
    ctl.admission.stamp(due);
    assert_eq!(ctl.tick(due), Some(5));
    assert_eq!(ctl.tick(due + TICK), None);
    assert!(ctl.stepping());
}

/// Once a refusal marked the running step, nothing refills its trial slot
/// before the next tick decides the step; a runner below the limit is
/// still replaced.
#[test]
#[expect(clippy::single_range_in_vec_init, reason = "a list holding one range")]
fn test_a_marked_step_opens_no_other_trial_before_its_tick() {
    for unclear in [false, true] {
        let t0 = Instant::now();
        let mut ctl = lowered_to_four(t0);
        let due = start_step(&mut ctl, t0);
        assert_eq!(ctl.cap(), 5);
        let refill = |ctl: &mut Ctl, active| {
            let mut context = context(&mut ctl.admission, active, vec![0..10], due + TICK / 2);
            context.proven_runners = 4;
            created(&ctl.strategy.create_tasks(&context)).len()
        };
        assert_eq!(refill(&mut ctl, 4), 1, "the trial slot is open");
        if unclear {
            ctl.refuse_uncorroborated();
        } else {
            ctl.refuse(1);
        }
        assert_eq!(refill(&mut ctl, 4), 0, "unclear: {unclear}");
        assert_eq!(refill(&mut ctl, 3), 1, "unclear: {unclear}");
        assert_eq!(ctl.strategy.refill_cap(usize::MAX, &ctl.admission), 4);
        assert_eq!(ctl.tick(due + TICK), Some(4));
    }
}

#[test]
fn test_a_refusal_rejects_a_step() {
    let t0 = Instant::now();
    let mut ctl = lowered_to_four(t0);
    let due = start_step(&mut ctl, t0);
    ctl.refuse(1);
    assert_eq!(ctl.tick(due + TICK), Some(4));
    assert_eq!(
        ctl.strategy.state(),
        (4, None, Some(due + TICK + 2 * base_wait()), 2 * base_wait())
    );
}

#[test]
fn test_an_uncorroborated_one_makes_it_inconclusive() {
    let t0 = Instant::now();
    let mut ctl = lowered_to_four(t0);
    let due = start_step(&mut ctl, t0);
    ctl.refuse_uncorroborated();
    assert_eq!(ctl.tick(due + TICK), Some(4));
    assert_eq!(
        ctl.strategy.state(),
        (4, None, Some(due + TICK + base_wait()), base_wait())
    );
}

#[test]
fn test_a_congestion_rejects_a_step_and_lowers() {
    let t0 = Instant::now();
    let mut ctl = lowered_to_four(t0);
    let due = start_step(&mut ctl, t0);
    // Two runners are lost, then a 503 holds new connections for 5 s.
    ctl.release(0);
    ctl.release(1);
    congest(&mut ctl.admission);
    let hold_end = due + TICK + 5 * SECOND;
    ctl.congestion_until = Some(hold_end);
    assert_eq!(ctl.tick(due + TICK), Some(2));
    assert_eq!(
        ctl.strategy.state(),
        (2, None, Some(hold_end + 2 * base_wait()), 2 * base_wait())
    );
}

/// Rejected steps follow 30, 60, 120, 240, 300 and 300 s apart, each at
/// the first tick from when it is due; an inconclusive one keeps the wait.
#[test]
fn test_rejected_steps_back_off() {
    let t0 = Instant::now();
    let mut ctl = lowered_to_four(t0);
    let mut now = t0;
    let mut last = t0;
    let mut gaps = Vec::new();
    while gaps.len() < 6 {
        now += TICK;
        if ctl.tick(now) == Some(5) {
            gaps.push((now - last).as_secs_f64());
            // Its trial is refused at once.
            ctl.refuse(1);
            now += TICK;
            assert_eq!(ctl.tick(now), Some(4));
            last = now;
        }
    }
    let expected = [30.0, 60.0, 120.0, 240.0, 300.0, 300.0];
    for (gap, wait) in gaps.iter().zip(expected) {
        assert!(*gap >= wait && *gap < wait + TICK.as_secs_f64(), "{gaps:?}");
    }

    let wait = ctl.strategy.state().3;
    loop {
        now += TICK;
        if ctl.tick(now) == Some(5) {
            break;
        }
    }
    ctl.refuse_uncorroborated();
    now += TICK;
    assert_eq!(ctl.tick(now), Some(4));
    assert_eq!(ctl.strategy.state().3, wait);
}

#[test]
fn test_one_connection_never_steps() {
    let t0 = Instant::now();
    let mut ctl = Ctl::new(1, 1, t0);
    ctl.refuse(5);
    ctl.admission.stamp(t0);
    assert_eq!(ctl.ticks(t0 + TICK, t0 + 400 * SECOND), []);
    assert_eq!(ctl.strategy.state(), (1, None, None, base_wait()));
    assert_eq!(ctl.cap(), 1);
}
