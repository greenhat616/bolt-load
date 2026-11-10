//! The lock that keeps one task at a time on a destination (design §11.5.4,
//! [#152](https://github.com/greenhat616/bolt-load/issues/152)).
//!
//! Every task takes an exclusive lock on `<dest>.boltload` for its whole
//! life, after it resolved the destination and before it touches any file
//! there. The same file will hold the resume state (design §11.5.5); until
//! then it is empty, and a task that finds it with content invalidates it
//! under the lock before it touches the temporary file.
//!
//! Rules this module keeps:
//!
//! - The file is read and changed only through the locked handle: on Windows
//!   the lock is mandatory, so another handle, even of this process, could
//!   not read or write it.
//! - While the lock is held, the file is neither removed nor replaced: a
//!   task could otherwise create a new file at the path, lock it and work on
//!   the same temporary file. It is removed only after the last operation on
//!   the temporary file, and then the lock is released.
//! - A task can open the old file just before another removes it, and lock
//!   it just after. So, once locked, the path must still name the locked
//!   file; else the task retries.
//! - A task that did not get the lock never removes the file, even one it
//!   created: another task may hold the lock on it by then. It leaves an
//!   empty file, which the next task takes as having no state.

use std::{
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use bolt_load_utils::telemetry::*;
use fs4::{FileExt, TryLockError};
use same_file::Handle;

use crate::task::{DownloadError, IoOp};

/// The extension of the lock file, which will also hold the resume state.
pub(crate) const STATE_EXTENSION: &str = ".boltload";

/// How often a task opens the lock file again when the locked file is no
/// longer the one at the path, or opening it failed transiently, before it
/// reports the destination in use.
const IDENTITY_RETRIES: usize = 3;

/// How long to wait before opening again after a transient failure: a file
/// being deleted on Windows goes once its last handle closes.
const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);

/// The lock file for `dest`; it always differs from `dest`.
pub(crate) fn state_path_for(dest: &Path) -> PathBuf {
    let mut path = dest.as_os_str().to_os_string();
    path.push(STATE_EXTENSION);
    PathBuf::from(path)
}

/// Whether a task without the lock may go on when the file system cannot
/// lock files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unsupported {
    /// Warn and go on unlocked: a task that keeps no state for resuming.
    Proceed,
    /// Fail: a task that resumes needs the lock.
    #[cfg_attr(not(test), expect(dead_code, reason = "resume arrives with C33"))]
    Fail,
}

/// The exclusive lock on a destination's lock file.
#[derive(Debug)]
pub(crate) struct DestinationLock {
    path: PathBuf,
    /// The locked handle; `None` when the file system cannot lock files and
    /// the task goes on unlocked.
    file: Option<File>,
    /// Whether this task created the file.
    created: bool,
}

/// One way to try the lock, replaced in tests.
type TryLock = dyn Fn(&File) -> Result<(), TryLockError> + Send + Sync;

/// What to call between opening and locking the file; tests change the
/// file at the path there.
type BeforeLock = dyn Fn(usize) + Send + Sync;

/// One way to check that the path names the locked file, replaced in
/// tests.
type Identity = dyn Fn(&Path, &File) -> io::Result<bool> + Send + Sync;

/// How the lock is taken; the defaults lock for real.
#[derive(Clone, Default)]
pub(crate) struct LockHooks {
    try_lock: Option<Arc<TryLock>>,
    before_lock: Option<Arc<BeforeLock>>,
    identity: Option<Arc<Identity>>,
}

impl LockHooks {
    /// Lock with `try_lock` instead of the file system's lock.
    #[cfg(test)]
    pub(crate) fn try_lock(
        mut self,
        f: impl Fn(&File) -> Result<(), TryLockError> + Send + Sync + 'static,
    ) -> Self {
        self.try_lock = Some(Arc::new(f));
        self
    }

    /// Call `f` with the attempt's number between opening and locking.
    #[cfg(test)]
    pub(crate) fn before_lock(mut self, f: impl Fn(usize) + Send + Sync + 'static) -> Self {
        self.before_lock = Some(Arc::new(f));
        self
    }

    /// Check the file's identity with `f` instead.
    #[cfg(test)]
    pub(crate) fn identity(
        mut self,
        f: impl Fn(&Path, &File) -> io::Result<bool> + Send + Sync + 'static,
    ) -> Self {
        self.identity = Some(Arc::new(f));
        self
    }

    fn names_file(&self, path: &Path, file: &File) -> io::Result<bool> {
        match &self.identity {
            Some(f) => f(path, file),
            None => names_file(path, file),
        }
    }

    fn lock(&self, file: &File) -> Result<(), TryLockError> {
        match &self.try_lock {
            Some(f) => f(file),
            // Not `file.try_lock()`: the inherent method needs Rust 1.89.
            None => FileExt::try_lock(file),
        }
    }
}

/// Why opening or locking failed.
enum Failure {
    /// Another task holds the lock.
    Contended,
    /// The file system cannot lock files.
    Unsupported(io::Error),
    /// Any other error, such as a missing permission.
    Io(io::Error),
}

impl DestinationLock {
    /// Take the lock for `dest`. Blocking.
    ///
    /// - [`DownloadError::InUse`] when another task holds it.
    /// - When the file system cannot lock files: with
    ///   [`Unsupported::Proceed`], a warning and an unlocked
    ///   [`DestinationLock`]; else [`DownloadError::Io`] with
    ///   [`IoOp::Lock`]. The lock file stays, empty if this task created
    ///   it.
    /// - [`DownloadError::Io`] with [`IoOp::Lock`] for any other error.
    pub(crate) fn acquire(
        dest: &Path,
        unsupported: Unsupported,
        hooks: &LockHooks,
    ) -> Result<Self, DownloadError> {
        let path = state_path_for(dest);
        // The last open that failed with an access denial taken for a
        // delete-pending file. A real permission denial can look the same
        // (the metadata query can be denied too), and it does not go away:
        // if the retries run out on it, it is reported as an error, not as
        // another task's lock.
        let mut denied = None;
        for attempt in 0..IDENTITY_RETRIES {
            let (file, created) = match open(&path) {
                Ok(opened) => opened,
                Err(OpenFailure::Retry(e)) => {
                    denied = e;
                    std::thread::sleep(RETRY_DELAY);
                    continue;
                }
                Err(OpenFailure::Io(e)) => return Err(lock_error(&path, e)),
            };
            denied = None;
            if let Some(f) = &hooks.before_lock {
                f(attempt);
            }
            match classify(hooks.lock(&file)) {
                Ok(()) => {}
                Err(Failure::Contended) => {
                    return Err(DownloadError::InUse {
                        path: dest.to_path_buf(),
                    });
                }
                Err(Failure::Unsupported(e)) if unsupported == Unsupported::Proceed => {
                    warn!(
                        "the file system cannot lock {}, so another task may download to the same \
                         destination: {e}",
                        path.display()
                    );
                    // Not removed: without the lock, it may be another
                    // task's by now.
                    return Ok(Self {
                        path,
                        file: None,
                        created,
                    });
                }
                Err(Failure::Unsupported(e) | Failure::Io(e)) => {
                    return Err(lock_error(&path, e));
                }
            }
            // The file was removed or replaced between opening and locking:
            // what is locked no longer guards the path.
            match hooks.names_file(&path, &file) {
                Ok(true) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                        created,
                    });
                }
                Ok(false) => {}
                Err(e) => {
                    unlock(&file);
                    return Err(lock_error(&path, e));
                }
            }
            debug!(
                "{} changed while it was locked, opening it again",
                path.display()
            );
            unlock(&file);
        }
        Err(exhausted(dest, &path, denied))
    }

    /// Whether this task created the lock file.
    #[cfg_attr(not(test), expect(dead_code, reason = "resume arrives with C33"))]
    pub(crate) fn created(&self) -> bool {
        self.created
    }

    /// Whether the lock is held; `false` when the task went on unlocked.
    #[cfg(test)]
    pub(crate) fn is_locked(&self) -> bool {
        self.file.is_some()
    }

    /// Empty the file through the locked handle and sync it, so no state
    /// left by an earlier task survives. Done before the temporary file is
    /// touched (design §11.5.5). Nothing to do when the task went on
    /// unlocked: without the lock, the file may be another task's.
    pub(crate) fn invalidate(&self) -> io::Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        if file.metadata()?.len() == 0 {
            return Ok(());
        }
        file.set_len(0)?;
        file.sync_all()
    }

    /// Remove the lock file, then release the lock. Call it only after the
    /// last operation on the temporary file. A failure to remove it is
    /// logged: the file is empty, and the next task takes it as having no
    /// state.
    pub(crate) fn release(self) {
        let Some(file) = self.file else {
            return;
        };
        remove(&self.path);
        unlock(&file);
    }
}

/// Why opening the lock file failed.
enum OpenFailure {
    /// Something transient: open it again. Holds the error when it was an
    /// access denial taken for a delete-pending file.
    Retry(Option<io::Error>),
    /// Anything else.
    Io(io::Error),
}

/// What [`DestinationLock::acquire`] reports when its attempts run out:
/// `InUse`, unless the last attempt failed to open the file with an access
/// denial, which a missing permission explains as well as a file being
/// deleted.
fn exhausted(dest: &Path, path: &Path, denied: Option<io::Error>) -> DownloadError {
    match denied {
        Some(e) => lock_error(path, e),
        None => DownloadError::InUse {
            path: dest.to_path_buf(),
        },
    }
}

/// Open the lock file, creating it if it is missing, and say whether it was
/// created.
fn open(path: &Path) -> Result<(File, bool), OpenFailure> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    match options.clone().create_new(true).open(path) {
        Ok(file) => return Ok((file, true)),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(classify_open(path, e)),
    }
    match options.open(path) {
        Ok(file) => Ok((file, false)),
        // Removed after `create_new` saw it: open it again.
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(OpenFailure::Retry(None)),
        Err(e) => Err(classify_open(path, e)),
    }
}

/// Whether `path` names the file `file` has open: `false` when it names
/// another file or none; an error when either could not be inspected.
fn names_file(path: &Path, file: &File) -> io::Result<bool> {
    let locked = Handle::from_file(file.try_clone()?)?;
    match Handle::from_path(path) {
        Ok(named) => Ok(locked == named),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) if is_delete_pending(path, &e) => Ok(false),
        Err(e) => Err(e),
    }
}

fn unlock(file: &File) {
    // Not `file.unlock()`: the inherent method needs Rust 1.89.
    if let Err(e) = FileExt::unlock(file) {
        debug!("failed to unlock: {e}");
    }
}

fn remove(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => warn!("failed to remove {}: {e}", path.display()),
    }
}

fn lock_error(path: &Path, source: io::Error) -> DownloadError {
    DownloadError::Io {
        op: IoOp::Lock,
        path: path.to_path_buf(),
        source: Arc::new(source),
    }
}

fn classify(result: Result<(), TryLockError>) -> Result<(), Failure> {
    match result {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(Failure::Contended),
        Err(TryLockError::Error(e)) if is_unsupported(&e) => Err(Failure::Unsupported(e)),
        Err(TryLockError::Error(e)) => Err(Failure::Io(e)),
    }
}

/// An error opening the lock file: another task, or one being removed,
/// can make it fail transiently.
fn classify_open(path: &Path, e: io::Error) -> OpenFailure {
    if is_transient_open(&e) {
        OpenFailure::Retry(None)
    } else if is_delete_pending(path, &e) {
        OpenFailure::Retry(Some(e))
    } else {
        OpenFailure::Io(e)
    }
}

/// Whether locking failed because the file system cannot lock files.
fn is_unsupported(e: &io::Error) -> bool {
    if e.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    let Some(code) = e.raw_os_error() else {
        return false;
    };
    #[cfg(unix)]
    {
        code == libc::ENOLCK || code == libc::EOPNOTSUPP || code == libc::ENOTSUP
    }
    #[cfg(windows)]
    {
        code == windows::ERROR_NOT_SUPPORTED || code == windows::ERROR_INVALID_FUNCTION
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = code;
        false
    }
}

/// Whether opening `path` failed with "access denied" because the file is
/// being deleted (Windows reports `ERROR_ACCESS_DENIED` for a delete-pending
/// file), not for a missing permission. A file with a real permission
/// denial can usually still be inspected without access to its data; a
/// delete-pending one cannot be opened at all, or no longer has a name.
/// The guess can be wrong when the metadata query is denied as well, so
/// [`DestinationLock::acquire`] reports a denial that outlasts its retries
/// as an error (see [`exhausted`]).
fn is_delete_pending(path: &Path, e: &io::Error) -> bool {
    #[cfg(windows)]
    {
        e.raw_os_error() == Some(windows::ERROR_ACCESS_DENIED)
            && match std::fs::metadata(path) {
                Ok(_) => false,
                Err(e) => {
                    e.kind() == io::ErrorKind::NotFound
                        || e.raw_os_error() == Some(windows::ERROR_ACCESS_DENIED)
                }
            }
    }
    #[cfg(not(windows))]
    {
        let _ = (path, e);
        false
    }
}

/// Whether opening failed because another handle is in the way: on
/// Windows, a file open without sharing, or one being deleted.
fn is_transient_open(e: &io::Error) -> bool {
    #[cfg(windows)]
    {
        matches!(
            e.raw_os_error(),
            Some(windows::ERROR_SHARING_VIOLATION | windows::ERROR_DELETE_PENDING)
        )
    }
    #[cfg(not(windows))]
    {
        let _ = e;
        false
    }
}

/// The Win32 error codes this module tells apart.
#[cfg(windows)]
mod windows {
    pub(super) const ERROR_INVALID_FUNCTION: i32 = 1;
    pub(super) const ERROR_ACCESS_DENIED: i32 = 5;
    pub(super) const ERROR_SHARING_VIOLATION: i32 = 32;
    pub(super) const ERROR_NOT_SUPPORTED: i32 = 50;
    pub(super) const ERROR_DELETE_PENDING: i32 = 303;
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    fn dest() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("file.bin");
        (dir, dest)
    }

    fn take(dest: &Path) -> Result<DestinationLock, DownloadError> {
        DestinationLock::acquire(dest, Unsupported::Proceed, &LockHooks::default())
    }

    #[test]
    fn test_state_path_appends_extension() {
        assert_eq!(
            state_path_for(Path::new("/tmp/dir/file.bin")),
            PathBuf::from("/tmp/dir/file.bin.boltload")
        );
    }

    /// The lock file is created, held, and removed on release; a second
    /// task meanwhile finds the destination in use.
    #[test]
    fn test_a_second_lock_is_in_use_until_released() {
        let (_dir, dest) = dest();
        let lock = take(&dest).unwrap();
        assert!(lock.is_locked() && lock.created());
        assert!(state_path_for(&dest).exists());
        assert!(matches!(
            take(&dest),
            Err(DownloadError::InUse { path }) if path == dest
        ));
        lock.release();
        assert!(!state_path_for(&dest).exists());
        let again = take(&dest).unwrap();
        assert!(again.created());
        again.release();
    }

    /// A file left by an earlier task is locked as it is, not created, and
    /// emptied on invalidation; an empty one stays as it is.
    #[test]
    fn test_a_stale_file_is_invalidated() {
        let (_dir, dest) = dest();
        let path = state_path_for(&dest);
        std::fs::File::create(&path)
            .unwrap()
            .write_all(b"stale state")
            .unwrap();
        let lock = take(&dest).unwrap();
        assert!(!lock.created());
        lock.invalidate().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        lock.invalidate().unwrap();
        lock.release();
        assert!(!path.exists());
    }

    /// A crash leaves an empty, unlocked file: the next task takes it.
    #[test]
    fn test_an_empty_leftover_is_taken() {
        let (_dir, dest) = dest();
        std::fs::File::create(state_path_for(&dest)).unwrap();
        let lock = take(&dest).unwrap();
        assert!(lock.is_locked() && !lock.created());
        lock.release();
    }

    /// The file at the path is replaced between opening and locking: the
    /// task notices that it locked a removed file, and locks the new one.
    #[test]
    fn test_a_replaced_file_is_opened_again() {
        let (_dir, dest) = dest();
        let path = state_path_for(&dest);
        let calls = Arc::new(AtomicUsize::new(0));
        let hooks = LockHooks::default().before_lock({
            let path = path.clone();
            let calls = calls.clone();
            move |attempt| {
                calls.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    std::fs::remove_file(&path).unwrap();
                    std::fs::File::create(&path).unwrap();
                }
            }
        });
        let lock = DestinationLock::acquire(&dest, Unsupported::Proceed, &hooks).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(names_file(&path, lock.file.as_ref().unwrap()).unwrap());
        assert!(matches!(take(&dest), Err(DownloadError::InUse { .. })));
        lock.release();
    }

    /// A file replaced on every attempt ends as in use.
    #[test]
    fn test_a_file_replaced_every_time_is_in_use() {
        let (_dir, dest) = dest();
        let path = state_path_for(&dest);
        let hooks = LockHooks::default().before_lock(move |_| {
            std::fs::remove_file(&path).unwrap();
            std::fs::File::create(&path).unwrap();
        });
        assert!(matches!(
            DestinationLock::acquire(&dest, Unsupported::Proceed, &hooks),
            Err(DownloadError::InUse { .. })
        ));
    }

    fn unsupported_error() -> io::Error {
        #[cfg(unix)]
        let code = libc::ENOLCK;
        #[cfg(windows)]
        let code = windows::ERROR_NOT_SUPPORTED;
        io::Error::from_raw_os_error(code)
    }

    /// A file system that cannot lock: a task that keeps no state goes on
    /// unlocked and leaves no lock file; one that must lock fails with
    /// `Io { op: Lock }`, not `InUse`.
    #[test]
    fn test_unsupported_locking() {
        let (_dir, dest) = dest();
        let hooks =
            LockHooks::default().try_lock(|_| Err(TryLockError::Error(unsupported_error())));
        let lock = DestinationLock::acquire(&dest, Unsupported::Proceed, &hooks).unwrap();
        assert!(!lock.is_locked());
        lock.invalidate().unwrap();
        lock.release();
        // Left, empty: without the lock, it is not this task's to remove.
        assert_eq!(std::fs::metadata(state_path_for(&dest)).unwrap().len(), 0);

        let failed = DestinationLock::acquire(&dest, Unsupported::Fail, &hooks);
        assert!(
            matches!(&failed, Err(DownloadError::Io { op: IoOp::Lock, path, .. }) if *path == state_path_for(&dest)),
            "{failed:?}"
        );
    }

    /// A task that created the lock file, but failed to lock it after
    /// another task did, leaves that task's lock in place: a third task
    /// still finds the destination in use.
    #[test]
    fn test_a_failed_lock_leaves_the_holder_s_file() {
        use std::sync::Barrier;

        for error in [
            io::Error::from(io::ErrorKind::PermissionDenied),
            unsupported_error(),
        ] {
            let (_dir, dest) = dest();
            let opened = Arc::new(Barrier::new(2));
            let held = Arc::new(Barrier::new(2));
            let error = Arc::new(std::sync::Mutex::new(Some(error)));
            let a = std::thread::spawn({
                let dest = dest.clone();
                let (opened, held) = (opened.clone(), held.clone());
                move || {
                    let hooks = LockHooks::default()
                        .before_lock(move |_| {
                            opened.wait();
                            held.wait();
                        })
                        .try_lock(move |_| {
                            Err(TryLockError::Error(error.lock().unwrap().take().unwrap()))
                        });
                    DestinationLock::acquire(&dest, Unsupported::Fail, &hooks)
                }
            });
            opened.wait();
            // A created the file; B locks it before A tries.
            let b = take(&dest).unwrap();
            assert!(!b.created());
            held.wait();
            assert!(matches!(
                a.join().unwrap(),
                Err(DownloadError::Io { op: IoOp::Lock, .. })
            ));
            assert!(state_path_for(&dest).exists());
            assert!(matches!(take(&dest), Err(DownloadError::InUse { .. })));
            b.release();
        }
    }

    /// An error inspecting the locked file is an error, not a false
    /// "in use"; a path that names no file or another file is retried.
    #[test]
    fn test_the_identity_check() {
        let (dir, dest) = dest();
        let path = state_path_for(&dest);
        let file = std::fs::File::create(&path).unwrap();
        assert!(names_file(&path, &file).unwrap());
        assert!(!names_file(&dir.path().join("other"), &file).unwrap());
        std::fs::File::create(dir.path().join("other")).unwrap();
        assert!(!names_file(&dir.path().join("other"), &file).unwrap());
    }

    /// "Access denied" on Windows is retried only for a file being deleted;
    /// a file that can still be inspected is a real permission error.
    #[cfg(windows)]
    #[test]
    fn test_access_denied_is_told_from_delete_pending() {
        let (dir, _) = dest();
        let denied = io::Error::from_raw_os_error(windows::ERROR_ACCESS_DENIED);
        let existing = dir.path().join("existing");
        std::fs::File::create(&existing).unwrap();
        assert!(!is_delete_pending(&existing, &denied));
        assert!(matches!(
            classify_open(&existing, denied),
            OpenFailure::Io(_)
        ));
        let gone = dir.path().join("gone");
        let denied = io::Error::from_raw_os_error(windows::ERROR_ACCESS_DENIED);
        assert!(is_delete_pending(&gone, &denied));
        assert!(matches!(
            classify_open(&gone, denied),
            OpenFailure::Retry(Some(_))
        ));
        let other = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(!is_delete_pending(&gone, &other));
        let sharing = io::Error::from_raw_os_error(windows::ERROR_SHARING_VIOLATION);
        assert!(matches!(
            classify_open(&gone, sharing),
            OpenFailure::Retry(None)
        ));
    }

    /// Attempts that run out on an access denial taken for a
    /// delete-pending file report an error, since a real permission denial
    /// looks the same; attempts that run out on anything else are `InUse`.
    #[test]
    fn test_running_out_on_an_access_denial_is_an_error() {
        let (_dir, dest) = dest();
        let path = state_path_for(&dest);
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(matches!(
            exhausted(&dest, &path, Some(denied)),
            DownloadError::Io { op: IoOp::Lock, path: p, .. } if p == path
        ));
        assert!(matches!(
            exhausted(&dest, &path, None),
            DownloadError::InUse { path: p } if p == dest
        ));
    }

    /// The identity check fails: `Io { op: Lock }`, not a false `InUse`,
    /// and the lock is released.
    #[test]
    fn test_an_identity_error_is_an_error() {
        let (_dir, dest) = dest();
        let hooks = LockHooks::default()
            .identity(|_, _| Err(io::Error::from(io::ErrorKind::PermissionDenied)));
        assert!(matches!(
            DestinationLock::acquire(&dest, Unsupported::Proceed, &hooks),
            Err(DownloadError::Io { op: IoOp::Lock, .. })
        ));
        take(&dest).unwrap().release();
    }

    /// Any other locking error fails both kinds of task with
    /// `Io { op: Lock }`.
    #[test]
    fn test_other_errors_fail() {
        let (_dir, dest) = dest();
        let hooks = LockHooks::default().try_lock(|_| {
            Err(TryLockError::Error(io::Error::from(
                io::ErrorKind::PermissionDenied,
            )))
        });
        for unsupported in [Unsupported::Proceed, Unsupported::Fail] {
            assert!(matches!(
                DestinationLock::acquire(&dest, unsupported, &hooks),
                Err(DownloadError::Io { op: IoOp::Lock, .. })
            ));
        }
        assert_eq!(std::fs::metadata(state_path_for(&dest)).unwrap().len(), 0);
    }

    /// The directory is missing: `Io { op: Lock }`.
    #[test]
    fn test_a_missing_directory_fails() {
        let (dir, _) = dest();
        let dest = dir.path().join("missing").join("file.bin");
        assert!(matches!(
            take(&dest),
            Err(DownloadError::Io { op: IoOp::Lock, .. })
        ));
    }
}
