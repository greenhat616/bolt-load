//! `discard_partial` (design §11.5.2, §11.5.5).

use std::path::{Path, PathBuf};

use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::*;
use crate::task::instance::file_steps::{
    Step,
    tests::{StepLog, When},
};

/// A destination with a partial file and a state file left by a task.
fn kept() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("file.bin");
    std::fs::write(tmp_path_for(&dest), b"partial").unwrap();
    std::fs::write(state_path_for(&dest), b"BOLTLOAD header").unwrap();
    (dir, dest)
}

fn discard(dest: &Path, log: &StepLog) -> Result<(), DownloadError> {
    futures::executor::block_on(discard_partial_with(
        dest,
        log.hooks(),
        LockHooks::default(),
    ))
}

/// The lock is free again.
fn assert_unlocked(dest: &Path) {
    DestinationLock::acquire(dest, Unsupported::Fail, &LockHooks::default())
        .expect("the lock is free")
        .release_keeping();
}

const DISCARD: [Step; 6] = [
    Step::Invalidate,
    Step::SyncInvalidation,
    Step::RemovePartial,
    Step::SyncDirAfterPartial,
    Step::RemoveState,
    Step::SyncDirAfterState,
];

/// Both files go, in the order of a discard: the state is invalidated
/// before the partial file goes.
#[test]
fn test_both_files_go_in_order() {
    let (_dir, dest) = kept();
    let log = StepLog::default();
    discard(&dest, &log).unwrap();
    assert!(!tmp_path_for(&dest).exists() && !state_path_for(&dest).exists());
    assert_eq!(log.steps(), DISCARD);
    assert_unlocked(&dest);
}

/// A missing partial file, or both files missing, is no error.
#[test]
fn test_missing_files_are_no_error() {
    for partial in [false, true] {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("file.bin");
        if partial {
            std::fs::write(state_path_for(&dest), b"state").unwrap();
        }
        discard(&dest, &StepLog::default()).unwrap();
        assert!(!tmp_path_for(&dest).exists() && !state_path_for(&dest).exists());
        assert_unlocked(&dest);
    }
}

/// While a task holds the destination, nothing is touched: `InUse`.
#[test]
fn test_a_held_destination_is_in_use() {
    let (_dir, dest) = kept();
    let held = DestinationLock::acquire(&dest, Unsupported::Fail, &LockHooks::default()).unwrap();
    let log = StepLog::default();
    let result = discard(&dest, &log);
    assert!(
        matches!(&result, Err(DownloadError::InUse { path }) if *path == dest),
        "{result:?}"
    );
    assert_eq!(std::fs::read(tmp_path_for(&dest)).unwrap(), b"partial");
    assert_eq!(log.steps(), []);
    held.release_keeping();
}

/// When the invalidation fails, the partial file stays and the error is
/// `Io { op: Lock }` on the state file: with its header intact when the cut
/// failed, empty when its sync did. The state file stays too.
#[test]
fn test_a_failed_invalidation_keeps_the_partial_file() {
    for (failed, intact) in [(Step::Invalidate, true), (Step::SyncInvalidation, false)] {
        let (_dir, dest) = kept();
        let log = StepLog::default();
        log.fail(failed, When::Nth(1));
        let result = discard(&dest, &log);
        let Err(DownloadError::Io { op, path, .. }) = &result else {
            panic!("{failed:?}: {result:?}");
        };
        assert_eq!((*op, path.clone()), (IoOp::Lock, state_path_for(&dest)));
        assert_eq!(std::fs::read(tmp_path_for(&dest)).unwrap(), b"partial");
        let state = std::fs::read(state_path_for(&dest)).unwrap();
        assert_eq!(state.is_empty(), !intact, "{failed:?}");
        assert_eq!(log.steps().last(), Some(&failed));
        assert_unlocked(&dest);
    }
}

/// A partial file that cannot be removed is `Io { op: Remove }`; the state
/// was invalidated before, and goes.
#[test]
fn test_a_partial_file_that_stays_is_an_error() {
    let (_dir, dest) = kept();
    let log = StepLog::default();
    log.fail(Step::RemovePartial, When::Nth(1));
    let result = discard(&dest, &log);
    let Err(DownloadError::Io { op, path, .. }) = &result else {
        panic!("{result:?}");
    };
    assert_eq!((*op, path.clone()), (IoOp::Remove, tmp_path_for(&dest)));
    assert!(tmp_path_for(&dest).exists());
    assert!(!state_path_for(&dest).exists());
    assert_unlocked(&dest);
}

/// It needs the lock: where the file system cannot lock, it fails with
/// `Io { op: Lock }` and touches nothing.
#[test]
fn test_it_needs_the_lock() {
    let (_dir, dest) = kept();
    let unsupported = || {
        #[cfg(unix)]
        let code = libc::ENOLCK;
        #[cfg(windows)]
        let code = 50; // ERROR_NOT_SUPPORTED
        std::io::Error::from_raw_os_error(code)
    };
    let log = StepLog::default();
    let result = futures::executor::block_on(discard_partial_with(
        &dest,
        log.hooks(),
        LockHooks::default().try_lock(move |_| Err(fs4::TryLockError::Error(unsupported()))),
    ));
    assert!(
        matches!(result, Err(DownloadError::Io { op: IoOp::Lock, .. })),
        "{result:?}"
    );
    assert_eq!(log.steps(), []);
    assert_eq!(std::fs::read(tmp_path_for(&dest)).unwrap(), b"partial");
}

/// The public entry point discards as the hooked one does.
#[tokio::test]
async fn test_discard_partial() {
    let (_dir, dest) = kept();
    discard_partial(&dest).await.unwrap();
    assert!(!tmp_path_for(&dest).exists() && !state_path_for(&dest).exists());
}

/// The steps after the partial file are logged, not errors (Q6): a state
/// file that cannot be removed leaves it empty, and a directory that
/// cannot be synced is synced no more; the discard goes on, succeeds, and
/// the lock is released.
#[test]
fn test_later_failures_are_only_logged() {
    for failed in [
        Step::SyncDirAfterPartial,
        Step::RemoveState,
        Step::SyncDirAfterState,
    ] {
        let (_dir, dest) = kept();
        let log = StepLog::default();
        log.fail(failed, When::Nth(1));
        discard(&dest, &log).unwrap_or_else(|e| panic!("{failed:?}: {e}"));
        assert!(!tmp_path_for(&dest).exists(), "{failed:?}");
        let state = state_path_for(&dest);
        if failed == Step::RemoveState {
            assert_eq!(
                std::fs::read(&state).unwrap(),
                b"",
                "an empty state is left"
            );
        } else {
            assert!(!state.exists(), "{failed:?}");
        }
        assert_eq!(log.steps(), DISCARD, "every step taken: {failed:?}");
        assert_unlocked(&dest);
    }
}
