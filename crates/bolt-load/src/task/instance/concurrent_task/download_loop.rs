//! The event loop of one concurrent download attempt.

mod cancel;
mod completion;
mod deadline;
mod hedge;
mod restart;
mod retry;
mod runner;
mod strategy_tick;
#[cfg(test)]
mod tests;
mod throughput;

use std::{
    future::poll_fn,
    ops::ControlFlow,
    sync::Arc,
    task::{Context, Poll},
    time::Instant,
};

use smol_cancellation_token::CancellationToken;

use self::{
    cancel::CancelSource,
    completion::settle_completions,
    deadline::DeadlineSource,
    strategy_tick::StrategyTick,
    throughput::{RunnerRates, ThroughputSampler},
};
use super::{
    RunnerManager, RunnerSpawner,
    file_writer::{FileWriter, PendingWriter, WriteCompletion},
    runner_manager::RunnerTick,
    strategy::{Backoff, DEFAULT_STRATEGY_TICK_INTERVAL, FailurePolicy, StrategyControl},
};
use crate::{
    runtime::TimerBuilder,
    task::{
        DownloadError, Progress, TaskOptions,
        instance::{Result, sampler::DEFAULT_SAMPLE_INTERVAL},
        status::ProgressReporter,
    },
};

enum LoopEvent {
    Cancelled,
    Deadline,
    /// Writes completed; `true` when the writer could not take data
    /// before they did (design §9.6).
    WriterCompleted(Option<Vec<WriteCompletion>>, bool),
    StrategyTimer,
    ThroughputTimer,
    Runner(RunnerTick),
}

/// The event loop of one concurrent download attempt.
///
/// Every event source lives as long as the loop and is polled in place, in
/// priority order, see [`Self::poll_event`].
pub(super) struct DownloadLoop {
    spawner: RunnerSpawner,
    manager: RunnerManager,
    strategy: StrategyControl,
    policy: FailurePolicy,
    /// When failed ranges, and the task after a congestion, may connect
    /// again.
    backoff: Backoff,
    max_concurrency: usize,
    writer: PendingWriter<FileWriter>,
    reporter: ProgressReporter,
    cancel: CancelSource,
    deadline: DeadlineSource,
    strategy_tick: StrategyTick,
    throughput: ThroughputSampler,
    /// Each runner's rate at the strategy ticks, for the doubling
    /// strategy, the restarts and the hedge.
    rates: RunnerRates,
}

impl DownloadLoop {
    pub(super) fn new(
        total: u64,
        spawner: RunnerSpawner,
        file_writer: &FileWriter,
        reporter: ProgressReporter,
        options: &TaskOptions,
        cancel_token: CancellationToken,
    ) -> Self {
        let max_concurrency = options.max_connections.get();
        let manager = RunnerManager::new(total, max_concurrency);
        let strategy = StrategyControl::new(max_concurrency);
        let rates = RunnerRates::default();
        let policy = FailurePolicy::new(options, Instant::now());
        let writer = PendingWriter::new_direct(
            Arc::new(file_writer.clone()),
            spawner.rt.clone(),
            max_concurrency,
        );
        let throughput_timer = spawner.rt.create_delayed_timer(DEFAULT_SAMPLE_INTERVAL);
        let strategy_timer = spawner
            .rt
            .create_delayed_timer(DEFAULT_STRATEGY_TICK_INTERVAL);
        let deadline = DeadlineSource::new(policy.deadline());
        Self {
            spawner,
            manager,
            strategy,
            policy,
            backoff: Backoff::new(),
            max_concurrency,
            writer,
            reporter,
            cancel: CancelSource::new(cancel_token),
            deadline,
            strategy_tick: StrategyTick::new(strategy_timer.into()),
            throughput: ThroughputSampler::new(throughput_timer.into()),
            rates,
        }
    }

    pub(super) async fn run(&mut self) -> Result<()> {
        loop {
            let event = poll_fn(|cx| self.poll_event(cx)).await;
            if self.handle_event(event)?.is_break() {
                return Ok(());
            }
        }
    }

    /// Priority: cancel > deadline > writer > strategy > throughput > runner.
    ///
    /// The deadline source is the loop's one timer: it fires at the
    /// no-progress deadline, or earlier when a range's backoff or the
    /// task's congestion ends, to create runners without waiting for the
    /// strategy tick (design §8.1).
    ///
    /// Every source is polled before `Pending` is returned, so each has
    /// registered for its wake-up.
    fn poll_event(&mut self, cx: &mut Context<'_>) -> Poll<LoopEvent> {
        if let Poll::Ready(event) = self.cancel.poll(cx) {
            return Poll::Ready(event);
        }
        if let Poll::Ready(event) = self.deadline.poll(cx) {
            return Poll::Ready(event);
        }
        if let Poll::Ready(event) = self.poll_writer(cx) {
            return Poll::Ready(event);
        }
        if let Poll::Ready(event) = self.strategy_tick.poll(cx) {
            return Poll::Ready(event);
        }
        if let Poll::Ready(event) = self.throughput.poll(cx) {
            return Poll::Ready(event);
        }
        self.poll_runner(cx)
    }

    /// Handle `event`; the handlers take the time from here, so tests can
    /// pass their own.
    fn handle_event(&mut self, event: LoopEvent) -> Result<ControlFlow<()>> {
        self.handle_event_at(event, Instant::now())
    }

    /// Handle `event` at `now`.
    fn handle_event_at(&mut self, event: LoopEvent, now: Instant) -> Result<ControlFlow<()>> {
        match event {
            LoopEvent::Cancelled => Err(DownloadError::Cancelled),
            LoopEvent::Deadline => self.on_deadline(now).map(ControlFlow::Continue),
            LoopEvent::WriterCompleted(Some(completions), was_full) => self
                .record_completions(completions, was_full, now)
                .map(ControlFlow::Continue),
            LoopEvent::WriterCompleted(None, _) => Err(DownloadError::other(
                "writer completion channel closed unexpectedly",
            )),
            LoopEvent::StrategyTimer => {
                self.on_strategy_tick(now);
                Ok(ControlFlow::Continue(()))
            }
            LoopEvent::ThroughputTimer => {
                self.on_throughput_tick();
                Ok(ControlFlow::Continue(()))
            }
            LoopEvent::Runner(tick) => self.on_runner(tick, now),
        }
    }

    /// Join every spawned worker and flush the pending writes, recording the
    /// final progress. Never short-circuits; the first error takes precedence.
    pub(super) async fn finish(
        self,
        transfer_result: Result<()>,
        progress: &mut Progress,
    ) -> Result<()> {
        let Self {
            spawner,
            manager,
            writer,
            ..
        } = self;
        spawner.cancel_token.cancel();
        progress.downloaded = manager.get_total_downloaded();
        let mut written = manager.written().clone();
        // Releasing the receivers unblocks runners pushing lifecycle or data frames.
        drop(manager);
        spawner.wg.wait().await;
        // The writes still in flight count once they complete, whatever
        // ended the task (design §11.1).
        let flush_result =
            settle_completions(writer.finish().await, |range| written.insert(range)).map(drop);
        progress.downloaded_chunks = written.into_ranges();
        transfer_result.and(flush_result)
    }
}
