//! The lifecycle of one download attempt: initialize, transfer, report.

use std::{
    any::Any,
    panic::{self, AssertUnwindSafe},
    path::PathBuf,
    sync::Arc,
};

use bolt_load_utils::telemetry::*;
use futures::FutureExt;
use smol_cancellation_token::CancellationToken;

use super::{
    ConcurrentTaskInner, SingletonTaskInner,
    output_file::{self, OutputFile, Teardown},
};
use crate::{
    adapter::{AnyAdapter, BoltLoadAdapter, BoltLoadAdapterMeta},
    runtime::{LocalRuntimeBuilderImpl, ThreadedRuntimeExt, ThreadedRuntimeImpl},
    task::{
        DownloadError, DownloadMode, Progress, TaskInfo, TaskOptions,
        destination::{self, Destination},
        meta,
        status::StatusPublisher,
    },
};

/// The transfer of one attempt in its selected mode.
pub(super) trait Transfer {
    /// The progress published when the download starts, and on failure.
    fn progress(&self) -> Progress;
    /// The progress published with a successful outcome.
    fn final_progress(&self) -> Progress;
    /// Download and commit or discard the output file.
    ///
    /// Cancellation is handled inside, so when this returns every spawned
    /// worker has been joined and the output file has been committed or
    /// removed.
    async fn download(&mut self, cancel_token: &CancellationToken) -> Teardown;
}

/// What one attempt needs, moved onto its thread.
pub(crate) struct Attempt {
    pub(crate) adapter: Arc<AnyAdapter>,
    pub(crate) destination: Destination,
    pub(crate) prefer_mode: Option<DownloadMode>,
    pub(crate) options: TaskOptions,
    /// Retrieved or injected before spawning; retrieved again otherwise.
    pub(crate) meta: Option<BoltLoadAdapterMeta>,
    /// The task's own token, a child of the caller's.
    pub(crate) cancel_token: CancellationToken,
}

/// Run `attempt` on a local runtime in a blocking thread.
///
/// The attempt's future is built on that thread, so it does not need to be
/// `Send`. A panic is caught there and published as
/// [`DownloadError::Panicked`].
pub(crate) fn spawn(
    attempt: Attempt,
    rt: ThreadedRuntimeImpl,
    local_runtime_builder: Option<LocalRuntimeBuilderImpl>,
    publisher: StatusPublisher,
) -> blocking::Task<()> {
    // TODO: replace the local task executor with global runtime executor while `-Zhigher-ranked-assumptions` is stable
    // Ref:  https://github.com/rust-lang/rust/issues/100013.
    blocking::unblock(move || {
        let on_panic = publisher.share();
        let cancel_token = attempt.cancel_token.clone();
        let ran = panic::catch_unwind(AssertUnwindSafe(move || {
            match rt.downcast_local(local_runtime_builder) {
                Some(local_rt) => local_rt.block_on(run(attempt, rt, publisher)),
                None => publisher.finish(
                    Err(DownloadError::Runtime {
                        message: "the threaded runtime cannot provide a local runtime; set \
                                  `TaskBuilder::local_runtime_builder`"
                            .to_string(),
                    }),
                    Progress::default(),
                    None,
                ),
            }
        }));
        if let Err(payload) = ran {
            report_panic(&on_panic, &cancel_token, payload.as_ref());
        }
    })
}

/// Signal the workers, remove the temporary file and publish the panic, unless
/// the terminal status was already published.
#[cold]
fn report_panic(
    publisher: &StatusPublisher,
    cancel_token: &CancellationToken,
    payload: &(dyn Any + Send),
) {
    cancel_token.cancel();
    if publisher.is_finished() {
        return;
    }
    let message = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a non-string panic payload".to_string());
    error!("the download attempt panicked: {message}");
    let leftover = publisher
        .temp_file()
        .and_then(|tmp| output_file::discard_blocking(tmp));
    publisher.finish(
        Err(DownloadError::Panicked { message }),
        publisher.progress(),
        leftover,
    );
}

/// What initialization decided.
struct Initialized {
    meta: BoltLoadAdapterMeta,
    mode: DownloadMode,
    path: PathBuf,
}

/// Initialize, transfer and publish the terminal status, which comes last,
/// once the transfer has been torn down.
pub(super) async fn run(attempt: Attempt, rt: ThreadedRuntimeImpl, publisher: StatusPublisher) {
    let run = async move {
        publisher.initializing();
        let cancel_token = attempt.cancel_token.clone();
        let initialized = until_cancelled(&cancel_token, initialize(&attempt, &rt)).await;
        let Initialized { meta, mode, path } = match initialized {
            Ok(initialized) => initialized,
            Err(e) => return publisher.finish(Err(e), Progress::default(), None),
        };
        trace!("[TASK] initialized, mode: {mode:?}, path: {path:?}");
        let output = OutputFile::new(path.clone());
        let tmp = output.tmp.clone();
        let total = meta.content_size;
        let info = TaskInfo { meta, mode, path };
        let Attempt {
            adapter, options, ..
        } = attempt;
        let reporter = publisher.reporter();
        let (teardown, progress) = match mode {
            DownloadMode::Singleton => {
                // A zero size is unknown: download until the stream ends.
                let total = (total != 0).then_some(total);
                let transfer =
                    SingletonTaskInner::new(adapter, output, total, rt, options, reporter);
                drive(transfer, info, tmp, &publisher, &cancel_token).await
            }
            DownloadMode::Concurrent => {
                let transfer =
                    ConcurrentTaskInner::new(adapter, output, total, rt, options, reporter);
                drive(transfer, info, tmp, &publisher, &cancel_token).await
            }
        };
        publisher.finish(teardown.result, progress, teardown.leftover_temp_file);
    };
    #[cfg(feature = "tracing")]
    let run =
        tracing::Instrument::instrument(run, tracing::trace_span!(parent: None, "Task::attempt"));
    run.await
}

/// Publish the start of the transfer and run it; the transfer, and with it
/// every progress reporter, is dropped before this returns.
async fn drive<T: Transfer>(
    mut transfer: T,
    info: TaskInfo,
    tmp: PathBuf,
    publisher: &StatusPublisher,
    cancel_token: &CancellationToken,
) -> (Teardown, Progress) {
    publisher.downloading(info, tmp, transfer.progress());
    // Never raced against cancellation, so its teardown always completes.
    let teardown = transfer.download(cancel_token).await;
    let progress = if teardown.result.is_ok() {
        transfer.final_progress()
    } else {
        transfer.progress()
    };
    (teardown, progress)
}

/// Retrieve the metadata unless it is known, select the mode and resolve the
/// destination.
async fn initialize(
    attempt: &Attempt,
    rt: &ThreadedRuntimeImpl,
) -> Result<Initialized, DownloadError> {
    let meta = match &attempt.meta {
        Some(meta) => meta.clone(),
        None => meta::retrieve_meta(&attempt.adapter, &attempt.options.meta_retry, Some(rt))
            .await
            .map_err(|source| DownloadError::Metadata { source })?,
    };
    let mode = if attempt.prefer_mode == Some(DownloadMode::Singleton) || meta.content_size == 0 {
        DownloadMode::Singleton
    } else if attempt.adapter.is_range_stream_available().await {
        DownloadMode::Concurrent
    } else {
        DownloadMode::Singleton
    };
    let path = attempt.destination.resolve(&meta)?;
    destination::check(&path).await?;
    Ok(Initialized { meta, mode, path })
}

/// Run `fut` unless `token` is cancelled first; cancellation wins a tie.
async fn until_cancelled<T>(
    token: &CancellationToken,
    fut: impl Future<Output = Result<T, DownloadError>>,
) -> Result<T, DownloadError> {
    if token.is_cancelled() {
        return Err(DownloadError::Cancelled);
    }
    let mut fut = std::pin::pin!(fut.fuse());
    let mut cancelled = std::pin::pin!(token.cancelled().fuse());
    futures::select_biased! {
        _ = cancelled => Err(DownloadError::Cancelled),
        res = fut => res,
    }
}
