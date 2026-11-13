//! The lifecycle of one download attempt: initialize, transfer, report.

use std::{
    any::Any,
    ops::Range,
    panic::{self, AssertUnwindSafe},
    path::PathBuf,
    sync::Arc,
};

use bolt_load_utils::telemetry::*;
use futures::FutureExt;
use smol_cancellation_token::CancellationToken;

use super::{
    ConcurrentTaskInner, SingletonTaskInner,
    concurrent_task::checkpoint::{self, BlockReader},
    destination_lock::{DestinationLock, LockHooks, Unsupported},
    file_steps::{FileHooks, Step},
    output_file::{self, OutputFile, Teardown},
    ownership::{Ownership, Phase},
    resume_check::{VerifyError, compare, report, verify},
    state_file::{
        Header, StateFile, encode_header,
        read::{ReadError, Scanned, read_full, read_state},
        sync_parent_dir,
    },
};
use crate::{
    adapter::{AnyAdapter, BoltLoadAdapter, BoltLoadAdapterMeta},
    runtime::{LocalRuntimeBuilderImpl, ThreadedRuntimeExt, ThreadedRuntimeImpl},
    task::{
        DownloadError, DownloadMode, FallbackReason, IoOp, Progress, RejectReason, ResumeReport,
        TaskInfo, TaskOptions,
        builder::Resume,
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
    /// Whether the task keeps a state file for a later resume.
    pub(crate) resume: Resume,
    /// What to call before each file step of a resumable task.
    pub(crate) file_hooks: FileHooks,
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
        let ownership = Ownership::with_hooks(attempt.file_hooks.clone());
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
        None => publisher.finish_keeping(
            Err(DownloadError::Panicked { message }),
            publisher.progress(),
            cleanup.leftover_temp_file,
            cleanup.kept_for_resume,
        ),
    }
}

/// What initialization decided.
struct Initialized {
    meta: BoltLoadAdapterMeta,
    mode: DownloadMode,
    fallback: Option<FallbackReason>,
    path: PathBuf,
    /// A resumable task that preferred one connection: the concurrent
    /// engine runs it with one (design §11.5.8).
    one_connection: bool,
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
            one_connection,
        } = match initialized {
            Ok(initialized) => initialized,
            Err(e) => return publisher.finish(Err(e), Progress::default(), None),
        };
        trace!("[TASK] initialized, mode: {mode:?}, fallback: {fallback:?}, path: {path:?}");
        let output = OutputFile::new(path.clone(), ownership.clone());
        let locked = match lock_destination(
            &path,
            &output,
            &ownership,
            &attempt.lock_hooks,
            attempt.resume,
        )
        .await
        {
            Ok(locked) => locked,
            Err(e) => {
                ownership.release();
                return publisher.finish(Err(e), Progress::default(), None);
            }
        };
        // Observed only between steps: the lock is never left to a dropped
        // future (design §11.5.4). A task that found a state to resume from
        // leaves both files (design §11.5.9, B2); any other owns the
        // temporary file now, so it removes one an earlier task left
        // (phase D).
        if cancel_token.is_cancelled() {
            let leftover = match locked {
                Locked::ToResume => {
                    ownership.keep_files();
                    None
                }
                Locked::Fresh => output_file::discard(&output.tmp).await,
            };
            ownership.release();
            return publisher.finish(Err(DownloadError::Cancelled), Progress::default(), leftover);
        }
        let (meta, mode, fallback, one_connection, state, resumed) = match locked {
            // Continue from the state file, once it was checked (design
            // §11.5.10).
            Locked::ToResume => {
                match resume_checked(&attempt, &rt, &output, &ownership, &cancel_token).await {
                    Ok(Resumed {
                        meta,
                        state,
                        report,
                        kept,
                    }) => (
                        meta,
                        DownloadMode::Concurrent,
                        None,
                        attempt.prefer_mode == Some(DownloadMode::Singleton),
                        Some(state),
                        Some((report, kept)),
                    ),
                    Err(e) => {
                        let (leftover, kept) = end_failed_start(&output, &ownership).await;
                        ownership.release();
                        return publisher.finish_keeping(
                            Err(e),
                            Progress::default(),
                            leftover,
                            kept,
                        );
                    }
                }
            }
            // A resumable new start creates the partial file and writes the
            // state header before the transfer starts (design §11.5.5).
            Locked::Fresh => {
                let state = match resumable_header(attempt.resume, mode, &meta) {
                    None => None,
                    Some(header) => match start_resumable(
                        &output,
                        &ownership,
                        meta.content_size,
                        header,
                        &cancel_token,
                    )
                    .await
                    {
                        Ok(state) => Some(state),
                        Err(e) => {
                            let (leftover, kept) = end_failed_start(&output, &ownership).await;
                            ownership.release();
                            return publisher.finish_keeping(
                                Err(e),
                                Progress::default(),
                                leftover,
                                kept,
                            );
                        }
                    },
                };
                (meta, mode, fallback, one_connection, state, None)
            }
        };
        let total = meta.content_size;
        let (resumed, kept) = resumed.unzip();
        // A zero size is unknown, and a coded size is not the file's: download
        // until the stream ends.
        let singleton_total = (total != 0 && meta.content_encoding.is_none()).then_some(total);
        let info = TaskInfo {
            meta,
            mode,
            path,
            fallback,
            resumable: state.is_some(),
            resumed,
        };
        let Attempt {
            adapter, options, ..
        } = attempt;
        let options = transfer_options(options, one_connection);
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
                let mut transfer = ConcurrentTaskInner::new(
                    adapter.clone(),
                    output.clone(),
                    total,
                    rt.clone(),
                    options.clone(),
                    publisher.reporter(),
                );
                if let Some(state) = state {
                    transfer = transfer.checkpoint_to(Box::new(state));
                }
                if let Some(kept) = kept {
                    transfer = transfer.resume_from(kept);
                }
                // What a fallback publishes, built before the info moves:
                // a resume's report is not copied (design §14.30.1).
                let fallback_info = TaskInfo {
                    meta: info.meta.clone(),
                    mode: DownloadMode::Singleton,
                    path: info.path.clone(),
                    fallback: None,
                    resumable: false,
                    resumed: None,
                };
                let (teardown, progress) = drive(transfer, info, &publisher, &cancel_token).await;
                let Err(DownloadError::RangeUnsupported { reason, source }) = &teardown.result
                else {
                    ownership.release();
                    return publisher.finish_keeping(
                        teardown.result,
                        progress,
                        teardown.leftover_temp_file,
                        teardown.kept_for_resume,
                    );
                };
                // The concurrent teardown removed what it wrote; the
                // singleton truncates the file again when it starts.
                warn!("[TASK] falling back to one connection, {reason}: {source}");
                // What a resume kept went with the state file.
                let info = TaskInfo {
                    fallback: Some(*reason),
                    ..fallback_info
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
        publisher.finish_keeping(
            teardown.result,
            progress,
            teardown.leftover_temp_file,
            teardown.kept_for_resume,
        );
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
    let mut one_connection = false;
    let (mode, fallback) = if attempt.prefer_mode == Some(DownloadMode::Singleton) {
        // A task that may resume and could keep a state file over ranges
        // takes them, with one connection (design §11.5.8); any other
        // stays with one connection's full download, not resumable.
        if attempt.resume == Resume::On
            && resumable_header(attempt.resume, DownloadMode::Concurrent, &meta).is_some()
            && meta.content_size != 0
            && meta.content_encoding.is_none()
            && attempt.adapter.is_range_stream_available().await
        {
            one_connection = true;
            (DownloadMode::Concurrent, None)
        } else {
            (DownloadMode::Singleton, None)
        }
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
        one_connection,
    })
}

/// What the destination's lock found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Locked {
    /// No state to resume from: the task starts anew.
    Fresh,
    /// A state file an earlier task left, to resume from (B2).
    ToResume,
}

/// Take the destination's lock (design §11.5.4); a task that may resume
/// needs it. With `Resume::On`, a state file an earlier task left, not
/// created by this task and not empty, is resumed from: both files are
/// left as they are (design §11.5.2, §11.5.10 step 0). Otherwise own the
/// temporary file, then invalidate any state an earlier task left, before
/// the temporary file is touched (design §11.5.5). Not raced against
/// cancellation: the lock is registered with `ownership` before any file
/// is changed, so it is always released.
async fn lock_destination(
    dest: &std::path::Path,
    output: &OutputFile,
    ownership: &Arc<Ownership>,
    hooks: &LockHooks,
    resume: Resume,
) -> Result<Locked, DownloadError> {
    let unsupported = match resume {
        Resume::Off => Unsupported::Proceed,
        Resume::On => Unsupported::Fail,
    };
    let state_path = super::destination_lock::state_path_for(dest);
    let (lock, found) = blocking::unblock({
        let dest = dest.to_path_buf();
        let hooks = hooks.clone();
        let state_path = state_path.clone();
        move || {
            let lock = DestinationLock::acquire(&dest, unsupported, &hooks)?;
            let found = match lock.handle() {
                Some(handle) if resume == Resume::On && !lock.created() => {
                    match handle.metadata() {
                        Ok(metadata) => metadata.len() > 0,
                        Err(source) => {
                            lock.release_keeping();
                            return Err(DownloadError::io(IoOp::Lock, state_path, source));
                        }
                    }
                }
                _ => false,
            };
            Ok::<_, DownloadError>((lock, found))
        }
    })
    .await?;
    if found {
        ownership.own_to_resume(lock, output.tmp.clone());
        return Ok(Locked::ToResume);
    }
    ownership.own(lock, output.tmp.clone());
    let invalidated = blocking::unblock({
        let ownership = ownership.clone();
        move || ownership.with_lock(DestinationLock::invalidate)
    })
    .await;
    match invalidated {
        Some(Err(source)) => Err(DownloadError::Io {
            op: IoOp::Lock,
            path: state_path,
            source: Arc::new(source),
        }),
        _ => Ok(Locked::Fresh),
    }
}

/// The options the transfer runs with: one connection for a resumable
/// task that preferred one (design §11.5.8), `options` otherwise.
fn transfer_options(mut options: TaskOptions, one_connection: bool) -> TaskOptions {
    if one_connection {
        options.max_connections = std::num::NonZeroUsize::MIN;
    }
    options
}

/// The state header of a resumable download, or `None` when it is not
/// resumable (design §11.5.3): `Resume::On`, the concurrent engine (which
/// implies a known, non-zero size, no content coding and ranges), and a
/// usable validator that fits the header.
fn resumable_header(
    resume: Resume,
    mode: DownloadMode,
    meta: &BoltLoadAdapterMeta,
) -> Option<Vec<u8>> {
    if resume != Resume::On || mode != DownloadMode::Concurrent {
        return None;
    }
    let validator = meta.validator.as_ref()?;
    encode_header(&Header {
        block_size: checkpoint::BLOCK_SIZE.get() as u32,
        total: meta.content_size,
        validator,
        source: None,
    })
    .inspect_err(|e| debug!("[TASK] not resumable: {e}"))
    .ok()
}

/// The resumable new start, in the order of design §11.5.5, after the
/// lock was taken and an old state invalidated: create or truncate the
/// partial file, size it and sync it; write the state header through the
/// locked handle and sync it; sync the directory.
///
/// The task is in phase B1 until the header is synced, and in phase C from
/// then on (design §11.5.9). Cancellation is checked between the steps,
/// never raced against one: a step that started ends.
async fn start_resumable(
    output: &OutputFile,
    ownership: &Arc<Ownership>,
    total: u64,
    header: Vec<u8>,
    cancel_token: &CancellationToken,
) -> Result<StateFile, DownloadError> {
    ownership.enter(Phase::Fresh);
    let tmp = output.tmp.clone();
    let state_path = super::destination_lock::state_path_for(&output.dest);
    let owner = ownership.clone();
    let token = cancel_token.clone();
    blocking::unblock(move || {
        let hooks = owner.hooks();
        let io = |op, path: &std::path::Path, source| DownloadError::Io {
            op,
            path: path.to_path_buf(),
            source: Arc::new(source),
        };
        let cancelled = || {
            if token.is_cancelled() {
                Err(DownloadError::Cancelled)
            } else {
                Ok(())
            }
        };
        let file = hooks
            .before(Step::CreatePartial)
            .and_then(|()| {
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&tmp)?;
                file.set_len(total)?;
                Ok(file)
            })
            .map_err(|e| io(IoOp::Create, &tmp, e))?;
        cancelled()?;
        hooks
            .before(Step::SyncPartial)
            .and_then(|()| file.sync_all())
            .map_err(|e| io(IoOp::Flush, &tmp, e))?;
        drop(file);
        cancelled()?;
        let handle = owner.state_file().ok_or_else(|| {
            io(
                IoOp::Lock,
                &state_path,
                std::io::Error::other("the state file is not locked"),
            )
        })?;
        let mut state = StateFile::new(Box::new(handle));
        state
            .write_header(&header)
            .map_err(|e| io(IoOp::Write, &state_path, e))?;
        // The header is durable: the pair can be resumed from now on.
        owner.enter(Phase::Resumable);
        cancelled()?;
        hooks
            .before(Step::SyncDirAfterStart)
            .and_then(|()| sync_parent_dir(&tmp))
            .map_err(|e| io(IoOp::Flush, &tmp, e))?;
        cancelled()?;
        Ok(state)
    })
    .await
}

/// A resume that was checked and can go on.
struct Resumed {
    /// The adapter's own metadata, which the state file was checked
    /// against.
    meta: BoltLoadAdapterMeta,
    /// The state file, its records written back, for the checkpoints.
    state: StateFile,
    report: ResumeReport,
    /// The ranges kept, which the transfer does not download again.
    kept: Vec<Range<u64>>,
}

/// Check the state file an earlier task left and the partial file next to
/// it, then write the records kept back (design §11.5.10, §11.5.5
/// "续传"), from phase B2:
///
/// 0. the partial file must be there ([`RejectReason::MissingPartial`]);
/// 1. and 3. read the state file through the locked handle;
/// 2. compare it with the metadata the adapter retrieves itself, not one
///    the caller gave (design §14.30);
/// 4. read every kept block back and check its CRC;
/// 5. enter phase C, write the kept records back, then size the partial
///    file to the total.
///
/// Cancellation is checked between the steps and before each block, never
/// raced against a file step. Whatever grows with the records is
/// allocated fallibly.
///
/// # Errors
///
/// [`DownloadError::ResumeRejected`] before either file was changed; the
/// metadata could not be retrieved; a file step failed; cancelled.
async fn resume_checked(
    attempt: &Attempt,
    rt: &ThreadedRuntimeImpl,
    output: &OutputFile,
    ownership: &Arc<Ownership>,
    cancel_token: &CancellationToken,
) -> Result<Resumed, DownloadError> {
    let tmp = output.tmp.clone();
    let state_path = super::destination_lock::state_path_for(&output.dest);
    let rejected = |reason| DownloadError::ResumeRejected {
        reason,
        state_file: state_path.clone(),
    };
    let cancelled = || {
        if cancel_token.is_cancelled() {
            Err(DownloadError::Cancelled)
        } else {
            Ok(())
        }
    };
    let (scanned, partial_len) = blocking::unblock({
        let owner = ownership.clone();
        let tmp = tmp.clone();
        let state_path = state_path.clone();
        move || {
            let rejected = |reason| DownloadError::ResumeRejected {
                reason,
                state_file: state_path.clone(),
            };
            let partial_len = match std::fs::metadata(&tmp) {
                Ok(metadata) => metadata.len(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(rejected(RejectReason::MissingPartial));
                }
                Err(e) => return Err(DownloadError::io(IoOp::Inspect, &tmp, e)),
            };
            let read = owner
                .hooks()
                .before(Step::ReadState)
                .map_err(ReadError::Io)
                .and_then(|()| {
                    let handle = owner
                        .with_lock(DestinationLock::handle)
                        .flatten()
                        .ok_or_else(|| {
                            ReadError::Io(std::io::Error::other("the state file is not locked"))
                        })?;
                    read_state(&*handle)
                });
            match read {
                Ok(scanned) => Ok((scanned, partial_len)),
                Err(ReadError::Rejected(reason)) => Err(rejected(reason)),
                Err(ReadError::Io(source)) => {
                    Err(DownloadError::io(IoOp::Read, &state_path, source))
                }
            }
        }
    })
    .await?;
    cancelled()?;
    // The network steps race cancellation; they change no file.
    let meta = until_cancelled(cancel_token, async {
        let meta = meta::retrieve_meta(&attempt.adapter, &attempt.options.meta_retry, Some(rt))
            .await
            .map_err(|source| DownloadError::Metadata { source })?;
        let ranges = attempt.adapter.is_range_stream_available().await;
        compare(&scanned.header, &meta, None, ranges).map_err(rejected)?;
        Ok(meta)
    })
    .await?;
    cancelled()?;
    let Scanned {
        header,
        records,
        dropped,
    } = scanned;
    let block_size = header.block_size;
    let (mut kept, dropped) = blocking::unblock({
        let hooks = ownership.hooks().clone();
        let tmp = tmp.clone();
        let token = cancel_token.clone();
        move || {
            let file =
                std::fs::File::open(&tmp).map_err(|e| DownloadError::io(IoOp::Read, &tmp, e))?;
            let partial = CheckedBlocks { file, hooks };
            let mut dropped = dropped;
            let kept = verify(
                records,
                block_size,
                partial_len,
                &partial,
                &|| token.is_cancelled(),
                &mut dropped,
            )
            .map_err(|e| match e {
                VerifyError::Cancelled => DownloadError::Cancelled,
                // The error names the block; its kind stays.
                VerifyError::Read { range, source } => DownloadError::io(
                    IoOp::Read,
                    &tmp,
                    std::io::Error::new(source.kind(), format!("the bytes {range:?}: {source}")),
                ),
                VerifyError::OutOfMemory(source) => DownloadError::io(IoOp::Read, &tmp, source),
            })?;
            Ok::<_, DownloadError>((kept, dropped))
        }
    })
    .await?;
    let report = report(state_path.clone(), &mut kept, block_size, dropped)
        .map_err(|e| DownloadError::io(IoOp::Read, &state_path, e))?;
    for drop in &report.dropped {
        warn!(
            "[TASK] resuming without the record at {} of {}, {}: {:?}",
            drop.position,
            state_path.display(),
            drop.reason,
            drop.range
        );
    }
    let mut ranges = Vec::new();
    ranges.try_reserve_exact(report.kept.len()).map_err(|e| {
        DownloadError::io(
            IoOp::Read,
            &state_path,
            std::io::Error::new(std::io::ErrorKind::OutOfMemory, e),
        )
    })?;
    ranges.extend_from_slice(&report.kept);
    cancelled()?;
    let total = meta.content_size;
    let state = blocking::unblock({
        let owner = ownership.clone();
        let token = cancel_token.clone();
        let tmp = tmp.clone();
        let state_path = state_path.clone();
        move || {
            // The state file changes from here on: the pair stays
            // resumable whatever happens (design §11.5.9, C).
            owner.enter(Phase::Resumable);
            let handle = owner.state_file().ok_or_else(|| {
                DownloadError::io(
                    IoOp::Lock,
                    &state_path,
                    std::io::Error::other("the state file is not locked"),
                )
            })?;
            let mut state = StateFile::new(Box::new(handle));
            state
                .rewrite(header.len, kept.iter().map(|record| record.block))
                .map_err(|e| DownloadError::io(IoOp::Write, &state_path, e))?;
            if token.is_cancelled() {
                return Err(DownloadError::Cancelled);
            }
            owner
                .hooks()
                .before(Step::SizePartial)
                .and_then(|()| {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(&tmp)?
                        .set_len(total)
                })
                .map_err(|e| DownloadError::io(IoOp::Allocate, &tmp, e))?;
            Ok(state)
        }
    })
    .await?;
    cancelled()?;
    Ok(Resumed {
        meta,
        state,
        report,
        kept: ranges,
    })
}

/// The partial file, read block by block through its own handle to check
/// it before a resume; each read announced to the hooks.
struct CheckedBlocks {
    file: std::fs::File,
    hooks: FileHooks,
}

impl BlockReader for CheckedBlocks {
    fn read(&self, range: Range<u64>) -> std::io::Result<Vec<u8>> {
        self.hooks.before(Step::ReadPartial)?;
        // A block: at most `BLOCK_SIZE` bytes.
        let mut data = vec![0; (range.end - range.start) as usize];
        if read_full(&self.file, &mut data, range.start)? < data.len() {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        Ok(data)
    }
}

/// After a resumable new start or a resume failed or was cancelled before
/// the transfer: leave both files as they are while a resume was checked
/// (B2); discard both before a new start's header was durable (B1); keep
/// both after it, or once a resume began to write back (C). The partial
/// file left and the one kept for a resume (design §11.5.9).
async fn end_failed_start(
    output: &OutputFile,
    ownership: &Arc<Ownership>,
) -> (Option<PathBuf>, Option<PathBuf>) {
    match ownership.phase() {
        Phase::Checking => {
            ownership.keep_files();
            (None, None)
        }
        Phase::Resumable => {
            ownership.keep_files();
            (None, Some(output.tmp.clone()))
        }
        _ => {
            let owner = ownership.clone();
            let discarded = blocking::unblock(move || owner.discard_resumable()).await;
            (discarded.leftover, None)
        }
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

    /// A resumable task that preferred one connection runs with one; any
    /// other with its own limit.
    #[test]
    fn test_a_routed_task_runs_with_one_connection() {
        let options = TaskOptions::default();
        assert_eq!(
            transfer_options(options.clone(), true)
                .max_connections
                .get(),
            1
        );
        assert_eq!(
            transfer_options(options.clone(), false),
            options,
            "untouched"
        );
    }
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
                resumable: false,
                resumed: None,
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
