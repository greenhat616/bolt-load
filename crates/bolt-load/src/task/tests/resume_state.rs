//! The state file of a resumable task (design §11.5.3, §11.5.5, §11.5.9):
//! the order of each file step, what each failure leaves, and that a task
//! with `Resume::Off` takes none of these steps.

use std::{
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use bolt_load_core::adapter::{UnretryableError, Validator};
use bolt_load_tests::adapter::{fault::Gate, simple::SimpleTestAdapter};
use bytes::Bytes;
use fs4::TryLockError;
use smol_cancellation_token::CancellationToken;
use tempfile::TempDir;

use crate::{
    adapter::{AdapterError, AnyBytesStream, BoltLoadAdapter, BoltLoadAdapterMeta, FullResponse},
    task::{
        DownloadError, DownloadMode, IoOp, Task, TaskOptions, TaskState, TaskStatus,
        builder::Resume,
        instance::{
            concurrent_task::checkpoint::BLOCK_SIZE,
            destination_lock::{LockHooks, state_path_for},
            file_steps::{
                Step,
                tests::{StepLog, When},
            },
            state_file::tests::{decode_header, decode_records},
        },
        test_support::{builder, tmp_path, watchdog},
    },
};

const BLOCK: u64 = BLOCK_SIZE.get();
/// Three blocks and a short one.
const SIZE: u64 = 3 * BLOCK + 4321;
/// Where a faulty source stops: within the third block.
const HALF: u64 = 2 * BLOCK + 100;
const FRAME: u64 = 32 << 10;

/// What a [`Source`] does once a range reaches [`HALF`].
#[derive(Clone)]
enum Fault {
    /// Nothing: the whole range.
    None,
    /// End a range's stream with this error.
    Error(AdapterError),
    /// End a range's stream, and the full stream, with this error.
    Everywhere(AdapterError),
    /// Wait at the gate, then go on.
    Gate(Gate),
}

/// The content of a [`SimpleTestAdapter`], by range, with a validator in
/// its metadata, failing or held as `fault` says.
#[derive(Clone)]
struct Source {
    content: Bytes,
    fault: Fault,
    /// Whether it serves ranges.
    ranges: bool,
    /// The range streams open now, and the most open at once.
    open: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl Source {
    fn new(fault: Fault) -> Self {
        Self {
            content: SimpleTestAdapter::new(SIZE as usize).content().clone(),
            fault,
            ranges: true,
            open: Arc::default(),
            peak: Arc::default(),
        }
    }

    /// A source that serves no ranges.
    fn without_ranges(fault: Fault) -> Self {
        Self {
            ranges: false,
            ..Self::new(fault)
        }
    }
}

/// Counts an open range stream until dropped.
struct Open(Arc<AtomicUsize>);

impl Drop for Open {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn meta() -> BoltLoadAdapterMeta {
    BoltLoadAdapterMeta::new(SIZE).with_validator(Some(Validator::StrongEtag("\"v1\"".to_owned())))
}

impl BoltLoadAdapter for Source {
    async fn is_range_stream_available(&self) -> bool {
        self.ranges
    }

    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError> {
        Ok(meta())
    }

    /// The whole content; with [`Fault::Everywhere`], the bytes before
    /// [`HALF`], then the error.
    async fn full_stream(&self) -> Result<FullResponse, AdapterError> {
        let content = self.content.clone();
        let items = match &self.fault {
            Fault::Everywhere(error) => {
                vec![Ok(content.slice(..HALF as usize)), Err(error.clone())]
            }
            _ => vec![Ok(content)],
        };
        Ok(FullResponse::new(Box::pin(futures::stream::iter(items))))
    }

    async fn range_stream(&self, start: u64, end: u64) -> Result<AnyBytesStream, AdapterError> {
        let content = self.content.clone();
        let fault = self.fault.clone();
        let open = self.open.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(open, Ordering::SeqCst);
        let held = Arc::new(Open(self.open.clone()));
        Ok(Box::pin(futures::stream::unfold(
            (start, false),
            move |(at, faulted)| {
                let content = content.clone();
                let fault = fault.clone();
                let _held = held.clone();
                async move {
                    if at >= end {
                        return None;
                    }
                    if at >= HALF && !faulted {
                        match fault {
                            Fault::None => {}
                            Fault::Error(error) | Fault::Everywhere(error) => {
                                return Some((Err(error), (end, true)));
                            }
                            Fault::Gate(gate) => gate.pass().await,
                        }
                    }
                    let mut next = (at + FRAME).min(end);
                    if at < HALF && next > HALF {
                        next = HALF;
                    }
                    Some((
                        Ok(content.slice(at as usize..next as usize)),
                        (next, at >= HALF),
                    ))
                }
            },
        )))
    }
}

fn dest() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("file.bin");
    (dir, dest)
}

/// One connection, so the source's fault comes at a known place, and a
/// checkpoint after every megabyte.
fn options() -> TaskOptions {
    TaskOptions::default()
        .max_connections(std::num::NonZeroUsize::new(1).unwrap())
        .checkpoint_bytes(std::num::NonZeroU64::new(BLOCK).unwrap())
}

/// A resumable task on `source` to `dest`, its file steps logged by `log`.
fn resumable(source: Source, dest: &Path, log: &StepLog, token: &CancellationToken) -> Task {
    builder(DownloadMode::Concurrent, source, dest, token)
        .meta(meta())
        .options(options())
        .resume(Resume::On)
        .file_hooks(log.hooks())
        .spawn()
        .expect("task should spawn")
}

/// The terminal status of `task`.
async fn finish(task: Task) -> std::sync::Arc<TaskStatus> {
    let status = task.status();
    let _ = watchdog(task).await;
    status.borrow().clone()
}

const NEW_START: [Step; 5] = [
    Step::CreatePartial,
    Step::SyncPartial,
    Step::WriteState,
    Step::SyncState,
    Step::SyncDirAfterStart,
];

const COMPLETION: [Step; 6] = [
    Step::Rename,
    Step::SyncDirAfterRename,
    Step::TruncateState,
    Step::SyncState,
    Step::RemoveState,
    Step::SyncDirAfterState,
];

const DISCARD: [Step; 6] = [
    Step::Invalidate,
    Step::SyncInvalidation,
    Step::RemovePartial,
    Step::SyncDirAfterPartial,
    Step::RemoveState,
    Step::SyncDirAfterState,
];

/// The steps between the new start and the end are the checkpoints':
/// writes and syncs of the state file, a write synced before the next.
fn assert_checkpoints(steps: &[Step]) {
    assert!(
        steps
            .chunks(2)
            .all(|pair| pair == [Step::WriteState, Step::SyncState]),
        "{steps:?}"
    );
}

/// The state file is a valid header followed by valid records only, each
/// of a block whose bytes in the partial file have its CRC.
fn assert_kept_state(dest: &Path) -> usize {
    let state = std::fs::read(state_path_for(dest)).unwrap();
    let header = decode_header(&state).expect("a valid header");
    assert_eq!(header.total, SIZE);
    let records = decode_records(&state, header.len);
    assert_eq!(
        state.len(),
        header.len + records.len() * 24,
        "nothing past the records"
    );
    let partial = std::fs::read(tmp_path(dest)).unwrap();
    assert_eq!(partial.len() as u64, SIZE);
    for record in &records {
        let start = (record.index * BLOCK) as usize;
        let block = &partial[start..start + record.len as usize];
        assert_eq!(record.crc, crc32c::crc32c(block), "block {}", record.index);
    }
    records.len()
}

/// The lock is free: another task can take the destination.
fn assert_unlocked(dest: &Path) {
    let lock = crate::task::instance::destination_lock::DestinationLock::acquire(
        dest,
        crate::task::instance::destination_lock::Unsupported::Fail,
        &LockHooks::default(),
    )
    .expect("the lock is free");
    lock.release_keeping();
}

/// A resumable download: the new start, checkpoints, and the completion,
/// each step in the order of §11.5.5, and no file left but the
/// destination.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_resumable_download_completes_in_order() {
    let (_dir, dest) = dest();
    let log = StepLog::default();
    let source = Source::new(Fault::None);
    let content = source.content.clone();
    let status = finish(resumable(source, &dest, &log, &CancellationToken::new())).await;
    assert_eq!(status.state, TaskState::Finished, "{:?}", status.error);
    assert_eq!(std::fs::read(&dest).unwrap(), content);
    assert!(!tmp_path(&dest).exists() && !state_path_for(&dest).exists());
    assert_eq!(
        (&status.kept_for_resume, &status.leftover_temp_file),
        (&None, &None)
    );
    let steps = log.steps();
    assert_eq!(steps[..5], NEW_START, "{steps:?}");
    assert_eq!(steps[steps.len() - 6..], COMPLETION, "{steps:?}");
    let middle = &steps[5..steps.len() - 6];
    assert_checkpoints(middle);
    assert!(!middle.is_empty(), "checkpoints were taken: {steps:?}");
}

/// A resumable task that fails or is cancelled keeps the partial file and
/// a valid state, records of synced blocks only, and names the partial
/// file as kept; the lock is released (§11.5.9, C).
#[tokio::test(flavor = "multi_thread")]
async fn test_a_failed_or_cancelled_task_keeps_both_files() {
    for cancel in [false, true] {
        let (_dir, dest) = dest();
        let log = StepLog::default();
        let gate = Gate::new();
        let fault = if cancel {
            Fault::Gate(gate.clone())
        } else {
            Fault::Error(UnretryableError::NotFound { status: 404 }.into())
        };
        let token = CancellationToken::new();
        let task = resumable(Source::new(fault), &dest, &log, &token);
        if cancel {
            watchdog(gate.entered()).await;
            // Cancel once blocks 0 and 1 are written (the runner holds the
            // last bytes before the gate, short of a frame), so that
            // the teardown checkpoint finds blocks 0 and 1 complete.
            let mut status = task.status();
            watchdog(async {
                while !status
                    .borrow_and_update()
                    .progress
                    .downloaded_chunks
                    .iter()
                    .any(|r| r.start == 0 && r.end >= 2 * BLOCK)
                {
                    status.changed().await.unwrap();
                }
            })
            .await;
            token.cancel();
        }
        let status = finish(task).await;
        let expected = if cancel {
            TaskState::Cancelled
        } else {
            TaskState::Failed
        };
        assert_eq!(status.state, expected, "{:?}", status.error);
        gate.release();
        assert_eq!(status.kept_for_resume, Some(tmp_path(&dest)), "{cancel}");
        assert_eq!(status.leftover_temp_file, None);
        // Blocks 0 and 1 were complete before the fault, and the
        // teardown checkpoint recorded them.
        assert_eq!(assert_kept_state(&dest), 2, "{cancel}");
        let steps = log.steps();
        assert_eq!(steps[..5], NEW_START);
        assert_checkpoints(&steps[5..]);
        assert_unlocked(&dest);
    }
}

/// A changed resource discards both files, in the order of §11.5.5: the
/// state is invalidated before the partial file goes.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_changed_resource_discards_both_files() {
    let (_dir, dest) = dest();
    let log = StepLog::default();
    let fault = Fault::Error(UnretryableError::ResourceChanged.into());
    let status = finish(resumable(
        Source::new(fault),
        &dest,
        &log,
        &CancellationToken::new(),
    ))
    .await;
    assert!(
        matches!(status.error, Some(DownloadError::ResourceChanged { .. })),
        "{:?}",
        status.error
    );
    assert!(!tmp_path(&dest).exists() && !state_path_for(&dest).exists());
    assert_eq!(
        (&status.kept_for_resume, &status.leftover_temp_file),
        (&None, &None)
    );
    let steps = log.steps();
    assert_eq!(steps[steps.len() - DISCARD.len()..], DISCARD, "{steps:?}");
}

/// When the invalidation of a discard fails, the partial file is not
/// removed: both stay, and the partial file is reported as left.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_failed_invalidation_keeps_the_partial_file() {
    let (_dir, dest) = dest();
    let log = StepLog::default();
    log.fail(Step::Invalidate, When::Nth(1));
    let fault = Fault::Error(UnretryableError::ResourceChanged.into());
    let status = finish(resumable(
        Source::new(fault),
        &dest,
        &log,
        &CancellationToken::new(),
    ))
    .await;
    assert!(matches!(
        status.error,
        Some(DownloadError::ResourceChanged { .. })
    ));
    assert_eq!(status.leftover_temp_file, Some(tmp_path(&dest)));
    assert_eq!(status.kept_for_resume, None);
    assert!(tmp_path(&dest).exists());
    assert!(decode_header(&std::fs::read(state_path_for(&dest)).unwrap()).is_some());
    let steps = log.steps();
    assert_eq!(steps.last(), Some(&Step::Invalidate), "{steps:?}");
    assert_unlocked(&dest);
}

/// A step of the new start that fails before the header is durable fails
/// the task in phase B1, which discards what it created: neither file is
/// left (§11.5.9, B1). Once the header is synced, the task is in phase C:
/// a failed directory sync keeps both files, for a resume.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_failed_new_start_discards_both_files() {
    let (_kept_dir, kept) = dest();
    let log = StepLog::default();
    log.fail(Step::SyncDirAfterStart, When::Nth(1));
    let status = finish(resumable(
        Source::new(Fault::None),
        &kept,
        &log,
        &CancellationToken::new(),
    ))
    .await;
    assert!(
        matches!(
            status.error,
            Some(DownloadError::Io {
                op: IoOp::Flush,
                ..
            })
        ),
        "{:?}",
        status.error
    );
    assert_eq!(status.kept_for_resume, Some(tmp_path(&kept)));
    assert_eq!(status.leftover_temp_file, None);
    assert_eq!(assert_kept_state(&kept), 0, "the header alone");
    assert_eq!(log.steps(), NEW_START);
    assert_unlocked(&kept);

    for (step, op) in [
        (Step::CreatePartial, IoOp::Create),
        (Step::SyncPartial, IoOp::Flush),
        (Step::WriteState, IoOp::Write),
        (Step::SyncState, IoOp::Write),
    ] {
        let (_dir, dest) = dest();
        let log = StepLog::default();
        log.fail(step, When::Nth(1));
        let status = finish(resumable(
            Source::new(Fault::None),
            &dest,
            &log,
            &CancellationToken::new(),
        ))
        .await;
        let Some(DownloadError::Io { op: failed, .. }) = &status.error else {
            panic!("{step:?}: {:?}", status.error);
        };
        assert_eq!(*failed, op, "{step:?}");
        assert!(
            !tmp_path(&dest).exists() && !state_path_for(&dest).exists(),
            "{step:?}"
        );
        assert_eq!(status.kept_for_resume, None);
        let steps = log.steps();
        let at = NEW_START.iter().position(|&s| s == step).unwrap();
        assert_eq!(steps[..=at], NEW_START[..=at], "{step:?}");
        assert_eq!(steps[at + 1..], DISCARD, "{step:?}");
    }
}

/// A failed rename keeps both files: the partial file is complete, and the
/// state records every block.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_failed_rename_keeps_both_files() {
    let (_dir, dest) = dest();
    let log = StepLog::default();
    log.fail(Step::Rename, When::Nth(1));
    let status = finish(resumable(
        Source::new(Fault::None),
        &dest,
        &log,
        &CancellationToken::new(),
    ))
    .await;
    assert!(
        matches!(
            status.error,
            Some(DownloadError::Io {
                op: IoOp::Rename,
                ..
            })
        ),
        "{:?}",
        status.error
    );
    assert_eq!(status.kept_for_resume, Some(tmp_path(&dest)));
    assert!(!dest.exists());
    assert_eq!(assert_kept_state(&dest), 4, "every block");
    assert_unlocked(&dest);
}

/// After the rename the task has finished: each later step that fails is
/// only logged, and what is left is what §11.5.5 says the next task sees.
#[tokio::test(flavor = "multi_thread")]
async fn test_failed_steps_after_the_rename_still_finish() {
    // The steps to fail, and whether the state file is left: none, empty,
    // or with its header.
    #[derive(Debug, PartialEq)]
    enum Left {
        Nothing,
        Empty,
        Valid,
    }
    for (failed, left) in [
        (vec![Step::SyncDirAfterRename], Left::Nothing),
        (vec![Step::TruncateState], Left::Nothing),
        (vec![Step::SyncState], Left::Nothing),
        (vec![Step::RemoveState], Left::Empty),
        (vec![Step::SyncDirAfterState], Left::Nothing),
        (vec![Step::SyncState, Step::RemoveState], Left::Empty),
        (vec![Step::TruncateState, Step::RemoveState], Left::Valid),
    ] {
        let (_dir, dest) = dest();
        let log = StepLog::default();
        for &step in &failed {
            log.fail(step, When::After(Step::Rename));
        }
        let source = Source::new(Fault::None);
        let content = source.content.clone();
        let status = finish(resumable(source, &dest, &log, &CancellationToken::new())).await;
        assert_eq!(status.state, TaskState::Finished, "{failed:?}");
        assert_eq!(std::fs::read(&dest).unwrap(), content);
        assert_eq!(
            (&status.kept_for_resume, &status.leftover_temp_file),
            (&None, &None)
        );
        let steps = log.steps();
        assert_eq!(steps[steps.len() - 6..], COMPLETION, "{failed:?}");
        let state = state_path_for(&dest);
        let found = match std::fs::read(&state) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Left::Nothing,
            Ok(bytes) if bytes.is_empty() => Left::Empty,
            Ok(bytes) if decode_header(&bytes).is_some() => Left::Valid,
            other => panic!("{failed:?}: {other:?}"),
        };
        assert_eq!(found, left, "{failed:?}");
        assert_unlocked(&dest);
    }
}

/// A fallback to one connection invalidates the state before the partial
/// file goes; the task is no longer resumable and ends as one that is not.
/// When the invalidation fails, it keeps both files and fails instead of
/// falling back.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_fallback_invalidates_the_state_first() {
    for invalidation_fails in [false, true] {
        let (_dir, dest) = dest();
        let log = StepLog::default();
        if invalidation_fails {
            log.fail(Step::Invalidate, When::Nth(1));
        }
        let source = Source::new(Fault::Error(UnretryableError::RangeIgnored.into()));
        let content = source.content.clone();
        let status = finish(resumable(source, &dest, &log, &CancellationToken::new())).await;
        let steps = log.steps();
        if invalidation_fails {
            assert!(
                matches!(status.error, Some(DownloadError::Io { op: IoOp::Lock, .. })),
                "{:?}",
                status.error
            );
            assert_eq!(status.kept_for_resume, Some(tmp_path(&dest)));
            assert!(assert_kept_state(&dest) >= 1);
            assert_eq!(steps.last(), Some(&Step::Invalidate), "{steps:?}");
        } else {
            assert_eq!(status.state, TaskState::Finished, "{:?}", status.error);
            assert!(status.info.as_ref().unwrap().fallback.is_some());
            assert_eq!(std::fs::read(&dest).unwrap(), content);
            assert!(!state_path_for(&dest).exists() && !tmp_path(&dest).exists());
            let at = steps.iter().position(|&s| s == Step::Invalidate).unwrap();
            assert_eq!(
                steps[at..],
                [
                    Step::Invalidate,
                    Step::SyncInvalidation,
                    Step::RemovePartial,
                    Step::SyncDirAfterPartial,
                    Step::RemoveState,
                    Step::SyncDirAfterState,
                ],
                "{steps:?}"
            );
        }
        assert_unlocked(&dest);
    }
}

/// A task with `Resume::Off`, and a task with `Resume::On` that is not
/// resumable (no validator, or one connection preferred with no ranges to
/// route it through), take none of the steps: no state is written, and the
/// files end as before C33.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_task_that_keeps_no_state_takes_none_of_the_steps() {
    for (resume, validator, mode) in [
        (Resume::Off, true, DownloadMode::Concurrent),
        (Resume::Off, true, DownloadMode::Singleton),
        (Resume::On, false, DownloadMode::Concurrent),
        (Resume::On, true, DownloadMode::Singleton),
    ] {
        let (_dir, dest) = dest();
        let log = StepLog::default();
        let source = if resume == Resume::On && mode == DownloadMode::Singleton {
            Source::without_ranges(Fault::None)
        } else {
            Source::new(Fault::None)
        };
        let content = source.content.clone();
        let meta = if validator {
            meta()
        } else {
            BoltLoadAdapterMeta::new(SIZE)
        };
        let task = builder(mode, source, &dest, &CancellationToken::new())
            .meta(meta)
            .options(options())
            .resume(resume)
            .file_hooks(log.hooks())
            .spawn()
            .unwrap();
        let status = finish(task).await;
        assert_eq!(status.state, TaskState::Finished, "{resume:?} {mode:?}");
        assert_eq!(std::fs::read(&dest).unwrap(), content);
        assert!(!tmp_path(&dest).exists() && !state_path_for(&dest).exists());
        assert_eq!(log.steps(), [], "{resume:?} {validator} {mode:?}");
    }
    // A failed task that keeps no state removes its temporary file.
    let (_dir, dest) = dest();
    let log = StepLog::default();
    let fault = Fault::Error(UnretryableError::NotFound { status: 404 }.into());
    let task = builder(
        DownloadMode::Concurrent,
        Source::new(fault),
        &dest,
        &CancellationToken::new(),
    )
    .meta(meta())
    .options(options())
    .file_hooks(log.hooks())
    .spawn()
    .unwrap();
    let status = finish(task).await;
    assert_eq!(status.state, TaskState::Failed);
    assert_eq!(status.kept_for_resume, None);
    assert!(!tmp_path(&dest).exists() && !state_path_for(&dest).exists());
    assert_eq!(log.steps(), []);
}

/// A task that may resume needs the lock: where the file system cannot
/// lock, it fails with `Io { op: Lock }` instead of going on unlocked.
#[tokio::test(flavor = "multi_thread")]
async fn test_resuming_needs_the_lock() {
    let (_dir, dest) = dest();
    let log = StepLog::default();
    let unsupported = || {
        #[cfg(unix)]
        let code = libc::ENOLCK;
        #[cfg(windows)]
        let code = 50; // ERROR_NOT_SUPPORTED
        io::Error::from_raw_os_error(code)
    };
    let task = builder(
        DownloadMode::Concurrent,
        Source::new(Fault::None),
        &dest,
        &CancellationToken::new(),
    )
    .meta(meta())
    .resume(Resume::On)
    .file_hooks(log.hooks())
    .lock_hooks(LockHooks::default().try_lock(move |_| Err(TryLockError::Error(unsupported()))))
    .spawn()
    .unwrap();
    let status = finish(task).await;
    assert!(
        matches!(status.error, Some(DownloadError::Io { op: IoOp::Lock, .. })),
        "{:?}",
        status.error
    );
    assert!(!tmp_path(&dest).exists());
    // It failed when it took the lock, before any file step.
    assert_eq!(log.steps(), []);
}

/// A state file operation held at a gate keeps the lock: teardown waits
/// for it, so another task on the destination is in use until it ends.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_held_state_write_keeps_the_lock() {
    let (_dir, dest) = dest();
    let log = StepLog::default();
    // The first checkpoint's records: the second write to the state file.
    let (entered, release) = log.gate(Step::WriteState, When::Nth(2));
    let token = CancellationToken::new();
    let task = resumable(Source::new(Fault::None), &dest, &log, &token);
    tokio::task::spawn_blocking(move || {
        entered
            .recv_timeout(Duration::from_secs(30))
            .expect("the checkpoint writes")
    })
    .await
    .unwrap();
    token.cancel();
    let second = builder(
        DownloadMode::Singleton,
        SimpleTestAdapter::new(16),
        &dest,
        &CancellationToken::new(),
    )
    .spawn()
    .unwrap();
    let second = watchdog(second).await;
    assert!(
        matches!(second, Err(DownloadError::InUse { .. })),
        "{second:?}"
    );
    assert!(!task.is_terminal(), "teardown waits for the write");
    release.send(()).unwrap();
    let status = finish(task).await;
    assert_eq!(status.state, TaskState::Cancelled);
    assert_eq!(status.kept_for_resume, Some(tmp_path(&dest)));
    assert_kept_state(&dest);
    assert_unlocked(&dest);
}

/// What a task left: its state, its error with the directory taken out,
/// whether each file is there, and what it reported.
#[derive(Debug, PartialEq)]
struct Outcome {
    state: TaskState,
    /// The mode it downloaded in, last.
    mode: Option<DownloadMode>,
    error: Option<String>,
    dest: bool,
    partial: bool,
    state_file: bool,
    kept_for_resume: bool,
    leftover_temp_file: bool,
    steps: Vec<Step>,
}

/// Run a task with `resume` that takes `meta` and prefers `mode`, on a
/// source that fails when `fails`; what it left.
async fn outcome(
    resume: Resume,
    meta: BoltLoadAdapterMeta,
    mode: DownloadMode,
    ranges: bool,
    fails: bool,
) -> Outcome {
    let (dir, dest) = dest();
    let log = StepLog::default();
    let fault = if fails {
        Fault::Everywhere(UnretryableError::NotFound { status: 404 }.into())
    } else {
        Fault::None
    };
    let source = if ranges {
        Source::new(fault)
    } else {
        Source::without_ranges(fault)
    };
    let task = builder(mode, source, &dest, &CancellationToken::new())
        .meta(meta)
        .options(options())
        .resume(resume)
        .file_hooks(log.hooks())
        .spawn()
        .unwrap();
    let status = finish(task).await;
    let dir = dir.path().display().to_string();
    Outcome {
        state: status.state,
        mode: status.info.as_ref().map(|info| info.mode),
        error: status
            .error
            .as_ref()
            .map(|error| error.to_string().replace(&dir, "<dir>")),
        dest: dest.exists(),
        partial: tmp_path(&dest).exists(),
        state_file: state_path_for(&dest).exists(),
        kept_for_resume: status.kept_for_resume.is_some(),
        leftover_temp_file: status.leftover_temp_file.is_some(),
        steps: log.steps(),
    }
}

/// A task with `Resume::On` that cannot resume (one connection preferred,
/// an unknown total, no validator) ends exactly as the same task with
/// `Resume::Off`, files and error included, whether it finishes or fails;
/// the lock it takes is the only difference (design §11.5.3).
#[tokio::test(flavor = "multi_thread")]
async fn test_a_task_that_cannot_resume_ends_as_one_with_resume_off() {
    let unknown = || {
        BoltLoadAdapterMeta::new(0).with_validator(Some(Validator::StrongEtag("\"v1\"".to_owned())))
    };
    let cases = [
        (
            "one connection preferred, no ranges",
            meta(),
            DownloadMode::Singleton,
            false,
        ),
        (
            "one connection preferred, an unknown total",
            unknown(),
            DownloadMode::Singleton,
            true,
        ),
        (
            "one connection preferred, no validator",
            BoltLoadAdapterMeta::new(SIZE),
            DownloadMode::Singleton,
            true,
        ),
        (
            "an unknown total",
            unknown(),
            DownloadMode::Concurrent,
            true,
        ),
        ("no ranges", meta(), DownloadMode::Concurrent, false),
        (
            "no validator",
            BoltLoadAdapterMeta::new(SIZE),
            DownloadMode::Concurrent,
            true,
        ),
    ];
    for (case, meta, mode, ranges) in cases {
        for fails in [false, true] {
            let off = outcome(Resume::Off, meta.clone(), mode, ranges, fails).await;
            let on = outcome(Resume::On, meta.clone(), mode, ranges, fails).await;
            assert_eq!(on, off, "{case}, fails: {fails}");
            let expected = if fails {
                TaskState::Failed
            } else {
                TaskState::Finished
            };
            assert_eq!(off.state, expected, "{case}: {:?}", off.error);
            assert!(
                !off.partial && !off.state_file && off.steps.is_empty(),
                "{case}"
            );
            assert_eq!(off.dest, !fails, "{case}");
        }
    }
}

/// Wait, off the async runtime, until a gated step is reached.
async fn entered(entered: std::sync::mpsc::Receiver<()>) {
    tokio::task::spawn_blocking(move || {
        entered
            .recv_timeout(Duration::from_secs(30))
            .expect("the step is reached")
    })
    .await
    .unwrap();
}

/// A cancellation during the new start is seen between its steps, never
/// racing one: before the header is durable it discards both files (B1);
/// after, it keeps both for a resume (C).
#[tokio::test(flavor = "multi_thread")]
async fn test_a_cancelled_new_start_follows_its_phase() {
    for (gated, kept) in [(Step::SyncPartial, false), (Step::SyncDirAfterStart, true)] {
        let (_dir, dest) = dest();
        let log = StepLog::default();
        let (reached, release) = log.gate(gated, When::Nth(1));
        let token = CancellationToken::new();
        let task = resumable(Source::new(Fault::None), &dest, &log, &token);
        entered(reached).await;
        token.cancel();
        release.send(()).unwrap();
        let status = finish(task).await;
        assert_eq!(status.state, TaskState::Cancelled, "{gated:?}");
        let steps = log.steps();
        let at = NEW_START.iter().position(|&s| s == gated).unwrap();
        assert_eq!(
            steps[..=at],
            NEW_START[..=at],
            "the gated step ended: {steps:?}"
        );
        if kept {
            assert_eq!(status.kept_for_resume, Some(tmp_path(&dest)));
            assert_eq!(assert_kept_state(&dest), 0);
            assert_eq!(steps.len(), at + 1, "{steps:?}");
        } else {
            assert_eq!(status.kept_for_resume, None);
            assert!(!tmp_path(&dest).exists() && !state_path_for(&dest).exists());
            assert_eq!(steps[at + 1..], DISCARD, "{steps:?}");
        }
        assert_eq!(status.leftover_temp_file, None);
        assert_unlocked(&dest);
    }
}

/// An invalidation whose cut fails leaves the state as it was: the
/// fallback stops and the pair is kept for a resume. One whose cut went
/// through but whose sync failed leaves an empty state, no usable pair: the
/// fallback stops, both files stay, and the partial file is reported as
/// left, not as kept. A discard that meets the same failures keeps the
/// partial file and reports it as left.
#[tokio::test(flavor = "multi_thread")]
async fn test_an_invalidation_reports_what_it_left() {
    for (fault, failed) in [
        (UnretryableError::RangeIgnored, Step::Invalidate),
        (UnretryableError::RangeIgnored, Step::SyncInvalidation),
        (UnretryableError::ResourceChanged, Step::Invalidate),
        (UnretryableError::ResourceChanged, Step::SyncInvalidation),
    ] {
        let (_dir, dest) = dest();
        let log = StepLog::default();
        log.fail(failed, When::Nth(1));
        let fallback = matches!(fault, UnretryableError::RangeIgnored);
        let status = finish(resumable(
            Source::new(Fault::Error(fault.into())),
            &dest,
            &log,
            &CancellationToken::new(),
        ))
        .await;
        let case = format!("fallback: {fallback}, {failed:?}");
        if fallback {
            assert!(
                matches!(status.error, Some(DownloadError::Io { op: IoOp::Lock, .. })),
                "{case}: {:?}",
                status.error
            );
        } else {
            assert!(
                matches!(status.error, Some(DownloadError::ResourceChanged { .. })),
                "{case}"
            );
        }
        assert!(tmp_path(&dest).exists(), "{case}");
        let state = std::fs::read(state_path_for(&dest)).unwrap();
        let intact = failed == Step::Invalidate;
        assert_eq!(decode_header(&state).is_some(), intact, "{case}");
        assert_eq!(state.is_empty(), !intact, "{case}");
        let kept = fallback && intact;
        assert_eq!(status.kept_for_resume.is_some(), kept, "{case}");
        assert_eq!(status.leftover_temp_file.is_some(), !kept, "{case}");
        assert_eq!(log.steps().last(), Some(&failed), "{case}");
        assert_unlocked(&dest);
    }
}

/// A panic at each file step of a real task ends as the phase it reached
/// says (design §11.5.9, TASK-8): the terminal status, both reports, the
/// files left, and the lock free again.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_panic_at_each_step_follows_its_phase() {
    #[derive(Debug, PartialEq)]
    enum Left {
        Neither,
        Both,
        Destination,
    }
    for (step, when, fault, left) in [
        // Before the header is durable (B1): both discarded.
        (Step::SyncPartial, When::Nth(1), Fault::None, Left::Neither),
        // After (C): both kept.
        (
            Step::SyncDirAfterStart,
            When::Nth(1),
            Fault::None,
            Left::Both,
        ),
        // A checkpoint's records (C): both kept.
        (Step::WriteState, When::Nth(2), Fault::None, Left::Both),
        // A discard, after the invalidation (D by then): both gone.
        (
            Step::RemovePartial,
            When::Nth(1),
            Fault::Error(UnretryableError::ResourceChanged.into()),
            Left::Neither,
        ),
        // After the rename (P): finished.
        (
            Step::TruncateState,
            When::After(Step::Rename),
            Fault::None,
            Left::Destination,
        ),
    ] {
        let (_dir, dest) = dest();
        let log = StepLog::default();
        log.panic(step, when);
        let status = finish(resumable(
            Source::new(fault),
            &dest,
            &log,
            &CancellationToken::new(),
        ))
        .await;
        let files = (
            dest.exists(),
            tmp_path(&dest).exists(),
            state_path_for(&dest).exists(),
        );
        match left {
            Left::Neither => {
                assert!(
                    matches!(status.error, Some(DownloadError::Panicked { .. })),
                    "{step:?}: {:?}",
                    status.error
                );
                assert_eq!(files, (false, false, false), "{step:?}");
                assert_eq!(status.kept_for_resume, None, "{step:?}");
            }
            Left::Both => {
                assert!(
                    matches!(status.error, Some(DownloadError::Panicked { .. })),
                    "{step:?}: {:?}",
                    status.error
                );
                assert_eq!(files, (false, true, true), "{step:?}");
                assert_eq!(status.kept_for_resume, Some(tmp_path(&dest)), "{step:?}");
                assert_kept_state(&dest);
            }
            Left::Destination => {
                assert_eq!(status.state, TaskState::Finished, "{step:?}");
                assert_eq!(files, (true, false, false), "{step:?}");
                assert_eq!(status.kept_for_resume, None, "{step:?}");
            }
        }
        assert_eq!(status.leftover_temp_file, None, "{step:?}");
        assert_unlocked(&dest);
    }
}

/// A panic in an invalidation, once the state file was cut and before its
/// sync ended, leaves both files, no usable pair: the partial file is
/// reported as left, not as kept, on a fallback and on a changed resource.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_panic_after_the_cut_reports_no_pair() {
    for fault in [
        UnretryableError::RangeIgnored,
        UnretryableError::ResourceChanged,
    ] {
        let (_dir, dest) = dest();
        let log = StepLog::default();
        log.panic(Step::SyncInvalidation, When::Nth(1));
        let status = finish(resumable(
            Source::new(Fault::Error(fault.clone().into())),
            &dest,
            &log,
            &CancellationToken::new(),
        ))
        .await;
        assert!(
            matches!(status.error, Some(DownloadError::Panicked { .. })),
            "{fault:?}: {:?}",
            status.error
        );
        assert!(tmp_path(&dest).exists(), "{fault:?}");
        assert!(
            std::fs::read(state_path_for(&dest)).unwrap().is_empty(),
            "{fault:?}"
        );
        assert_eq!(status.kept_for_resume, None, "{fault:?}");
        assert_eq!(
            status.leftover_temp_file,
            Some(tmp_path(&dest)),
            "{fault:?}"
        );
        assert_unlocked(&dest);
    }
}

/// A new start that fails, is cancelled or panics before its header is
/// durable discards; when that discard's invalidation cuts the state but
/// fails to sync it, both files stay, the state is empty, and only the
/// partial file left is reported.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_new_start_whose_discard_fails_to_sync_leaves_both() {
    #[derive(Debug, Clone, Copy)]
    enum End {
        Failed,
        Cancelled,
        Panicked,
    }
    for end in [End::Failed, End::Cancelled, End::Panicked] {
        let (_dir, dest) = dest();
        let log = StepLog::default();
        log.fail(Step::SyncInvalidation, When::Nth(1));
        let token = CancellationToken::new();
        let task = match end {
            End::Failed => {
                log.fail(Step::SyncState, When::Nth(1));
                resumable(Source::new(Fault::None), &dest, &log, &token)
            }
            End::Panicked => {
                log.panic(Step::SyncState, When::Nth(1));
                resumable(Source::new(Fault::None), &dest, &log, &token)
            }
            End::Cancelled => {
                let (reached, release) = log.gate(Step::SyncPartial, When::Nth(1));
                let task = resumable(Source::new(Fault::None), &dest, &log, &token);
                entered(reached).await;
                token.cancel();
                release.send(()).unwrap();
                task
            }
        };
        let status = finish(task).await;
        match end {
            End::Failed => assert!(
                matches!(
                    status.error,
                    Some(DownloadError::Io {
                        op: IoOp::Write,
                        ..
                    })
                ),
                "{:?}",
                status.error
            ),
            End::Cancelled => assert_eq!(status.state, TaskState::Cancelled),
            End::Panicked => assert!(
                matches!(status.error, Some(DownloadError::Panicked { .. })),
                "{:?}",
                status.error
            ),
        }
        assert!(tmp_path(&dest).exists(), "{end:?}");
        assert!(
            std::fs::read(state_path_for(&dest)).unwrap().is_empty(),
            "{end:?}"
        );
        assert_eq!(status.kept_for_resume, None, "{end:?}");
        assert_eq!(status.leftover_temp_file, Some(tmp_path(&dest)), "{end:?}");
        let steps = log.steps();
        assert_eq!(
            steps.last(),
            Some(&Step::SyncInvalidation),
            "{end:?}: {steps:?}"
        );
        assert_unlocked(&dest);
    }
}

/// A new start that fails before its header is durable discards; when its
/// discard panics after cutting the state, before syncing it, the panic
/// handler invalidates again: once that succeeds both files go; when its
/// cut fails, both stay, the state empty, and only the partial file left
/// is reported.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_new_start_retries_its_invalidation_after_a_panic() {
    for second_cut_fails in [false, true] {
        let (_dir, dest) = dest();
        let log = StepLog::default();
        log.fail(Step::SyncState, When::Nth(1));
        log.panic(Step::SyncInvalidation, When::Nth(1));
        if second_cut_fails {
            log.fail(Step::Invalidate, When::Nth(2));
        }
        let status = finish(resumable(
            Source::new(Fault::None),
            &dest,
            &log,
            &CancellationToken::new(),
        ))
        .await;
        let case = format!("second cut fails: {second_cut_fails}");
        assert!(
            matches!(status.error, Some(DownloadError::Panicked { .. })),
            "{case}: {:?}",
            status.error
        );
        let steps = log.steps();
        let at = steps
            .iter()
            .position(|&s| s == Step::SyncInvalidation)
            .unwrap();
        assert_eq!(
            steps[at - 1..=at + 1],
            [Step::Invalidate, Step::SyncInvalidation, Step::Invalidate],
            "{case}: {steps:?}"
        );
        assert_eq!(status.kept_for_resume, None, "{case}");
        if second_cut_fails {
            assert!(tmp_path(&dest).exists(), "{case}");
            assert!(
                std::fs::read(state_path_for(&dest)).unwrap().is_empty(),
                "{case}"
            );
            assert_eq!(status.leftover_temp_file, Some(tmp_path(&dest)), "{case}");
            assert_eq!(steps.last(), Some(&Step::Invalidate), "{case}: {steps:?}");
        } else {
            assert!(
                !tmp_path(&dest).exists() && !state_path_for(&dest).exists(),
                "{case}"
            );
            assert_eq!(status.leftover_temp_file, None, "{case}");
            assert_eq!(steps[at + 1..], DISCARD, "{case}: {steps:?}");
        }
        assert_unlocked(&dest);
    }
}

/// A task that may resume and prefers one connection goes through the
/// concurrent engine with one connection when it can keep a state file
/// (design §11.5.8): its info says concurrent, no two ranges are open at
/// once, and its files follow the resumable order. It ends as one, and
/// when it fails it keeps a valid state.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_single_connection_task_that_may_resume_goes_through_ranges() {
    for fails in [false, true] {
        let (_dir, dest) = dest();
        let log = StepLog::default();
        let fault = if fails {
            Fault::Error(UnretryableError::NotFound { status: 404 }.into())
        } else {
            Fault::None
        };
        let source = Source::new(fault);
        let (content, peak) = (source.content.clone(), source.peak.clone());
        let task = builder(
            DownloadMode::Singleton,
            source,
            &dest,
            &CancellationToken::new(),
        )
        .meta(meta())
        .options(TaskOptions::default().checkpoint_bytes(std::num::NonZeroU64::new(BLOCK).unwrap()))
        .resume(Resume::On)
        .file_hooks(log.hooks())
        .spawn()
        .unwrap();
        let status = finish(task).await;
        let info = status.info.clone().expect("an info");
        assert_eq!(info.mode, DownloadMode::Concurrent, "{fails}");
        assert_eq!(peak.load(Ordering::SeqCst), 1, "one connection: {fails}");
        let steps = log.steps();
        assert_eq!(steps[..5], NEW_START, "{steps:?}");
        if fails {
            assert_eq!(status.state, TaskState::Failed);
            assert_eq!(status.kept_for_resume, Some(tmp_path(&dest)));
            assert_eq!(assert_kept_state(&dest), 2);
            assert_checkpoints(&steps[5..]);
        } else {
            assert_eq!(status.state, TaskState::Finished, "{:?}", status.error);
            assert_eq!(std::fs::read(&dest).unwrap(), content);
            assert!(!tmp_path(&dest).exists() && !state_path_for(&dest).exists());
            assert_eq!(steps[steps.len() - 6..], COMPLETION);
            assert_checkpoints(&steps[5..steps.len() - 6]);
        }
        assert_unlocked(&dest);
    }
}

/// A task with `Resume::Off` that prefers one connection stays with one
/// connection's full download: no range is requested, no file step taken.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_single_connection_task_without_resume_is_unchanged() {
    let (_dir, dest) = dest();
    let log = StepLog::default();
    let source = Source::new(Fault::None);
    let (content, peak) = (source.content.clone(), source.peak.clone());
    let task = builder(
        DownloadMode::Singleton,
        source,
        &dest,
        &CancellationToken::new(),
    )
    .meta(meta())
    .file_hooks(log.hooks())
    .spawn()
    .unwrap();
    let status = finish(task).await;
    assert_eq!(status.state, TaskState::Finished);
    assert_eq!(status.info.clone().unwrap().mode, DownloadMode::Singleton);
    assert_eq!(peak.load(Ordering::SeqCst), 0, "no range");
    assert_eq!(std::fs::read(&dest).unwrap(), content);
    assert_eq!(log.steps(), []);
}

/// A source whose first range fails, retryably, past [`HALF`], and whose
/// first stream holds its destruction at a gate; it counts the live
/// streams and reports every range request.
struct HeldDrop {
    content: Bytes,
    first: std::sync::atomic::AtomicBool,
    live: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    requested: std::sync::Mutex<std::sync::mpsc::Sender<()>>,
    /// Reports that the first stream's destruction began, and holds it
    /// until the test sends or drops the release.
    entered: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>,
    release: Release,
}

/// The release a held destruction waits for.
type Release = Arc<std::sync::Mutex<std::sync::mpsc::Receiver<()>>>;

/// A live stream, until its destruction ends.
struct Live {
    live: Arc<AtomicUsize>,
    /// Report that the destruction began, then wait for the release.
    hold: Option<(std::sync::mpsc::Sender<()>, Release)>,
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Some((entered, release)) = self.hold.take() {
            let _ = entered.send(());
            let _ = release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(30));
        }
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}

impl BoltLoadAdapter for HeldDrop {
    async fn is_range_stream_available(&self) -> bool {
        true
    }

    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError> {
        Ok(meta())
    }

    async fn full_stream(&self) -> Result<FullResponse, AdapterError> {
        unreachable!("the task goes through ranges")
    }

    async fn range_stream(&self, start: u64, end: u64) -> Result<AnyBytesStream, AdapterError> {
        let _ = self.requested.lock().unwrap().send(());
        let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(live, Ordering::SeqCst);
        let first = self.first.swap(false, Ordering::SeqCst);
        let guard = Arc::new(Live {
            live: self.live.clone(),
            hold: first.then(|| {
                (
                    self.entered.lock().unwrap().take().unwrap(),
                    self.release.clone(),
                )
            }),
        });
        let content = self.content.clone();
        Ok(Box::pin(futures::stream::unfold(start, move |at| {
            let content = content.clone();
            let guard = guard.clone();
            async move {
                let _guard = guard;
                if at >= end {
                    return None;
                }
                if first && at >= HALF {
                    let error = AdapterError::Retryable {
                        source: crate::adapter::RetryableError::Io {
                            source: Arc::new(io::Error::other("reset")),
                        },
                    };
                    return Some((Err(error), end));
                }
                let mut next = (at + FRAME).min(end);
                if first && at < HALF && next > HALF {
                    next = HALF;
                }
                Some((Ok(content.slice(at as usize..next as usize)), next))
            }
        })))
    }
}

/// A routed task keeps one connection through a retry: the stream of the
/// failed attempt is destroyed before its runner reports the stop, so the
/// retry waits for it. While its destruction is held, no other range is
/// requested; at most one stream is ever alive. A spare worker lets the
/// manager go on while the destruction blocks its thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_a_retry_waits_for_the_old_stream() {
    let (_dir, dest) = dest();
    let (requested_tx, requested) = std::sync::mpsc::channel();
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let source = HeldDrop {
        content: SimpleTestAdapter::new(SIZE as usize).content().clone(),
        first: std::sync::atomic::AtomicBool::new(true),
        live: Arc::default(),
        peak: Arc::default(),
        requested: std::sync::Mutex::new(requested_tx),
        entered: std::sync::Mutex::new(Some(entered_tx)),
        release: Arc::new(std::sync::Mutex::new(release_rx)),
    };
    let (content, peak) = (source.content.clone(), source.peak.clone());
    let task = builder(
        DownloadMode::Singleton,
        source,
        &dest,
        &CancellationToken::new(),
    )
    .meta(meta())
    .resume(Resume::On)
    .spawn()
    .unwrap();
    let held = tokio::task::spawn_blocking(move || {
        requested
            .recv_timeout(Duration::from_secs(30))
            .expect("the first range");
        entered
            .recv_timeout(Duration::from_secs(30))
            .expect("the first stream is destroyed");
        // Well past the retry's backoff: no range while the old stream
        // lives. Nothing signals that none comes, so this waits.
        let second = requested.recv_timeout(Duration::from_secs(3));
        (second.is_err(), requested)
    })
    .await
    .unwrap();
    assert!(held.0, "a range was requested while the old stream lived");
    assert_eq!(peak.load(Ordering::SeqCst), 1);
    release.send(()).unwrap();
    let status = finish(task).await;
    assert_eq!(status.state, TaskState::Finished, "{:?}", status.error);
    assert_eq!(std::fs::read(&dest).unwrap(), content);
    assert_eq!(peak.load(Ordering::SeqCst), 1, "one stream at a time");
}

/// A quoted strong entity tag of `len` bytes.
fn tag_of(len: usize) -> Validator {
    Validator::StrongEtag(format!("\"{}\"", "v".repeat(len - 2)))
}

/// More metadata that cannot resume: content-coded, and a validator one
/// byte over the header's limit. A task with `Resume::On` ends exactly as
/// with `Resume::Off`, preferring one connection or not; a validator of
/// exactly 1024 bytes resumes.
#[tokio::test(flavor = "multi_thread")]
async fn test_coded_or_oversized_metadata_cannot_resume() {
    let coded = meta().with_content_encoding(Some("gzip".to_owned()));
    let oversized = BoltLoadAdapterMeta::new(SIZE).with_validator(Some(tag_of(1025)));
    for (case, meta) in [
        ("content-coded", coded),
        ("a validator of 1025 bytes", oversized),
    ] {
        for mode in [DownloadMode::Singleton, DownloadMode::Concurrent] {
            for fails in [false, true] {
                let off = outcome(Resume::Off, meta.clone(), mode, true, fails).await;
                let on = outcome(Resume::On, meta.clone(), mode, true, fails).await;
                assert_eq!(on, off, "{case}, {mode:?}, fails: {fails}");
                assert!(
                    !off.partial && !off.state_file && off.steps.is_empty(),
                    "{case}"
                );
            }
        }
    }
    for mode in [DownloadMode::Singleton, DownloadMode::Concurrent] {
        let limit = BoltLoadAdapterMeta::new(SIZE).with_validator(Some(tag_of(1024)));
        let on = outcome(Resume::On, limit, mode, true, false).await;
        assert_eq!(on.state, TaskState::Finished, "{mode:?}");
        assert_eq!(on.mode, Some(DownloadMode::Concurrent), "{mode:?}");
        assert_eq!(
            on.steps[..5],
            NEW_START,
            "a validator of 1024 bytes resumes"
        );
    }
}
