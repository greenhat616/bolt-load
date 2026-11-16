use std::{
    future::poll_fn,
    path::Path,
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, ready},
};

use bolt_load_utils::telemetry::*;
use futures::{FutureExt, StreamExt};
use smol_cancellation_token::CancellationToken;

use super::{
    Result,
    drained_file::DrainedFile,
    drive::Transfer,
    full_body::file_body,
    output_file::{OutputFile, Teardown},
    sampler::{DEFAULT_SAMPLE_INTERVAL, SpeedSampler},
};
use crate::{
    adapter::{AnyAdapter, BoltLoadAdapter},
    runner::{LifecycleEvent, StoppedReason, TaskError, TaskRunner},
    runtime::{EventTimer, ThreadedRuntimeImpl, TimerBuilder},
    task::{DownloadError, IoOp, Progress, RunnerId, Speed, TaskOptions, status::ProgressReporter},
};

/// Singleton task only have one runner, so we use a static id for the runner
const STATIC_RUNNER_ID: RunnerId = 0;

const CONTROL_CHANNEL_CAPACITY: usize = 8;

/// The sampling ticks, on the task's runtime: the immediate first tick of
/// the periodic timer is taken without sampling, so the first sample is
/// the backend's second tick, about a period after the ticks were built,
/// as the singleton's loop polls them at once. When the first poll comes
/// late, tokio keeps that target, up to its 5 ms of tolerance, and smol
/// waits a full period from that poll.
struct SampleTicks {
    timer: EventTimer,
    /// Whether the periodic timer's first tick, which is immediate, was
    /// taken.
    started: bool,
}

impl SampleTicks {
    fn new(rt: &ThreadedRuntimeImpl) -> Self {
        Self {
            timer: rt.create_delayed_timer(DEFAULT_SAMPLE_INTERVAL).into(),
            started: false,
        }
    }

    fn poll_tick(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if !self.started {
            ready!(self.timer.poll_tick(cx));
            self.started = true;
        }
        self.timer.poll_tick(cx)
    }
}

/// The transfer of the singleton mode: one runner streams the whole content.
pub(crate) struct SingletonTaskInner {
    /// `None` when the size is unknown; the stream's end finishes the download.
    /// A decoded body drops it once connected.
    total: Option<u64>,
    downloaded: u64,
    output: OutputFile,
    adapter: Arc<AnyAdapter>,
    reporter: ProgressReporter,
    options: TaskOptions,
    /// The runtime the runner's clock and the sampling are on.
    rt: ThreadedRuntimeImpl,
}

impl SingletonTaskInner {
    pub(crate) fn new(
        adapter: Arc<AnyAdapter>,
        output: OutputFile,
        total: Option<u64>,
        rt: ThreadedRuntimeImpl,
        options: TaskOptions,
        reporter: ProgressReporter,
    ) -> Self {
        Self {
            total,
            downloaded: 0,
            output,
            adapter,
            reporter,
            options,
            rt,
        }
    }
}

impl SingletonTaskInner {
    /// Create the temporary file, sized to the total when it is known. Its
    /// writes hold the attempt's drain group, so a panic handler waits for
    /// one in flight before it releases the destination (design §11.5.4).
    async fn create_file(&self, tmp_path: &Path) -> Result<DrainedFile> {
        let file = DrainedFile::create(tmp_path, self.output.drain())
            .await
            .map_err(|e| DownloadError::io(IoOp::Create, tmp_path, e))?;
        if let Some(total) = self.total {
            file.set_len(total)
                .await
                .map_err(|e| DownloadError::io(IoOp::Allocate, tmp_path, e))?;
        }
        Ok(file)
    }

    async fn handle_lifecycle_event(
        &mut self,
        event: LifecycleEvent,
        is_finished: &mut bool,
    ) -> Result<()> {
        match event {
            LifecycleEvent::Stopped(reason) => match reason {
                StoppedReason::Finished => {
                    *is_finished = true;
                    return Ok(());
                }
                StoppedReason::Failed(kind) => return Err(DownloadError::transfer(kind)),
            },
            LifecycleEvent::Started => {}
        }
        Ok(())
    }

    async fn handle_data_frame(
        &mut self,
        data: bytes::Bytes,
        file: &mut DrainedFile,
        meter: &mut usize,
    ) -> Result<()> {
        trace!("[SINGLETON TASK] downloaded chunk: {:?}", data.len());
        *meter += data.len();
        self.downloaded += data.len() as u64;
        file.write_all(data)
            .await
            .map_err(|e| DownloadError::io(IoOp::Write, &self.output.tmp, e))?;
        Ok(())
    }

    /// Publish a periodic progress sample.
    fn handle_timer_tick(&self, sampler: &mut SpeedSampler, meter: &mut usize) {
        let (_, ema_speed) = sampler.sample(meter, self.rt.now());
        if self.reporter.is_observed() {
            self.reporter
                .report(Transfer::progress(self), Speed::new(ema_speed, 0.0));
        }
    }

    /// Download into the temporary file, then commit it or discard it.
    async fn download(&mut self, cancel_token: &CancellationToken) -> Teardown {
        trace!(
            "[SINGLETON TASK] download() method called, path: {:?}",
            self.output.dest
        );
        let tmp_path = self.output.tmp.clone();
        let transfer = self.download_to(&tmp_path, cancel_token).await;
        let completed = Transfer::final_progress(self);
        self.output.commit(transfer, completed, cancel_token).await
    }

    /// Run the transfer; returns only after the runner has exited.
    async fn download_to(
        &mut self,
        tmp_path: &Path,
        cancel_token: &CancellationToken,
    ) -> Result<()> {
        // A child token: finishing or failing must never cancel the caller's token.
        let runner_token = cancel_token.child_token();
        let _runner_cancel_guard = runner_token.clone().drop_guard();

        // The runner fails with `ChannelClosed` once every control sender is
        // gone, so the sender must outlive it.
        let (control_tx, control_rx) = async_channel::bounded(CONTROL_CHANNEL_CAPACITY);
        // Set before `Started`: whether the body is decoded.
        let decoded = Arc::new(AtomicBool::new(false));
        let connect = {
            let adapter = self.adapter.clone();
            let decoded = decoded.clone();
            async move {
                let body = file_body(adapter.full_stream().await?)?;
                decoded.store(body.decoded, Ordering::Relaxed);
                Ok(body.stream)
            }
        };
        // The total reaches the runner once connected, see `Started` below.
        let (mut runner, lifecycle_rx, data_rx) = TaskRunner::builder()
            .connector(connect)
            .runner_id(STATIC_RUNNER_ID)
            .control_signal(control_rx)
            .cancel_token(runner_token.clone())
            .idle_timeout(self.options.stream_idle_timeout)
            .connect_timeout(self.options.connect_timeout)
            .timer_source(&self.rt)
            .build()
            .expect("failed to build task runner");

        let mut meter = 0;
        let mut sampler = SpeedSampler::new(self.rt.now());
        let mut ticks = SampleTicks::new(&self.rt);
        // Opened once the runner is connected; data is consumed only then.
        let mut file = None;

        // The runner is driven inline; leaving this block drops it, which stops
        // it and releases the upstream stream.
        let result = async {
            let mut is_finished = false;
            let mut runner_stopped = false;
            let mut data_done = false;
            let mut lifecycle_done = false;
            // Streaming starts once the file is ready, with the total.
            let (file_ready_tx, file_ready_rx) = oneshot::channel::<Option<u64>>();
            let mut file_ready_tx = Some(file_ready_tx);
            let mut fut = pin!(
                runner
                    .run_after_started(async move { file_ready_rx.await.ok().flatten() })
                    .fuse()
            );
            let mut lifecycle_rx = pin!(lifecycle_rx);
            let mut data_rx = pin!(data_rx);
            loop {
                let data_next = if data_done || file.is_none() {
                    futures::future::Either::Left(futures::future::pending())
                } else {
                    futures::future::Either::Right(data_rx.next())
                }
                .fuse();
                let lifecycle_next = if lifecycle_done {
                    futures::future::Either::Left(futures::future::pending())
                } else {
                    futures::future::Either::Right(lifecycle_rx.next())
                }
                .fuse();
                let mut data_next = pin!(data_next);
                let mut lifecycle_next = pin!(lifecycle_next);

                futures::select_biased! {
                    _ = poll_fn(|cx| ticks.poll_tick(cx)).fuse() => {
                        if file.is_some() {
                            self.handle_timer_tick(&mut sampler, &mut meter);
                        }
                    }
                    _ = fut => (),
                    frame = data_next => {
                        match frame {
                            Some(frame) => {
                                let file = file.as_mut().expect("data is consumed once connected");
                                self.handle_data_frame(frame.data, file, &mut meter).await?;
                            }
                            None => {
                                data_done = true;
                                if runner_stopped {
                                    break;
                                }
                            }
                        }
                    }
                    event = lifecycle_next => {
                        match event {
                            Some(LifecycleEvent::Started) => {
                                trace!("[SINGLETON TASK] stream obtained successfully");
                                if decoded.load(Ordering::Relaxed) {
                                    // The size counts the coded bytes.
                                    debug!("[SINGLETON TASK] decoding the body, size unknown");
                                    self.total = None;
                                }
                                file = Some(self.create_file(tmp_path).await?);
                                // Measure the speed from the first byte on.
                                sampler = SpeedSampler::new(self.rt.now());
                                ticks = SampleTicks::new(&self.rt);
                                if let Some(file_ready_tx) = file_ready_tx.take() {
                                    let _ = file_ready_tx.send(self.total);
                                }
                            }
                            Some(LifecycleEvent::Stopped(reason)) if file.is_none() => {
                                return Err(connect_error(reason));
                            }
                            Some(event) => {
                                self.handle_lifecycle_event(event, &mut is_finished).await?;
                                if is_finished {
                                    runner_stopped = true;
                                    if data_done {
                                        break;
                                    }
                                }
                            }
                            None => {
                                warn!("lifecycle stream ended unexpectedly");
                                lifecycle_done = true;
                                if !runner_stopped {
                                    return Err(DownloadError::transfer(
                                        TaskError::ChannelClosed,
                                    ));
                                }
                                if data_done {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            Ok(())
        }
        .await;

        // ---- teardown: never short-circuit ----
        runner_token.cancel();
        drop(runner);
        drop(control_tx);
        // Every write was awaited until it reached the file: nothing is
        // buffered to flush.
        drop(file);

        result
    }
}

impl Transfer for SingletonTaskInner {
    fn progress(&self) -> Progress {
        Progress {
            total: self.total,
            downloaded: self.downloaded,
            #[expect(
                clippy::single_range_in_vec_init,
                reason = "a list holding one downloaded range"
            )]
            downloaded_chunks: vec![0..self.downloaded],
        }
    }

    /// An unknown total is reported as the downloaded size once finished.
    fn final_progress(&self) -> Progress {
        Progress {
            total: Some(self.total.unwrap_or(self.downloaded)),
            ..Transfer::progress(self)
        }
    }

    async fn download(&mut self, cancel_token: &CancellationToken) -> Teardown {
        SingletonTaskInner::download(self, cancel_token).await
    }
}

/// The error of a runner that stopped before it connected; a failed
/// connection keeps the adapter's error.
#[cold]
fn connect_error(reason: StoppedReason) -> DownloadError {
    match reason {
        StoppedReason::Failed(error) => DownloadError::transfer(error),
        StoppedReason::Finished => DownloadError::other("the runner finished before it connected"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::runtime::test_support::{ManualClock, ManualRuntime};

    const NS: Duration = Duration::from_nanos(1);

    /// On the runtime's clock, a sample is due a full period after the
    /// ticks start, then every period; so after a restart, as at
    /// `Started`.
    #[test]
    fn test_samples_a_period_after_the_start() {
        let clock = ManualClock::new();
        let rt = ThreadedRuntimeImpl::new_other_rt(ManualRuntime {
            clock: clock.clone(),
        });
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        for start in ["the start", "a restart"] {
            let mut ticks = SampleTicks::new(&rt);
            assert!(
                ticks.poll_tick(&mut cx).is_pending(),
                "{start}: none at once"
            );
            clock.advance(DEFAULT_SAMPLE_INTERVAL - NS);
            assert!(
                ticks.poll_tick(&mut cx).is_pending(),
                "{start}: a nanosecond early"
            );
            clock.advance(NS);
            assert!(
                ticks.poll_tick(&mut cx).is_ready(),
                "{start}: a period after"
            );
            assert!(ticks.poll_tick(&mut cx).is_pending());
            clock.advance(DEFAULT_SAMPLE_INTERVAL);
            assert!(ticks.poll_tick(&mut cx).is_ready(), "{start}: every period");
            // Time passes before the restart.
            clock.advance(DEFAULT_SAMPLE_INTERVAL / 3);
        }
    }

    /// The same on tokio's ticks, on the paused clock of the task's
    /// runtime.
    #[cfg(feature = "tokio")]
    #[test]
    fn test_samples_a_period_after_the_start_on_tokio() {
        let paused = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        let rt = ThreadedRuntimeImpl::Tokio(crate::runtime::TokioThreadedRuntime::Handle(
            paused.handle().clone(),
        ));
        let mut ticks = SampleTicks::new(&rt);
        let within = |ticks: &mut SampleTicks, within| {
            paused.block_on(async {
                tokio::time::timeout(within, poll_fn(|cx| ticks.poll_tick(cx)))
                    .await
                    .is_ok()
            })
        };
        let ms = Duration::from_millis(1);
        assert!(
            !within(&mut ticks, DEFAULT_SAMPLE_INTERVAL - ms),
            "none before a period"
        );
        assert!(within(&mut ticks, 2 * ms), "a period after the start");
        assert!(!within(&mut ticks, DEFAULT_SAMPLE_INTERVAL - ms));
        assert!(within(&mut ticks, 2 * ms), "every period");
    }
}
