//! What the cleanup of one attempt needs, kept outside its unwind boundary
//! (design §11.5.4, §11.5.9; task-api-spec TASK-8).
//!
//! The attempt records here, as it goes, the destination's lock, the
//! temporary file it owns and its phase. When the attempt panics, the
//! handler waits until every worker that holds the temporary file has
//! exited, cleans up as the phase requires and releases the lock, all from
//! outside the unwound attempt. The wait depends only on [`WaitGroup`]
//! clones: the workers run on thread pools and drop their clones as they
//! exit, so nothing destroyed by the unwind is needed.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use async_waitgroup::WaitGroup;

use super::{destination_lock::DestinationLock, output_file};
use crate::task::Progress;

/// How far an attempt got, as its cleanup sees it (design §11.5.9). The
/// phases of a resumable task arrive with C33.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// No lock and no file yet (A): nothing to clean up.
    Unowned,
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
}

/// The cleanup state of one attempt; shared between the attempt and its
/// panic handler.
#[derive(Debug)]
pub(crate) struct Ownership {
    state: Mutex<State>,
    /// Cloned into every worker that holds the temporary file; taken by
    /// the panic handler to wait for them.
    drain: Mutex<Option<WaitGroup>>,
}

/// What the panic handler did.
#[derive(Debug)]
pub(crate) struct PanicCleanup {
    pub(crate) phase: Phase,
    /// The temporary file that could not be removed.
    pub(crate) leftover_temp_file: Option<PathBuf>,
    /// The completed progress, when the temporary file had been renamed:
    /// the task finished.
    pub(crate) published: Option<Progress>,
}

impl Ownership {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                phase: Phase::Unowned,
                lock: None,
                tmp: None,
                published: None,
            }),
            drain: Mutex::new(Some(WaitGroup::new())),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
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
    #[cfg(test)]
    pub(crate) fn phase(&self) -> Phase {
        self.state().phase
    }

    /// Give up the temporary file, then remove the lock file and release
    /// the lock, after the last operation on the temporary file. Blocking,
    /// briefly.
    ///
    /// The task owns nothing afterwards, so a later panic cleanup touches
    /// no file: by then another task may own the temporary path.
    pub(crate) fn release(&self) {
        let lock = {
            let mut state = self.state();
            state.phase = Phase::Unowned;
            state.tmp = None;
            state.lock.take()
        };
        if let Some(lock) = lock {
            lock.release();
        }
    }

    /// After a panic, with the attempt's token cancelled: wait until every
    /// worker that holds the temporary file has exited, then clean up as
    /// the phase requires and release the lock. Blocking.
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
        let (phase, tmp, published) = {
            let state = self.state();
            (state.phase, state.tmp.clone(), state.published.clone())
        };
        let leftover_temp_file = match (phase, tmp) {
            (Phase::Transfer, Some(tmp)) => output_file::discard_blocking(&tmp),
            _ => None,
        };
        self.release();
        PanicCleanup {
            phase,
            leftover_temp_file,
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
}
