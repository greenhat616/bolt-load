use std::{path::Path, pin::pin, sync::Arc};

use async_fs::{File, OpenOptions};
use async_io::Timer;
use bolt_load_utils::telemetry::*;
use futures::{AsyncWriteExt, FutureExt, StreamExt};
use smol_cancellation_token::CancellationToken;

use super::{
    Result,
    drive::Transfer,
    output_file::{OutputFile, Teardown},
    sampler::{DEFAULT_SAMPLE_INTERVAL, SpeedSampler},
};
use crate::{
    adapter::{AnyAdapter, BoltLoadAdapter},
    runner::{LifecycleEvent, StoppedReason, TaskError, TaskRunner},
    runtime::ThreadedRuntimeImpl,
    task::{DownloadError, IoOp, Progress, RunnerId, Speed, TaskOptions, status::ProgressReporter},
};

/// Singleton task only have one runner, so we use a static id for the runner
const STATIC_RUNNER_ID: RunnerId = 0;

const CONTROL_CHANNEL_CAPACITY: usize = 8;

/// The transfer of the singleton mode: one runner streams the whole content.
pub(crate) struct SingletonTaskInner {
    /// `None` when the size is unknown; the stream's end finishes the download.
    total: Option<u64>,
    downloaded: u64,
    output: OutputFile,
    adapter: Arc<AnyAdapter>,
    reporter: ProgressReporter,
    options: TaskOptions,
}

impl SingletonTaskInner {
    pub(crate) fn new(
        adapter: Arc<AnyAdapter>,
        output: OutputFile,
        total: Option<u64>,
        _rt: ThreadedRuntimeImpl,
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
        }
    }
}

impl SingletonTaskInner {
    /// Create the temporary file, sized to the total when it is known.
    async fn create_file(&self, tmp_path: &Path) -> Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(tmp_path)
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
        file: &mut File,
        meter: &mut usize,
    ) -> Result<()> {
        trace!("[SINGLETON TASK] downloaded chunk: {:?}", data.len());
        *meter += data.len();
        self.downloaded += data.len() as u64;
        file.write_all(&data)
            .await
            .map_err(|e| DownloadError::io(IoOp::Write, &self.output.tmp, e))?;
        Ok(())
    }

    /// Publish a periodic progress sample.
    fn handle_timer_tick(&self, sampler: &mut SpeedSampler, meter: &mut usize) {
        let (_, ema_speed) = sampler.sample(meter);
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
        self.output.commit(transfer, cancel_token).await
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
        let adapter = self.adapter.clone();
        let mut builder = TaskRunner::builder()
            .connector(async move { adapter.full_stream().await })
            .runner_id(STATIC_RUNNER_ID)
            .control_signal(control_rx)
            .cancel_token(runner_token.clone())
            .idle_timeout(self.options.stream_idle_timeout)
            .connect_timeout(self.options.connect_timeout);
        if let Some(total) = self.total {
            builder = builder.total(total);
        }
        let (mut runner, lifecycle_rx, data_rx) =
            builder.build().expect("failed to build task runner");

        let mut meter = 0;
        let mut sampler = SpeedSampler::new();
        let mut timer = Timer::interval(DEFAULT_SAMPLE_INTERVAL);
        // Opened once the runner is connected; data is consumed only then.
        let mut file = None;

        // The runner is driven inline; leaving this block drops it, which stops
        // it and releases the upstream stream.
        let result = async {
            let mut is_finished = false;
            let mut runner_stopped = false;
            let mut data_done = false;
            let mut lifecycle_done = false;
            // Streaming starts once the file is ready.
            let (file_ready_tx, file_ready_rx) = oneshot::channel::<()>();
            let mut file_ready_tx = Some(file_ready_tx);
            let mut fut = pin!(
                runner
                    .run_after_started(async move {
                        let _ = file_ready_rx.await;
                    })
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
                    _ = timer.next().fuse() => {
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
                                file = Some(self.create_file(tmp_path).await?);
                                // Measure the speed from the first byte on.
                                sampler = SpeedSampler::new();
                                timer = Timer::interval(DEFAULT_SAMPLE_INTERVAL);
                                if let Some(file_ready_tx) = file_ready_tx.take() {
                                    let _ = file_ready_tx.send(());
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
        let flush = match file.as_mut() {
            Some(file) => file
                .flush()
                .await
                .map_err(|e| DownloadError::io(IoOp::Flush, tmp_path, e)),
            None => Ok(()),
        };
        drop(file);

        result.and(flush)
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
