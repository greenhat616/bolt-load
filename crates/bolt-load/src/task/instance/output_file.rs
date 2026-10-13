//! Ownership rules of the `.downloading` temporary file.
//!
//! A task writes into [`tmp_path_for`] and only [`publish`]es it to the
//! destination after every owned worker has exited. On failure or
//! cancellation the temporary file is [`discard`]ed and an existing
//! destination file is never touched.

use std::{
    io,
    path::{Path, PathBuf},
};

use bolt_load_utils::telemetry::*;
use smol_cancellation_token::CancellationToken;

use super::{Result, TaskInstanceError};
use crate::{DOWNLOADING_TMP_EXTENSION, runner::TaskError};

/// The temporary path for `dest`; always differs from `dest`.
pub(crate) fn tmp_path_for(dest: &Path) -> PathBuf {
    let mut path = dest.as_os_str().to_os_string();
    path.push(DOWNLOADING_TMP_EXTENSION);
    PathBuf::from(path)
}

/// Atomically move the finished temporary file to its destination.
pub(crate) async fn publish(tmp: &Path, dest: &Path) -> io::Result<()> {
    async_fs::rename(tmp, dest).await
}

/// Remove the temporary file. Failures are logged and never override the
/// error that caused the discard.
pub(crate) async fn discard(tmp: &Path) {
    match async_fs::remove_file(tmp).await {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => warn!("failed to remove temporary file {tmp:?}: {e}"),
    }
}

/// Apply the commit rule to a finished transfer whose workers have all exited.
///
/// A cancellation observed before the rename starts discards the result;
/// once the rename starts, its own outcome decides.
pub(crate) async fn commit(
    transfer: Result<()>,
    tmp: &Path,
    dest: &Path,
    cancel_token: &CancellationToken,
) -> Result<()> {
    let transfer = match transfer {
        Ok(()) if cancel_token.is_cancelled() => {
            Err(TaskInstanceError::new_failed(TaskError::Cancelled))
        }
        other => other,
    };
    if let Err(e) = transfer {
        discard(tmp).await;
        return Err(e);
    }
    if let Err(e) = publish(tmp, dest).await {
        discard(tmp).await;
        return Err(TaskInstanceError::new_failed(TaskError::Other {
            message: format!("failed to move {tmp:?} to {dest:?}: {e}"),
        }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tmp_path_appends_extension() {
        let dest = Path::new("/tmp/dir/file.bin");
        assert_eq!(
            tmp_path_for(dest),
            PathBuf::from("/tmp/dir/file.bin.downloading")
        );
    }

    #[test]
    fn test_tmp_path_differs_even_with_extension_suffix() {
        let dest = Path::new("/tmp/dir/file.downloading");
        assert_ne!(tmp_path_for(dest), dest);
    }

    #[tokio::test]
    async fn test_publish_and_discard() {
        let dir = tempfile::TempDir::new().unwrap();
        let dest = dir.path().join("out.bin");
        let tmp = tmp_path_for(&dest);

        std::fs::write(&tmp, b"data").unwrap();
        publish(&tmp, &dest).await.unwrap();
        assert!(!tmp.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"data");

        std::fs::write(&tmp, b"partial").unwrap();
        discard(&tmp).await;
        assert!(!tmp.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"data");

        // Discarding a missing file is a no-op.
        discard(&tmp).await;
    }

    #[tokio::test]
    async fn test_commit_rules() {
        let dir = tempfile::TempDir::new().unwrap();
        let dest = dir.path().join("out.bin");
        let tmp = tmp_path_for(&dest);
        std::fs::write(&dest, b"old").unwrap();

        // A failed transfer discards the temporary file and keeps the destination.
        std::fs::write(&tmp, b"partial").unwrap();
        let failed = Err(TaskInstanceError::new_failed(TaskError::Timeout));
        let token = CancellationToken::new();
        assert!(commit(failed, &tmp, &dest, &token).await.is_err());
        assert!(!tmp.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"old");

        // Cancellation before the rename wins over a finished transfer.
        std::fs::write(&tmp, b"new").unwrap();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        match commit(Ok(()), &tmp, &dest, &cancelled).await {
            Err(TaskInstanceError::Failed { source }) => assert!(source.is_cancelled()),
            other => panic!("expected cancellation, got {other:?}"),
        }
        assert!(!tmp.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"old");

        std::fs::write(&tmp, b"new").unwrap();
        commit(Ok(()), &tmp, &dest, &token).await.unwrap();
        assert!(!tmp.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
    }
}
