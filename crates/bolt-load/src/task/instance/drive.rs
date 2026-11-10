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
    destination_lock::{DestinationLock, LockHooks, Unsupported},
    output_file::{self, OutputFile, Teardown},
    ownership::Ownership,
};
use crate::{
    adapter::{AnyAdapter, BoltLoadAdapter, BoltLoadAdapterMeta},
    runtime::{LocalRuntimeBuilderImpl, ThreadedRuntimeExt, ThreadedRuntimeImpl},
    task::{
        DownloadError, DownloadMode, FallbackReason, IoOp, Progress, TaskInfo, TaskOptions,
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
    /// How the destination's lock is taken; replaced in tests.
    pub(crate) lock_hooks: LockHooks,
}

/// Run `attempt` on a local runtime in a blocking thread.
///
/// The attempt's future is built on that thread, so it does not need to be
/// `Send`. A panic is caught there and published as
/// [`DownloadError::Panicked`], after the cleanup its phase requires; the
/// state that cleanup needs lives here, outside the unwind boundary.
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
        let ownership = Ownership::new();
        let owned = ownership.clone();
        let ran = panic::catch_unwind(AssertUnwindSafe(move || {
            match rt.downcast_local(local_runtime_builder) {
                Some(local_rt) => local_rt.block_on(run(attempt, rt, publisher, owned)),
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
            report_panic(&on_panic, &cancel_token, &ownership, payload.as_ref());
        }
    })
}

/// Cancel the workers and wait until those that hold the temporary file
/// have exited, clean up as the attempt's phase requires, release the
/// destination's lock, and publish the outcome, unless the terminal status
/// was already published (task-api-spec TASK-8). Once the temporary file
/// was renamed to the destination, the task finished.
#[cold]
fn report_panic(
    publisher: &StatusPublisher,
    cancel_token: &CancellationToken,
    ownership: &Ownership,
    payload: &(dyn Any + Send),
) {
    cancel_token.cancel();
    let message = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a non-string panic payload".to_string());
    error!("the download attempt panicked: {message}");
    let cleanup = ownership.clean_up_after_panic();
    debug!("cleaned up after the panic, in phase {:?}", cleanup.phase);
    if publisher.is_finished() {
        return;
    }
    match cleanup.published {
        Some(completed) => publisher.finish(Ok(()), completed, None),
        None => publisher.finish(
            Err(DownloadError::Panicked { message }),
            publisher.progress(),
            cleanup.leftover_temp_file,
        ),
    }
}

/// What initialization decided.
struct Initialized {
    meta: BoltLoadAdapterMeta,
    mode: DownloadMode,
    fallback: Option<FallbackReason>,
    path: PathBuf,
}

/// Initialize, transfer and publish the terminal status, which comes last,
/// once the transfer has been torn down and the destination's lock
/// released.
pub(super) async fn run(
    attempt: Attempt,
    rt: ThreadedRuntimeImpl,
    publisher: StatusPublisher,
    ownership: Arc<Ownership>,
) {
    let run = async move {
        publisher.initializing();
        let cancel_token = attempt.cancel_token.clone();
        let initialized = until_cancelled(&cancel_token, initialize(&attempt, &rt)).await;
        let Initialized {
            meta,
            mode,
            fallback,
            path,
        } = match initialized {
            Ok(initialized) => initialized,
            Err(e) => return publisher.finish(Err(e), Progress::default(), None),
        };
        trace!("[TASK] initialized, mode: {mode:?}, fallback: {fallback:?}, path: {path:?}");
        let output = OutputFile::new(path.clone(), ownership.clone());
        if let Err(e) = lock_destination(&path, &output, &ownership, &attempt.lock_hooks).await {
            ownership.release();
            return publisher.finish(Err(e), Progress::default(), None);
        }
        // Observed only between steps: the lock is never left to a dropped
        // future (design §11.5.4). The task owns the temporary file now, so
        // it removes one an earlier task left (design §11.5.9, phase D).
        if cancel_token.is_cancelled() {
            let leftover = output_file::discard(&output.tmp).await;
            ownership.release();
            return publisher.finish(Err(DownloadError::Cancelled), Progress::default(), leftover);
        }
        let total = meta.content_size;
        // A zero size is unknown, and a coded size is not the file's: download
        // until the stream ends.
        let singleton_total = (total != 0 && meta.content_encoding.is_none()).then_some(total);
        let info = TaskInfo {
            meta,
            mode,
            path,
            fallback,
        };
        let Attempt {
            adapter, options, ..
        } = attempt;
        let (teardown, progress) = match mode {
            DownloadMode::Singleton => {
                let transfer = SingletonTaskInner::new(
                    adapter,
                    output,
                    singleton_total,
                    rt,
                    options,
                    publisher.reporter(),
                );
                drive(transfer, info, &publisher, &cancel_token).await
            }
            DownloadMode::Concurrent => {
                let transfer = ConcurrentTaskInner::new(
                    adapter.clone(),
                    output.clone(),
                    total,
                    rt.clone(),
                    options.clone(),
                    publisher.reporter(),
                );
                let (teardown, progress) =
                    drive(transfer, info.clone(), &publisher, &cancel_token).await;
                let Err(DownloadError::RangeUnsupported { reason, source }) = &teardown.result
                else {
                    ownership.release();
                    return publisher.finish(
                        teardown.result,
                        progress,
                        teardown.leftover_temp_file,
                    );
                };
                // The concurrent teardown removed what it wrote; the
                // singleton truncates the file again when it starts.
                warn!("[TASK] falling back to one connection, {reason}: {source}");
                let info = TaskInfo {
                    mode: DownloadMode::Singleton,
                    fallback: Some(*reason),
                    ..info
                };
                let transfer = SingletonTaskInner::new(
                    adapter,
                    output,
                    None,
                    rt,
                    options,
                    publisher.reporter(),
                );
                drive(transfer, info, &publisher, &cancel_token).await
            }
        };
        // After the last operation on the temporary file.
        ownership.release();
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
    publisher: &StatusPublisher,
    cancel_token: &CancellationToken,
) -> (Teardown, Progress) {
    publisher.downloading(info, transfer.progress());
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
    let (mode, fallback) = if attempt.prefer_mode == Some(DownloadMode::Singleton) {
        (DownloadMode::Singleton, None)
    } else if meta.content_encoding.is_some() {
        // Ranges of a coded response count encoded bytes.
        (DownloadMode::Singleton, Some(FallbackReason::ContentCoded))
    } else if meta.content_size != 0 && attempt.adapter.is_range_stream_available().await {
        (DownloadMode::Concurrent, None)
    } else {
        (DownloadMode::Singleton, None)
    };
    let path = attempt.destination.resolve(&meta)?;
    destination::check(&path).await?;
    Ok(Initialized {
        meta,
        mode,
        fallback,
        path,
    })
}

/// Take the destination's lock and own the temporary file, then invalidate
/// any state an earlier task left, before the temporary file is touched
/// (design §11.5.4, §11.5.5). Not raced against cancellation: the lock is
/// registered with `ownership` before any file is changed, so it is always
/// released.
async fn lock_destination(
    dest: &std::path::Path,
    output: &OutputFile,
    ownership: &Arc<Ownership>,
    hooks: &LockHooks,
) -> Result<(), DownloadError> {
    let lock = blocking::unblock({
        let dest = dest.to_path_buf();
        let hooks = hooks.clone();
        move || DestinationLock::acquire(&dest, Unsupported::Proceed, &hooks)
    })
    .await?;
    ownership.own(lock, output.tmp.clone());
    let invalidated = blocking::unblock({
        let ownership = ownership.clone();
        move || ownership.with_lock(DestinationLock::invalidate)
    })
    .await;
    match invalidated {
        Some(Err(source)) => Err(DownloadError::Io {
            op: IoOp::Lock,
            path: super::destination_lock::state_path_for(dest),
            source: Arc::new(source),
        }),
        _ => Ok(()),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{
        TaskState,
        instance::destination_lock::{LockHooks, state_path_for},
    };

    fn publisher_downloading(
        path: &std::path::Path,
    ) -> (StatusPublisher, crate::task::StatusReceiver) {
        let (publisher, rx) = StatusPublisher::new();
        publisher.initializing();
        publisher.downloading(
            TaskInfo {
                meta: BoltLoadAdapterMeta::new(10),
                mode: DownloadMode::Singleton,
                path: path.to_path_buf(),
                fallback: None,
            },
            Progress::default(),
        );
        (publisher, rx)
    }

    fn owned(dest: &std::path::Path, tmp: &std::path::Path) -> Arc<Ownership> {
        let ownership = Ownership::new();
        let lock =
            DestinationLock::acquire(dest, Unsupported::Proceed, &LockHooks::default()).unwrap();
        ownership.own(lock, tmp.to_path_buf());
        ownership
    }

    /// TASK-8: a panic before the rename fails the task, removes the
    /// temporary file and releases the lock.
    #[test]
    fn test_a_panic_while_downloading_fails_the_task() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("file.bin");
        let tmp = dir.path().join("file.bin.downloading");
        std::fs::write(&tmp, b"partial").unwrap();
        let ownership = owned(&dest, &tmp);
        let (publisher, rx) = publisher_downloading(&dest);
        let token = CancellationToken::new();
        report_panic(&publisher, &token, &ownership, &"boom");
        let status = rx.borrow();
        assert_eq!(status.state, TaskState::Failed);
        assert!(matches!(status.error, Some(DownloadError::Panicked { .. })));
        assert!(token.is_cancelled());
        assert!(!tmp.exists() && !state_path_for(&dest).exists());
    }

    /// TASK-8: once the temporary file was renamed, a panic leaves the
    /// destination as it is, and the task finished with the progress it
    /// completed, not the last sample.
    #[test]
    #[expect(clippy::single_range_in_vec_init, reason = "a list holding one range")]
    fn test_a_panic_after_the_rename_finishes_the_task() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("file.bin");
        let tmp = dir.path().join("file.bin.downloading");
        std::fs::write(&dest, b"complete").unwrap();
        let ownership = owned(&dest, &tmp);
        let completed = Progress::new(Some(8), 8, vec![0..8]);
        ownership.publish(completed.clone());
        assert_eq!(ownership.phase(), super::super::ownership::Phase::Published);
        let (publisher, rx) = publisher_downloading(&dest);
        report_panic(&publisher, &CancellationToken::new(), &ownership, &"boom");
        let status = rx.borrow();
        assert_eq!(status.state, TaskState::Finished);
        assert!(status.error.is_none());
        assert_eq!(status.progress, completed);
        assert_eq!(std::fs::read(&dest).unwrap(), b"complete");
        assert!(!state_path_for(&dest).exists());
    }
}
