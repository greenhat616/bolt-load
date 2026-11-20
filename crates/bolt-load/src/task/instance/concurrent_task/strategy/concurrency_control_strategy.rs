//! The Strategy that controls the max concurrency of the task, from what
//! failed connects tell about the server's connection limit (design §8.3)

use std::{
    ops::Range,
    time::{Duration, Instant},
};

use super::{
    DEFAULT_STRATEGY_TICK_INTERVAL, Strategy, StrategyAction, admission_evidence::AdmissionEvidence,
};

/// Corroborated refusals in a row before the silent rule lowers the limit.
const REFUSALS_IN_A_ROW: usize = 3;

#[derive(Debug, Clone)]
/// The configuration for the concurrency control strategy
pub struct ConcurrencyControlConfig {
    /// The minimum concurrency
    pub min_concurrency: usize,
    /// The wait before the first step, and after an accepted one
    pub base_wait: Duration,
    /// The longest wait between steps, however many were rejected
    pub max_wait: Duration,
    /// How long a step's trial must stay connected without a break
    pub observation: Duration,
    /// When a step that is still undecided ends, counted from its start
    pub step_deadline: Duration,
    /// The interval of the strategy tick: progress one interval after the
    /// third refusal confirms a streak, and a tick more than two intervals
    /// after the previous one is late.
    pub tick_interval: Duration,
}

impl ConcurrencyControlConfig {
    /// The defaults for runners that may take `connect_timeout` to connect.
    pub fn with_connect_timeout(connect_timeout: Duration) -> Self {
        let observation = Duration::from_secs(15);
        let tick_interval = DEFAULT_STRATEGY_TICK_INTERVAL;
        Self {
            min_concurrency: 1,
            base_wait: Duration::from_secs(30),
            max_wait: Duration::from_secs(300),
            observation,
            // Time to connect, to be observed, and two ticks to see it.
            // A deadline past what an instant can hold is never reached.
            step_deadline: connect_timeout
                .saturating_add(observation)
                .saturating_add(2 * tick_interval),
            tick_interval,
        }
    }
}

impl Default for ConcurrencyControlConfig {
    fn default() -> Self {
        Self::with_connect_timeout(crate::runner::DEFAULT_CONNECT_TIMEOUT)
    }
}

/// The context for the concurrency control strategy
#[derive(derive_more::Debug)]
pub struct ConcurrencyControlContext<'a> {
    /// The connections the growth side wants open
    pub want: usize,
    /// The current active runner count
    pub active_runners: usize,
    /// The runners that are running and had bytes accepted
    pub proven_runners: usize,
    /// What ended attempts tell about the connections the server admits
    #[debug(skip)]
    pub admission: &'a mut AdmissionEvidence,
    /// The ranges a runner may be created for now: none while a congestion
    /// holds back new connections, and no range still backing off.
    pub available_chunks: Vec<Range<u64>>,
    /// Until when a congestion holds back new connections
    pub congestion_until: Option<Instant>,
    /// When a non-empty frame was last accepted
    pub last_progress_at: Instant,
    /// Whether the writer can take data
    pub writer_ready: bool,
    /// The time of this step
    pub now: Instant,
}

/// The limit controller (design §8.3).
///
/// `limit` is what the server is believed to admit. The only way above it
/// is a step, a trial of one more connection, which ends rejected,
/// inconclusive or accepted; while it runs the cap is `limit + 1`.
#[derive(Debug)]
pub struct ConcurrencyControlStrategy {
    config: ConcurrencyControlConfig,
    max_connections: usize,
    limit: usize,
    /// When the running step began.
    step: Option<Instant>,
    /// When the next step may begin; any time before the first.
    due: Option<Instant>,
    /// The wait after a rejected step, doubled by each.
    wait: Duration,
    first_execute: bool,
    /// The previous step's time, to tell a late tick.
    last_tick: Option<Instant>,
}

impl ConcurrencyControlStrategy {
    pub fn new(config: ConcurrencyControlConfig, max_connections: usize) -> Self {
        let wait = config.base_wait;
        Self {
            config,
            max_connections,
            limit: max_connections,
            step: None,
            due: None,
            wait,
            first_execute: true,
            last_tick: None,
        }
    }

    #[cfg(test)]
    pub fn with_default_config(max_connections: usize) -> Self {
        Self::new(ConcurrencyControlConfig::default(), max_connections)
    }

    /// `limit + 1` while a step runs, else `limit`.
    fn limit_cap(&self) -> usize {
        self.limit + usize::from(self.step.is_some())
    }

    /// The connections the runners may take with `want` wanted:
    /// `min(limit_cap, want, max_connections)`.
    pub fn cap(&self, want: usize) -> usize {
        self.limit_cap().min(want).min(self.max_connections)
    }

    /// The connections new runners may take now, with `want` wanted: the
    /// [cap](Self::cap), except that a running step already marked rejected
    /// or unclear opens no other trial before its next tick decides it.
    /// Runners below the limit are still replaced.
    pub fn refill_cap(&self, want: usize, admission: &AdmissionEvidence) -> usize {
        if self.step.is_some() && (admission.step_rejected() || admission.step_unclear()) {
            self.limit.min(want).min(self.max_connections)
        } else {
            self.cap(want)
        }
    }

    /// The limit, the start of the running step, when the next step is due
    /// and the wait after a rejection.
    #[cfg(test)]
    pub fn state(&self) -> (usize, Option<Instant>, Option<Instant>, Duration) {
        (self.limit, self.step, self.due, self.wait)
    }

    /// The congestion rule: a 429/503 lowers the limit to the active
    /// runners, and never raises it. It rejects a running step, and the
    /// next step waits for the end of the congestion's hold.
    fn handle_congestion(
        &mut self,
        context: &mut ConcurrencyControlContext,
    ) -> Vec<StrategyAction> {
        let now = context.now;
        // Never degrade to zero: with no active runner nothing would be scheduled again.
        let lowered = self.limit.min(
            context
                .active_runners
                .max(self.config.min_concurrency)
                .max(1),
        );
        if self.step.take().is_some() {
            context.admission.end_step();
            self.wait = (self.wait * 2).min(self.config.max_wait);
        }
        if lowered < self.limit {
            self.limit = lowered;
            context.admission.on_lowered(lowered);
        }
        let hold_end = context.congestion_until.map_or(now, |until| until.max(now));
        let due = hold_end + self.wait;
        self.due = Some(self.due.map_or(due, |current| current.max(due)));
        vec![StrategyAction::ChangeMaxConcurrency(self.limit)]
    }

    /// The silent rule: the limit the evidence lowers the current one to,
    /// if any.
    ///
    /// Three corroborated refusals in a row, a frame accepted at least one
    /// tick interval after the third, some but fewer runners connected than
    /// the limit allows, and a writer that takes data: the server admits
    /// the connected runners. Nothing at a late tick, where queued events
    /// may be about to change what the evidence shows.
    fn silent_limit(&self, context: &ConcurrencyControlContext, late: bool) -> Option<usize> {
        let admission = &*context.admission;
        if late || !context.writer_ready || admission.streak_count() < REFUSALS_IN_A_ROW {
            return None;
        }
        let third = admission.third_refusal_at()?;
        if context.last_progress_at < third + self.config.tick_interval {
            return None;
        }
        let connected = admission.connected();
        (1..self.limit)
            .contains(&connected)
            .then(|| connected.max(self.config.min_concurrency).max(1))
    }

    /// Decide the running step, which began at `started_at`: rejected,
    /// inconclusive or accepted, checked in that order; `None` while it
    /// runs on. A late tick acts only on rejections and the deadline.
    fn judge_step(
        &self,
        context: &ConcurrencyControlContext,
        started_at: Instant,
        late: bool,
    ) -> Option<StepOutcome> {
        let admission = &*context.admission;
        let now = context.now;
        if admission.step_rejected() {
            return Some(StepOutcome::Rejected);
        }
        if !late && admission.step_unclear() {
            return Some(StepOutcome::Inconclusive);
        }
        if started_at
            .checked_add(self.config.step_deadline)
            .is_some_and(|deadline| now >= deadline)
        {
            return Some(StepOutcome::Inconclusive);
        }
        let occupied = admission
            .occupied_since()
            .is_some_and(|since| now.saturating_duration_since(since) >= self.config.observation);
        (!late && occupied && context.proven_runners > self.limit).then_some(StepOutcome::Accepted)
    }

    /// End the running step as `outcome`.
    fn end_step(
        &mut self,
        context: &mut ConcurrencyControlContext,
        outcome: StepOutcome,
        actions: &mut Vec<StrategyAction>,
    ) {
        let now = context.now;
        self.step = None;
        context.admission.end_step();
        bolt_load_utils::telemetry::trace!(
            "[CONTROL STRATEGY] step to {} {outcome:?}",
            self.limit + 1
        );
        match outcome {
            StepOutcome::Accepted => {
                self.limit += 1;
                self.wait = self.config.base_wait;
                self.due = Some(now);
            }
            StepOutcome::Rejected => {
                self.wait = (self.wait * 2).min(self.config.max_wait);
                self.due = Some(now + self.wait);
            }
            StepOutcome::Inconclusive => self.due = Some(now + self.wait),
        }
        // An accepted step keeps the cap: the trial's slot is the limit's.
        if outcome != StepOutcome::Accepted {
            actions.push(StrategyAction::ChangeMaxConcurrency(self.limit));
        }
    }

    /// Whether a step may begin: none runs, no congestion holds, more is
    /// wanted and allowed, it is due, and the limit's runners are
    /// connected. Not at a late tick.
    fn may_step(&self, context: &ConcurrencyControlContext, late: bool) -> bool {
        let now = context.now;
        !late
            && self.step.is_none()
            && context.congestion_until.is_none_or(|until| now >= until)
            && context.want > self.limit
            && self.limit < self.max_connections
            && self.due.is_none_or(|due| now >= due)
            && context.admission.connected() >= self.limit
    }

    /// The runners a step would create once the concurrency needs no
    /// change: one per available range, up to the free slots. Changes no
    /// state.
    ///
    /// None before the first step, which creates the first runners, nor
    /// while a congestion waits for the next step to lower the limit:
    /// runners created before it would be counted as the new limit.
    pub fn create_tasks(&self, context: &ConcurrencyControlContext) -> Vec<StrategyAction> {
        if self.first_execute || context.admission.congestion_pending() {
            return Vec::new();
        }
        self.fill_free_slots(context)
    }

    /// One runner per available range, up to the free slots:
    /// `refill_cap.saturating_sub(active)`.
    fn fill_free_slots(&self, context: &ConcurrencyControlContext) -> Vec<StrategyAction> {
        context
            .available_chunks
            .iter()
            .take(
                self.refill_cap(context.want, context.admission)
                    .saturating_sub(context.active_runners),
            )
            .map(|chunk| StrategyAction::CreateTask(chunk.clone()))
            .collect()
    }
}

/// How a step ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StepOutcome {
    /// One more connection held for the observation period, and delivers.
    Accepted,
    /// A corroborated refusal or a congestion since the step began.
    Rejected,
    /// An uncorroborated refusal, or the deadline.
    Inconclusive,
}

impl Strategy for ConcurrencyControlStrategy {
    type Context<'a>
        = ConcurrencyControlContext<'a>
    where
        Self: 'a;

    fn name() -> &'static str {
        "concurrency_control"
    }

    /// In order: the first runners; the stamp; a pending congestion; the
    /// running step's exits; without a step, the silent rule and then the
    /// start of a step; otherwise one runner per available range.
    #[inline]
    fn step(&mut self, context: &mut Self::Context<'_>) -> Vec<StrategyAction> {
        let now = context.now;
        let late = self.last_tick.replace(now).is_some_and(|last| {
            now.saturating_duration_since(last) > 2 * self.config.tick_interval
        });

        // When first launch the task,
        // Create background runners for all available chunks according to the current concurrency
        if self.first_execute {
            self.first_execute = false;
            return self.fill_free_slots(context);
        }

        // A streak with no refusal for its gap ends at the stamp.
        context.admission.stamp(now);
        context.admission.observe_proven(context.proven_runners);

        if context.admission.take_congestion() {
            bolt_load_utils::telemetry::trace!("[CONTROL STRATEGY] congestion, degraded");
            return self.handle_congestion(context);
        }

        let mut actions = Vec::new();
        if let Some(started_at) = self.step {
            if let Some(outcome) = self.judge_step(context, started_at, late) {
                self.end_step(context, outcome, &mut actions);
            }
        } else if let Some(lowered) = self.silent_limit(context, late) {
            bolt_load_utils::telemetry::trace!(
                "[CONTROL STRATEGY] refused connects: lowered to {lowered}"
            );
            self.limit = lowered;
            context.admission.on_lowered(lowered);
            self.due = Some(now + self.config.base_wait);
            actions.push(StrategyAction::ChangeMaxConcurrency(lowered));
        } else if self.may_step(context, late) {
            self.step = Some(now);
            context.admission.begin_step(self.limit + 1, now);
            actions.push(StrategyAction::ChangeMaxConcurrency(self.limit + 1));
        }

        // When the active runners are less than the max concurrency, create background runners for all available chunks
        actions.extend(self.fill_free_slots(context));
        actions
    }
}

#[cfg(test)]
mod tests;
