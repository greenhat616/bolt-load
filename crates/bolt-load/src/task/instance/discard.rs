//! Discarding what a resumable task kept for a destination (design
//! §11.5.2, §11.5.5): `<dest>.downloading` and `<dest>.boltload`.
//!
//! Crate internal until resuming is public (C20, a lead decision).

use std::{path::Path, sync::Arc};

use super::{
    destination_lock::{DestinationLock, LockHooks, Unsupported, state_path_for},
    file_steps::FileHooks,
    output_file::tmp_path_for,
    ownership::{Invalidation, Ownership, Phase},
};
use crate::task::{DownloadError, IoOp};

/// Remove what a task kept for `dest`, in the order of a discard (design
/// §11.5.5): take the destination's lock, invalidate the state file, remove
/// the partial file and sync the directory, then remove the state file,
/// sync the directory and release the lock.
///
/// # Errors
///
/// - [`DownloadError::InUse`] while a task holds the destination.
/// - [`DownloadError::Io`] with [`IoOp::Lock`] when the lock cannot be
///   taken, or the state file cannot be invalidated: the partial file is
///   kept then, as a state that may name it stays.
/// - [`DownloadError::Io`] with [`IoOp::Remove`] when the partial file
///   cannot be removed; the state is invalidated by then.
///
/// A missing partial file, or none of the files, is no error. The state
/// file that cannot be removed is only logged: it is empty, and the next
/// task takes it as no state (Q6).
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "public with resuming, C20 (lead decision)")
)]
pub(crate) async fn discard_partial(dest: impl AsRef<Path>) -> Result<(), DownloadError> {
    discard_partial_with(dest.as_ref(), FileHooks::default(), LockHooks::default()).await
}

/// [`discard_partial`], its file steps and its lock taken through hooks.
pub(crate) async fn discard_partial_with(
    dest: &Path,
    hooks: FileHooks,
    lock_hooks: LockHooks,
) -> Result<(), DownloadError> {
    let dest = dest.to_path_buf();
    blocking::unblock(move || {
        let lock = DestinationLock::acquire(&dest, Unsupported::Fail, &lock_hooks)?;
        let ownership = Ownership::with_hooks(hooks);
        let tmp = tmp_path_for(&dest);
        ownership.own(lock, tmp.clone());
        // A task that wrote a state file: its release syncs the directory.
        ownership.enter(Phase::Fresh);
        let discarded = ownership.discard_resumable();
        ownership.release();
        let failed = |op, path: &Path, source| DownloadError::Io {
            op,
            path: path.to_path_buf(),
            source: Arc::new(source),
        };
        match (discarded.invalidation, discarded.removal) {
            (Invalidation::Done, None) => Ok(()),
            (Invalidation::Done, Some(source)) => Err(failed(IoOp::Remove, &tmp, source)),
            (Invalidation::Intact(source) | Invalidation::Unsynced(source), _) => {
                Err(failed(IoOp::Lock, &state_path_for(&dest), source))
            }
        }
    })
    .await
}

#[cfg(test)]
mod tests;
