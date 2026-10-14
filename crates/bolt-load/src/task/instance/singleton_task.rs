use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_fs::{File, OpenOptions};
use async_io::Timer;
use bolt_load_utils::telemetry::*;
use futures::{AsyncWriteExt, FutureExt, StreamExt};
use smol_cancellation_token::CancellationToken;

use super::{
    Result, TaskInstance, TaskInstanceError,
    drive::{self, Transfer},
    output_file,
    sampler::{DEFAULT_SAMPLE_INTERVAL, SpeedSampler},
};
use crate::{
    adapter::{AnyAdapter, BoltLoadAdapter},
    runner::{LifecycleEvent, StoppedReason, TaskError, TaskRunner},
    runtime::{LocalRuntimeBuilderImpl, ThreadedRuntimeImpl},
    task::{
        Progress, RunnerId, TaskOptions,
        instance::{ProgressWithSpeed, TaskControl, TaskEvent},
        meta,
    },
};

/// Singleton task only have one runner, so we use a static id for the runner
const STATIC_RUNNER_ID: RunnerId = 0;

const CONTROL_CHANNEL_CAPACITY: usize = 8;

pub struct SingletonTask {
    threaded_rt: ThreadedRuntimeImpl,
    local_runtime_builder: Option<LocalRuntimeBuilderImpl>,
    task: Option<TaskControl>,
    options: TaskOptions,
}

impl SingletonTask {
    pub fn new(
        threaded_rt: ThreadedRuntimeImpl,
        local_runtime_builder: Option<LocalRuntimeBuilderImpl>,
    ) -> Self {
        Self::with_options(threaded_rt, local_runtime_builder, TaskOptions::default())
    }

    pub fn with_options(
        threaded_rt: ThreadedRuntimeImpl,
        local_runtime_builder: Option<LocalRuntimeBuilderImpl>,
        options: TaskOptions,
    ) -> Self {
        Self {
            threaded_rt,
            local_runtime_builder,
            task: None,
            options,
        }
    }
}

impl TaskInstance for SingletonTask {
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, name = "SingletonTask::run")
    )]
    fn run(
        &mut self,
        adapter: Arc<AnyAdapter>,
        path: PathBuf,
        event_tx: async_channel::Sender<TaskEvent>,
        cancel_token: CancellationToken,
    ) -> Result<()> {
        let options = self.options.clone();
        let task = drive::spawn_on_local(
            self.threaded_rt.clone(),
            self.local_runtime_builder.clone(),
            cancel_token.clone(),
            move |rt| {
                let inner = SingletonTaskInner::new(adapter, path, event_tx.clone(), rt, options);
                let task = drive::drive(inner, event_tx, cancel_token);
                #[cfg(feature = "tracing")]
                let task = tracing::Instrument::instrument(
                    task,
                    tracing::trace_span!(parent: None, "SingletonTask::background_task"),
                );
                task
            },
        );
        self.task = Some(task);
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(task) = self.task.as_mut() {
            task.stop().await;
        }
        Ok(())
    }

    async fn wait(&mut self) -> Result<()> {
        trace!(
            "[SINGLETON TASK] wait() called, task present: {}",
            self.task.is_some()
        );
        if let Some(task) = self.task.as_mut() {
            trace!("[SINGLETON TASK] waiting for task to complete");
            task.wait().await;
            trace!("[SINGLETON TASK] task completed");
        }
        Ok(())
    }
}

struct SingletonTaskInner {
    total: Option<u64>,
    downloaded: u64,
    path: PathBuf,
    adapter: Arc<AnyAdapter>,
    event_tx: async_channel::Sender<TaskEvent>,
    rt: ThreadedRuntimeImpl,
    options: TaskOptions,
}

impl SingletonTaskInner {
    fn new(
        adapter: Arc<AnyAdapter>,
        path: PathBuf,
        event_tx: async_channel::Sender<TaskEvent>,
        rt: ThreadedRuntimeImpl,
        options: TaskOptions,
    ) -> Self {
        Self {
            total: None,
            downloaded: 0,
            path,
            adapter,
            event_tx,
            rt,
            options,
        }
    }
}

impl SingletonTaskInner {
    /// Retrieve the meta data of the file
    async fn retrieve_meta(&mut self) -> Result<()> {
        let meta = meta::retrieve_meta(&self.adapter, &self.options.meta_retry, Some(&self.rt))
            .await
            .map_err(TaskInstanceError::new_retrieve_meta_failed)?;
        let total = if meta.content_size == 0 {
            None
        } else {
            Some(meta.content_size)
        };
        self.total = total;
        self.downloaded = 0;
        Ok(())
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
                StoppedReason::Failed(kind) => return Err(TaskInstanceError::new_failed(kind)),
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
            .map_err(TaskInstanceError::new_write_chunk_failed)?;
        Ok(())
    }

    /// Publish a periodic progress sample; it is dropped when the channel is full.
    fn handle_timer_tick(&self, sampler: &mut SpeedSampler, meter: &mut usize) {
        let (_, ema_speed) = sampler.sample(meter);
        let _ = self
            .event_tx
            .try_send(TaskEvent::Downloading(ProgressWithSpeed::new(
                Transfer::progress(self),
                ema_speed,
                0.0,
            )));
    }

    /// Download into the temporary file, then publish it or discard it.
    async fn download(&mut self, cancel_token: &CancellationToken) -> Result<()> {
        trace!(
            "[SINGLETON TASK] download() method called, path: {:?}",
            self.path
        );
        let tmp_path = output_file::tmp_path_for(&self.path);
        let transfer = self.download_to(&tmp_path, cancel_token).await;
        output_file::commit(transfer, &tmp_path, &self.path, cancel_token).await
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

        let stream = {
            let adapter = self.adapter.clone();
            let connect = async move { adapter.full_stream().await }.fuse();
            let cancelled = runner_token.cancelled().fuse();
            let timeout = FutureExt::fuse(Timer::after(self.options.no_progress_timeout));
            futures::pin_mut!(connect, cancelled, timeout);
            futures::select_biased! {
                _ = cancelled => {
                    return Err(TaskInstanceError::new_failed(TaskError::Cancelled));
                }
                _ = timeout => {
                    return Err(TaskInstanceError::new_failed(TaskError::NoProgress {
                        timeout: self.options.no_progress_timeout,
                    }));
                }
                res = connect => res.map_err(TaskInstanceError::new_stream_failed)?,
            }
        };
        trace!("[SINGLETON TASK] stream obtained successfully");

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(tmp_path)
            .await
            .map_err(TaskInstanceError::new_write_chunk_failed)?;

        if let Some(total) = self.total {
            file.set_len(total)
                .await
                .map_err(TaskInstanceError::new_write_chunk_failed)?;
        }

        // The runner fails with `ChannelClosed` once every control sender is
        // gone, so the sender must outlive it.
        let (control_tx, control_rx) = async_channel::bounded(CONTROL_CHANNEL_CAPACITY);

        // Use builder pattern to create runner with split channels
        let mut builder = TaskRunner::builder()
            .stream(stream)
            .runner_id(STATIC_RUNNER_ID)
            .control_signal(control_rx)
            .cancel_token(runner_token.clone())
            .idle_timeout(self.options.stream_idle_timeout);

        if let Some(total) = self.total {
            builder = builder.total(total);
        }

        let (mut runner, lifecycle_rx, data_rx) =
            builder.build().expect("failed to build task runner");

        let mut meter = 0;
        let mut sampler = SpeedSampler::new();
        let mut timer = Timer::interval(DEFAULT_SAMPLE_INTERVAL);

        // The runner is driven inline; leaving this block drops it, which stops
        // it and releases the upstream stream.
        let result = async {
            let mut is_finished = false;
            let mut runner_stopped = false;
            let mut data_done = false;
            let mut lifecycle_done = false;
            let fut = runner.run().fuse();
            futures::pin_mut!(fut);
            futures::pin_mut!(lifecycle_rx);
            futures::pin_mut!(data_rx);
            loop {
                let data_next = if data_done {
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
                futures::pin_mut!(data_next, lifecycle_next);

                futures::select_biased! {
                    _ = timer.next().fuse() => self.handle_timer_tick(&mut sampler, &mut meter),
                    _ = fut => (),
                    frame = data_next => {
                        match frame {
                            Some(frame) => {
                                self.handle_data_frame(frame.data, &mut file, &mut meter).await?;
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
                            Some(event) => {
                                match self.handle_lifecycle_event(event, &mut is_finished).await {
                                    Ok(()) => {
                                        if is_finished {
                                            runner_stopped = true;
                                            if data_done {
                                                break;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        return Err(e);
                                    }
                                }
                            }
                            None => {
                                warn!("lifecycle stream ended unexpectedly");
                                lifecycle_done = true;
                                if !runner_stopped {
                                    return Err(TaskInstanceError::new_failed(
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
        let flush = file
            .flush()
            .await
            .map_err(TaskInstanceError::new_write_chunk_failed);
        drop(file);

        result.and(flush)
    }
}

impl Transfer for SingletonTaskInner {
    async fn prepare(&mut self) -> Result<()> {
        self.retrieve_meta().await
    }

    fn progress(&self) -> Progress {
        Progress {
            total: self.total,
            downloaded: self.downloaded,
            #[allow(clippy::single_range_in_vec_init)]
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

    async fn download(&mut self, cancel_token: &CancellationToken) -> Result<()> {
        SingletonTaskInner::download(self, cancel_token).await
    }
}
