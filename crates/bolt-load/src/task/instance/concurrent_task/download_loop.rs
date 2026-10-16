//! The event loop of one concurrent download attempt.

mod completion;
mod deadline;
mod runner;
mod strategy_tick;
#[cfg(test)]
mod tests;
mod throughput;

use std::{sync::Arc, time::Instant};

use futures::{FutureExt, future::Either};
use smol_cancellation_token::CancellationToken;

use self::{completion::check_completions, throughput::Throughput};
use super::{
    DEFAULT_MAX_CONCURRENCY, RunnerManager, RunnerSpawner,
    file_writer::{FileWriter, PendingWriter, WriteCompletion},
    runner_manager::RunnerTick,
    strategy::{DEFAULT_STRATEGY_TICK_INTERVAL, FailurePolicy, StrategyControl},
};
use crate::{
    runner::TaskError,
    runtime::{Timer, TimerBuilder, TimerImpl},
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
pub(super) struct DownloadLoop {
    spawner: RunnerSpawner,
    manager: RunnerManager,
    strategy: StrategyControl,
    policy: FailurePolicy,
    max_concurrency: usize,
    throughput: Throughput,
    writer: PendingWriter<FileWriter>,
    event_tx: async_channel::Sender<TaskEvent>,
    throughput_timer: TimerImpl,
    strategy_timer: TimerImpl,
    /// Woken at the no-progress deadline; re-armed lazily when progress moved it.
    deadline_timer: async_io::Timer,
}

impl DownloadLoop {
    pub(super) fn new(
        total: u64,
        spawner: RunnerSpawner,
        file_writer: &FileWriter,
        event_tx: async_channel::Sender<TaskEvent>,
        options: &TaskOptions,
    ) -> Self {
        let max_concurrency = std::thread::available_parallelism()
            .map(|t| t.get())
            .unwrap_or(DEFAULT_MAX_CONCURRENCY);
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
        let deadline_timer = async_io::Timer::at(policy.deadline());
        Self {
            spawner,
            manager,
            strategy,
            policy,
            max_concurrency,
            throughput: Throughput::new(),
            writer,
            event_tx,
            throughput_timer,
            strategy_timer,
            deadline_timer,
        }
    }

    pub(super) async fn run(&mut self, cancel_token: &CancellationToken) -> Result<()> {
        loop {
            let completions = self.writer.try_tick();
            self.record_completions(completions)?;

            match self.next_event(cancel_token).await {
                LoopEvent::Cancelled => {
                    return Err(TaskInstanceError::new_failed(TaskError::Cancelled));
                }
                LoopEvent::Deadline => self.on_deadline()?,
                LoopEvent::WriterCompleted(Some(completions)) => {
                    self.record_completions(completions)?;
                }
                LoopEvent::WriterCompleted(None) => {
                    return Err(TaskInstanceError::new_failed(TaskError::Other {
                        message: "writer completion channel closed unexpectedly".into(),
                    }));
                }
                LoopEvent::StrategyTimer => self.on_strategy_tick(),
                LoopEvent::ThroughputTimer => self.on_throughput_tick(),
                LoopEvent::Runner(tick) => {
                    if self.on_runner(tick)?.is_break() {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Priority: cancel > deadline > writer > strategy > throughput > runner.
    async fn next_event(&mut self, cancel_token: &CancellationToken) -> LoopEvent {
        let Self {
            manager,
            writer,
            throughput_timer,
            strategy_timer,
            deadline_timer,
            ..
        } = self;
        let can_write = writer.can_write();
        let writer_fut = if writer.is_idle() {
            Either::Left(futures::future::pending::<Option<Vec<WriteCompletion>>>())
        } else {
            Either::Right(writer.tick())
        }
        .fuse();
        let runner_fut = if can_write {
            Either::Left(manager.tick())
        } else {
            Either::Right(manager.tick_lifecycle_only())
        }
        .fuse();
        let cancelled = cancel_token.cancelled().fuse();
        let deadline = FutureExt::fuse(deadline_timer);
        let strategy_tick = strategy_timer.tick().fuse();
        let throughput_tick = throughput_timer.tick().fuse();

        futures::pin_mut!(
            cancelled,
            deadline,
            writer_fut,
            runner_fut,
            strategy_tick,
            throughput_tick
        );

        futures::select_biased! {
            _ = cancelled => LoopEvent::Cancelled,
            _ = deadline => LoopEvent::Deadline,
            completions = writer_fut => LoopEvent::WriterCompleted(completions),
            _ = strategy_tick => LoopEvent::StrategyTimer,
            _ = throughput_tick => LoopEvent::ThroughputTimer,
            tick = runner_fut => LoopEvent::Runner(tick),
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
