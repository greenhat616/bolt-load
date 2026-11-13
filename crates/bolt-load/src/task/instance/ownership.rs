//! What the cleanup of one attempt needs, kept outside its unwind boundary
//! (design §11.5.4, §11.5.9; task-api-spec TASK-8).
//!
//! The attempt records here, as it goes, the destination's lock, the
//! temporary file it owns and its phase. When the attempt panics, the
//! handler waits until every worker that holds the temporary or the state
//! file has exited, cleans up as the phase requires and releases the lock,
//! all from outside the unwound attempt. The wait depends only on
//! [`WaitGroup`] clones: the workers run on thread pools and drop their
//! clones as they exit, so nothing destroyed by the unwind is needed.

use std::{
    io,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use async_waitgroup::WaitGroup;
use bolt_load_utils::telemetry::*;

use super::{
    destination_lock::DestinationLock,
    file_steps::{FileHooks, HookedFile, Step},
    output_file,
    state_file::{LockedFile, StateIo, sync_parent_dir},
};
use crate::task::Progress;

/// How far an attempt got, as its cleanup sees it (design §11.5.9). The
/// phase of a resume being checked (B2) arrives with C20.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// No lock and no file yet (A): nothing to clean up.
    Unowned,
    /// A resumable new start, its state header not yet durable (B1): a
    /// failure discards both files.
    Fresh,
    /// A resumable transfer, its state header durable (C): a failure keeps
    /// the partial file and the state file for a later resume.
    Resumable,
    /// The lock is held and the temporary file is the task's (D): a
    /// failure removes the temporary file.
    Transfer,
    /// The temporary file was renamed to the destination (P): the task
    /// finished, whatever happens after.
    Published,
}

#[derive(Debug)]
struct State {
    phase: Phase,
    lock: Option<DestinationLock>,
    tmp: Option<PathBuf>,
    /// The completed progress, once the temporary file was renamed; kept
    /// after the release, as the task's outcome.
    published: Option<Progress>,
    /// The task wrote a state file: its end syncs the directory after it
    /// removes it (design §11.5.5).
    durable: bool,
    /// The task keeps the partial file and the state file: its release
    /// leaves the state file.
    keep: bool,
    /// The state file was cut by an invalidation: whatever the files are,
    /// they are no usable pair any more, even if the invalidation never
    /// ended (design §11.5.9; task-api-spec STATUS-13).
    cut: bool,
}

/// The cleanup state of one attempt; shared between the attempt and its
/// panic handler.
#[derive(Debug)]
pub(crate) struct Ownership {
    state: Mutex<State>,
    /// Cloned into every worker that holds the temporary file; taken by
    /// the panic handler to wait for them.
    drain: Mutex<Option<WaitGroup>>,
    /// Called before each file step of a resumable task; tests fail them.
    hooks: FileHooks,
}

/// What the panic handler did.
#[derive(Debug)]
pub(crate) struct PanicCleanup {
    pub(crate) phase: Phase,
    /// The temporary file that could not be removed.
    pub(crate) leftover_temp_file: Option<PathBuf>,
    /// The partial file kept, with the state file, for a later resume.
    pub(crate) kept_for_resume: Option<PathBuf>,
    /// The completed progress, when the temporary file had been renamed:
    /// the task finished.
    pub(crate) published: Option<Progress>,
}

/// How a discard of a resumable task's files went (design §11.5.5).
#[derive(Debug)]
pub(crate) struct Discarded {
    /// The invalidation of the state file, which must be durable before
    /// the partial file is removed.
    pub(crate) invalidation: Invalidation,
    /// The partial file, when it is left: it could not be removed, or the
    /// invalidation failed and it was kept on purpose.
    pub(crate) leftover: Option<PathBuf>,
}

/// How an invalidation of the state file went: a cut to nothing, then its
/// sync.
#[derive(Debug)]
pub(crate) enum Invalidation {
    /// Both done: no state any more.
    Done,
    /// The cut failed: the state is as it was, its header intact.
    Intact(io::Error),
    /// The cut was made, its sync failed: the state is empty now, and a
    /// power loss may bring it back. It names no usable pair either way.
    Unsynced(io::Error),
}

impl Ownership {
    #[cfg(test)]
    pub(crate) fn new() -> Arc<Self> {
        Self::with_hooks(FileHooks::default())
    }

    /// An ownership whose file steps call `hooks` first.
    pub(crate) fn with_hooks(hooks: FileHooks) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                phase: Phase::Unowned,
                lock: None,
                tmp: None,
                published: None,
                durable: false,
                keep: false,
                cut: false,
            }),
            drain: Mutex::new(Some(WaitGroup::new())),
            hooks,
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The hooks of the file steps.
    pub(crate) fn hooks(&self) -> &FileHooks {
        &self.hooks
    }

    /// A clone of the drain group for a worker that holds the temporary
    /// file; it must keep it until it closed the file.
    pub(crate) fn drain(&self) -> WaitGroup {
        self.drain
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(WaitGroup::new)
            .clone()
    }

    /// The task holds `lock` and owns the temporary file `tmp`.
    pub(crate) fn own(&self, lock: DestinationLock, tmp: PathBuf) {
        let mut state = self.state();
        state.lock = Some(lock);
        state.tmp = Some(tmp);
        state.phase = Phase::Transfer;
    }

    /// Enter `phase`; [`Phase::Fresh`] marks the task as one that writes a
    /// state file.
    pub(crate) fn enter(&self, phase: Phase) {
        let mut state = self.state();
        if phase == Phase::Fresh {
            state.durable = true;
        }
        state.phase = phase;
    }

    /// Run `f` on the held lock.
    pub(crate) fn with_lock<T>(&self, f: impl FnOnce(&DestinationLock) -> T) -> Option<T> {
        self.state().lock.as_ref().map(f)
    }

    /// The temporary file was renamed to the destination, which holds
    /// `completed`.
    pub(crate) fn publish(&self, completed: Progress) {
        let mut state = self.state();
        state.phase = Phase::Published;
        state.published = Some(completed);
    }

    /// The phase the attempt reached.
    pub(crate) fn phase(&self) -> Phase {
        self.state().phase
    }

    /// The state file's locked handle, each step announced to the hooks.
    pub(crate) fn state_file(&self) -> Option<HookedFile> {
        let handle = self.with_lock(DestinationLock::handle).flatten()?;
        Some(HookedFile {
            file: LockedFile(handle),
            hooks: self.hooks.clone(),
        })
    }

    /// The task keeps the partial file and the state file: the release
    /// leaves the state file.
    pub(crate) fn keep_files(&self) {
        self.state().keep = true;
    }

    /// Invalidate the state file through the locked handle: cut it to
    /// nothing, then sync it, each step announced to the hooks. Blocking.
    pub(crate) fn invalidate_state(&self) -> Invalidation {
        let Some(HookedFile { mut file, .. }) = self.state_file() else {
            return Invalidation::Done;
        };
        if let Err(e) = self
            .hooks
            .before(Step::Invalidate)
            .and_then(|()| file.set_len(0))
        {
            return Invalidation::Intact(e);
        }
        // Recorded before the sync and its hook: a panic from here on must
        // not report the pair as resumable.
        self.state().cut = true;
        if let Err(e) = self
            .hooks
            .before(Step::SyncInvalidation)
            .and_then(|()| file.sync())
        {
            return Invalidation::Unsynced(e);
        }
        Invalidation::Done
    }

    /// Discard a resumable task's files, in the order of design §11.5.5:
    /// invalidate the state file, then remove the partial file and sync
    /// the directory; the state file goes with the release. Blocking.
    ///
    /// When the invalidation fails, the partial file stays: removing it
    /// would leave a state that may name a missing file. The task then
    /// keeps both files, and the next resume checks them.
    pub(crate) fn discard_resumable(&self) -> Discarded {
        let tmp = self.state().tmp.clone();
        let invalidation = self.invalidate_state();
        if !matches!(invalidation, Invalidation::Done) {
            warn!(
                "failed to invalidate the state file, so the partial file is kept: \
                 {invalidation:?}"
            );
            self.keep_files();
            return Discarded {
                invalidation,
                leftover: tmp,
            };
        }
        self.enter(Phase::Transfer);
        let Some(tmp) = tmp else {
            return Discarded {
                invalidation,
                leftover: None,
            };
        };
        let leftover = match self.hooks.before(Step::RemovePartial) {
            Ok(()) => output_file::discard_blocking(&tmp),
            Err(e) => {
                warn!("failed to remove temporary file {tmp:?}: {e}");
                Some(tmp.clone())
            }
        };
        if let Err(e) = self
            .hooks
            .before(Step::SyncDirAfterPartial)
            .and_then(|()| sync_parent_dir(&tmp))
        {
            warn!("failed to sync the directory of {tmp:?}: {e}");
        }
        Discarded {
            invalidation,
            leftover,
        }
    }

    /// The completion steps after the rename (design §11.5.5, steps 3 to
    /// 5): sync the destination's directory, cut the state file and sync
    /// it. Each failure is logged and the next step taken; the task has
    /// finished. Blocking.
    pub(crate) fn complete_resumable(&self) {
        let tmp = self.state().tmp.clone();
        if let Some(tmp) = &tmp
            && let Err(e) = self
                .hooks
                .before(Step::SyncDirAfterRename)
                .and_then(|()| sync_parent_dir(tmp))
        {
            warn!("failed to sync the directory of {tmp:?} after the rename: {e}");
        }
        if let Some(mut state) = self.state_file() {
            if let Err(e) = state.set_len(0) {
                warn!("failed to empty the state file after the rename: {e}");
            }
            if let Err(e) = state.sync() {
                warn!("failed to sync the state file after the rename: {e}");
            }
        }
    }

    /// Give up the temporary file, then release the lock, after the last
    /// operation on the temporary and the state file. Blocking, briefly.
    ///
    /// A task that keeps its files ([`Self::keep_files`]) leaves the state
    /// file; one that wrote a state file and is done with it removes it and
    /// syncs the directory (design §11.5.5); any other removes the lock
    /// file.
    ///
    /// The task owns nothing afterwards, so a later panic cleanup touches
    /// no file: by then another task may own the temporary path.
    pub(crate) fn release(&self) {
        let (lock, keep, durable) = {
            let mut state = self.state();
            state.phase = Phase::Unowned;
            state.tmp = None;
            (state.lock.take(), state.keep, state.durable)
        };
        let Some(lock) = lock else {
            return;
        };
        if keep {
            lock.release_keeping();
        } else if durable {
            lock.release_synced(&self.hooks);
        } else {
            lock.release();
        }
    }

    /// After a panic, with the attempt's token cancelled: wait until every
    /// worker that holds the temporary or the state file has exited, then
    /// clean up as the phase requires and release the lock. Blocking.
    ///
    /// The wait has no bound: a worker stuck in a file operation keeps the
    /// cleanup, and with it the lock, pending (design §11.5.4).
    ///
    /// Not safe against a concurrent [`Self::release`]: the cleanup reads
    /// the phase and the temporary path, then removes the file, so a
    /// release in between would let it remove a file another task owns by
    /// then. Nothing calls both at once: the task's driver calls this only
    /// after the attempt has unwound, from the same thread that would
    /// otherwise have called `release`.
    pub(crate) fn clean_up_after_panic(&self) -> PanicCleanup {
        let drain = self
            .drain
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(drain) = drain {
            drain.wait_blocking();
        }
        let (phase, tmp, published, cut) = {
            let state = self.state();
            (
                state.phase,
                state.tmp.clone(),
                state.published.clone(),
                state.cut,
            )
        };
        let (leftover_temp_file, kept_for_resume) = match (phase, tmp) {
            // A new start not yet resumable: discard (design §11.5.9, B1).
            // When the invalidation fails, both stay, and the partial file
            // is reported as left.
            (Phase::Fresh, Some(_)) => (self.discard_resumable().leftover, None),
            // Both kept, as the last checkpoint left them; no teardown
            // checkpoint (design §11.5.9, C).
            // An invalidation cut the state before the panic: both stay, as
            // a failed invalidation leaves them, but they are no usable pair.
            (Phase::Resumable, tmp) if cut => {
                self.keep_files();
                (tmp, None)
            }
            (Phase::Resumable, tmp) => {
                self.keep_files();
                (None, tmp)
            }
            (Phase::Transfer, Some(tmp)) => (output_file::discard_blocking(&tmp), None),
            // The task finished: the rest of the completion (design
            // §11.5.5).
            (Phase::Published, _) if self.state().durable => {
                self.complete_resumable();
                (None, None)
            }
            _ => (None, None),
        };
        self.release();
        PanicCleanup {
            phase,
            leftover_temp_file,
            kept_for_resume,
            published,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        time::Duration,
    };

    use super::*;
    use crate::task::{
        DownloadError,
        instance::destination_lock::{LockHooks, Unsupported, state_path_for},
    };

    fn lock(dest: &std::path::Path) -> Result<DestinationLock, DownloadError> {
        DestinationLock::acquire(dest, Unsupported::Proceed, &LockHooks::default())
    }

    /// After a panic the lock stays held, and the temporary file stays,
    /// until the last worker that holds the file has exited.
    #[test]
    fn test_the_lock_waits_for_the_workers() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("file.bin");
        let tmp = dir.path().join("file.bin.downloading");
        std::fs::write(&tmp, b"partial").unwrap();
        let ownership = Ownership::new();
        ownership.own(lock(&dest).unwrap(), tmp.clone());

        let (release_tx, release_rx) = mpsc::channel::<()>();
        let exited = Arc::new(AtomicBool::new(false));
        let worker = std::thread::spawn({
            let drain = ownership.drain();
            let exited = exited.clone();
            move || {
                let _ = release_rx.recv();
                exited.store(true, Ordering::SeqCst);
                drop(drain);
            }
        });
        let handler = std::thread::spawn({
            let ownership = ownership.clone();
            move || ownership.clean_up_after_panic()
        });

        std::thread::sleep(Duration::from_millis(100));
        assert!(!handler.is_finished());
        assert!(matches!(lock(&dest), Err(DownloadError::InUse { .. })));
        assert!(tmp.exists());

        release_tx.send(()).unwrap();
        worker.join().unwrap();
        let cleanup = handler.join().unwrap();
        assert!(exited.load(Ordering::SeqCst));
        assert_eq!(cleanup.phase, Phase::Transfer);
        assert_eq!(cleanup.leftover_temp_file, None);
        assert!(!tmp.exists());
        assert!(!state_path_for(&dest).exists());
        lock(&dest).unwrap().release();
    }

    /// Without a lock nothing is cleaned up; once published, the
    /// destination's temporary path is not touched.
    #[test]
    #[expect(clippy::single_range_in_vec_init, reason = "a list holding one range")]
    fn test_the_cleanup_follows_the_phase() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("file.bin.downloading");
        std::fs::write(&tmp, b"someone else's").unwrap();
        let cleanup = Ownership::new().clean_up_after_panic();
        assert_eq!(cleanup.phase, Phase::Unowned);
        assert!(tmp.exists());

        let dest = dir.path().join("file.bin");
        let ownership = Ownership::new();
        ownership.own(lock(&dest).unwrap(), tmp.clone());
        let completed = Progress::new(Some(7), 7, vec![0..7]);
        ownership.publish(completed.clone());
        let cleanup = ownership.clean_up_after_panic();
        assert_eq!(cleanup.phase, Phase::Published);
        assert_eq!(cleanup.published, Some(completed));
        assert!(tmp.exists());
        assert!(!state_path_for(&dest).exists());
    }

    /// Once released, the task owns nothing: a panic cleanup afterwards
    /// leaves the temporary path, which a successor may own by then, and
    /// the successor's lock alone.
    #[test]
    fn test_a_cleanup_after_the_release_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("file.bin");
        let tmp = dir.path().join("file.bin.downloading");
        let ownership = Ownership::new();
        ownership.own(lock(&dest).unwrap(), tmp.clone());
        ownership.release();
        assert_eq!(ownership.phase(), Phase::Unowned);

        let successor = lock(&dest).unwrap();
        std::fs::write(&tmp, b"the successor's").unwrap();
        let cleanup = ownership.clean_up_after_panic();
        assert_eq!(cleanup.phase, Phase::Unowned);
        assert_eq!(cleanup.leftover_temp_file, None);
        assert_eq!(std::fs::read(&tmp).unwrap(), b"the successor's");
        assert!(matches!(lock(&dest), Err(DownloadError::InUse { .. })));
        successor.release();
    }

    /// An ownership of `dest` in `phase`, after a resumable new start
    /// wrote the partial file and a state header; with its step log.
    fn resumable(
        dest: &std::path::Path,
        tmp: &std::path::Path,
        phase: Phase,
        log: &super::super::file_steps::tests::StepLog,
    ) -> Arc<Ownership> {
        let ownership = Ownership::with_hooks(log.hooks());
        ownership.own(lock(dest).unwrap(), tmp.to_path_buf());
        ownership.enter(Phase::Fresh);
        std::fs::write(tmp, b"partial").unwrap();
        let mut state = ownership.state_file().unwrap();
        state.write_at(0, b"header").unwrap();
        ownership.enter(phase);
        ownership
    }

    /// TASK-8 by phase: a panic in a new start not yet resumable (B1)
    /// discards both files; in a resumable transfer (C) keeps both, the
    /// state file as it was, and releases the lock; after the rename (P)
    /// finishes the completion, the state file gone.
    #[test]
    fn test_a_panic_cleans_up_by_phase() {
        use super::super::file_steps::tests::{StepLog, When};

        for phase in [Phase::Fresh, Phase::Resumable, Phase::Published] {
            let dir = tempfile::tempdir().unwrap();
            let dest = dir.path().join("file.bin");
            let tmp = dir.path().join("file.bin.downloading");
            let log = StepLog::default();
            let ownership = resumable(&dest, &tmp, phase, &log);
            if phase == Phase::Published {
                ownership.publish(Progress::default());
            }
            let cleanup = ownership.clean_up_after_panic();
            assert_eq!(cleanup.phase, phase);
            let state = state_path_for(&dest);
            match phase {
                Phase::Fresh => {
                    assert!(!tmp.exists() && !state.exists());
                    assert_eq!(cleanup.kept_for_resume, None);
                    assert_eq!(
                        log.steps(),
                        [
                            Step::WriteState,
                            Step::Invalidate,
                            Step::SyncInvalidation,
                            Step::RemovePartial,
                            Step::SyncDirAfterPartial,
                            Step::RemoveState,
                            Step::SyncDirAfterState,
                        ]
                    );
                }
                Phase::Resumable => {
                    assert_eq!(std::fs::read(&tmp).unwrap(), b"partial");
                    assert_eq!(std::fs::read(&state).unwrap(), b"header");
                    assert_eq!(cleanup.kept_for_resume, Some(tmp.clone()));
                    assert_eq!(log.steps(), [Step::WriteState]);
                }
                _ => {
                    assert!(!state.exists());
                    assert_eq!(cleanup.kept_for_resume, None);
                    assert_eq!(
                        log.steps()[1..],
                        [
                            Step::SyncDirAfterRename,
                            Step::TruncateState,
                            Step::SyncState,
                            Step::RemoveState,
                            Step::SyncDirAfterState,
                        ]
                    );
                }
            }
            assert_eq!(cleanup.leftover_temp_file, None);
            lock(&dest).unwrap().release();
        }

        // B1, and the invalidation fails: both stay, the partial file is
        // reported as left, and the lock is released.
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("file.bin");
        let tmp = dir.path().join("file.bin.downloading");
        let log = StepLog::default();
        log.fail(Step::Invalidate, When::Nth(1));
        let ownership = resumable(&dest, &tmp, Phase::Fresh, &log);
        let cleanup = ownership.clean_up_after_panic();
        assert_eq!(cleanup.leftover_temp_file, Some(tmp.clone()));
        assert!(tmp.exists());
        assert_eq!(std::fs::read(state_path_for(&dest)).unwrap(), b"header");
        lock(&dest).unwrap().release();
    }
}
