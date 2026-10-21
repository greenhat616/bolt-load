//! The event loop of one concurrent download attempt.

mod cancel;
mod completion;
mod deadline;
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
    cancel::CancelSource, completion::check_completions, deadline::DeadlineSource,
    strategy_tick::StrategyTick, throughput::ThroughputSampler,
};
use super::{
    DEFAULT_MAX_CONCURRENCY, RunnerManager, RunnerSpawner,
    file_writer::{FileWriter, PendingWriter, WriteCompletion},
    runner_manager::RunnerTick,
    strategy::{DEFAULT_STRATEGY_TICK_INTERVAL, FailurePolicy, StrategyControl},
};
use crate::{
    runner::TaskError,
    runtime::TimerBuilder,
    task::{
        Progress, TaskOptions,
        instance::{Result, TaskEvent, TaskInstanceError, sampler::DEFAULT_SAMPLE_INTERVAL},
    },
};

enum LoopEvent {
    Cancelled,
    Deadline,
    WriterCompleted(Option<Vec<WriteCompletion>>),
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
    max_concurrency: usize,
    writer: PendingWriter<FileWriter>,
    event_tx: async_channel::Sender<TaskEvent>,
    cancel: CancelSource,
    deadline: DeadlineSource,
    strategy_tick: StrategyTick,
    throughput: ThroughputSampler,
}

impl DownloadLoop {
    pub(super) fn new(
        total: u64,
        spawner: RunnerSpawner,
        file_writer: &FileWriter,
        event_tx: async_channel::Sender<TaskEvent>,
        options: &TaskOptions,
        cancel_token: CancellationToken,
    ) -> Self {
        let max_concurrency = std::thread::available_parallelism()
            .map(|t| t.get())
            .unwrap_or(DEFAULT_MAX_CONCURRENCY);
        #[cfg(test)]
        let max_concurrency = options.max_concurrency.unwrap_or(max_concurrency);
        let manager = RunnerManager::new(total, max_concurrency);
        let strategy = StrategyControl::new(max_concurrency);
        let policy = FailurePolicy::new(options, Instant::now());
        let writer = PendingWriter::new(
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
            max_concurrency,
            writer,
            event_tx,
            cancel: CancelSource::new(cancel_token),
            deadline,
            strategy_tick: StrategyTick::new(strategy_timer.into()),
            throughput: ThroughputSampler::new(throughput_timer.into()),
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

    fn handle_event(&mut self, event: LoopEvent) -> Result<ControlFlow<()>> {
        match event {
            LoopEvent::Cancelled => Err(TaskInstanceError::new_failed(TaskError::Cancelled)),
            LoopEvent::Deadline => self.on_deadline().map(ControlFlow::Continue),
            LoopEvent::WriterCompleted(Some(completions)) => self
                .record_completions(completions)
                .map(ControlFlow::Continue),
            LoopEvent::WriterCompleted(None) => {
                Err(TaskInstanceError::new_failed(TaskError::Other {
                    message: "writer completion channel closed unexpectedly".into(),
                }))
            }
            LoopEvent::StrategyTimer => {
                self.on_strategy_tick();
                Ok(ControlFlow::Continue(()))
            }
            LoopEvent::ThroughputTimer => {
                self.on_throughput_tick();
                Ok(ControlFlow::Continue(()))
            }
            LoopEvent::Runner(tick) => self.on_runner(tick),
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
        progress.downloaded_chunks = manager.get_downloaded_ranges();
        // Releasing the receivers unblocks runners pushing lifecycle or data frames.
        drop(manager);
        spawner.wg.wait().await;
        let flush_result = check_completions(writer.finish().await).map(drop);
        transfer_result.and(flush_result)
    }
}
