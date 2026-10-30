//! Ownership rules of the `.downloading` temporary file.
//!
//! A task writes into [`OutputFile::tmp`] and only [`publish`]es it to the
//! destination after every owned worker has exited. On failure or
//! cancellation the temporary file is [`discard`]ed and an existing
//! destination file is never touched.

use std::{
    io,
    path::{Path, PathBuf},
};

use bolt_load_utils::telemetry::*;
use smol_cancellation_token::CancellationToken;

use super::Result;
use crate::{
    DOWNLOADING_TMP_EXTENSION,
    task::{DownloadError, IoOp},
};

/// The destination of one attempt and the temporary file it writes into.
///
/// The temporary path is derived here and nowhere else, so a change in how
/// it is chosen (for example, to give each attempt its own file) stays in
/// this type.
#[derive(Debug, Clone)]
pub(crate) struct OutputFile {
    pub(crate) dest: PathBuf,
    pub(crate) tmp: PathBuf,
}

impl OutputFile {
    pub(crate) fn new(dest: PathBuf) -> Self {
        Self {
            tmp: tmp_path_for(&dest),
            dest,
        }
    }

    /// Apply the commit rule to a finished transfer whose workers have all
    /// exited, see [`commit`].
    pub(crate) async fn commit(
        &self,
        transfer: Result<()>,
        cancel_token: &CancellationToken,
    ) -> Teardown {
        commit(transfer, &self.tmp, &self.dest, cancel_token).await
    }
}

/// The outcome of a transfer after its teardown.
#[derive(Debug)]
pub(crate) struct Teardown {
    pub(crate) result: Result<()>,
    /// The temporary file that could not be removed.
    pub(crate) leftover_temp_file: Option<PathBuf>,
}

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

/// Remove the temporary file, returning it when it could not be removed.
/// Failures never override the error that caused the discard.
pub(crate) async fn discard(tmp: &Path) -> Option<PathBuf> {
    removal_failed(tmp, async_fs::remove_file(tmp).await)
}

/// [`discard`] without an executor, for the panic handler.
pub(crate) fn discard_blocking(tmp: &Path) -> Option<PathBuf> {
    removal_failed(tmp, std::fs::remove_file(tmp))
}

fn removal_failed(tmp: &Path, removed: io::Result<()>) -> Option<PathBuf> {
    match removed {
        Ok(()) => None,
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            warn!("failed to remove temporary file {tmp:?}: {e}");
            Some(tmp.to_path_buf())
        }
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
) -> Teardown {
    let transfer = match transfer {
        Ok(()) if cancel_token.is_cancelled() => Err(DownloadError::Cancelled),
        other => other,
    };
    let result = match transfer {
        Err(e) => Err(e),
        Ok(()) => match publish(tmp, dest).await {
            Ok(()) => {
                return Teardown {
                    result: Ok(()),
                    leftover_temp_file: None,
                };
            }
            Err(e) => Err(DownloadError::io(IoOp::Rename, dest, e)),
        },
    };
    Teardown {
        result,
        leftover_temp_file: discard(tmp).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::TaskError;

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
        assert_eq!(discard(&tmp).await, None);
        assert!(!tmp.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"data");

        // Discarding a missing file is a no-op.
        assert_eq!(discard(&tmp).await, None);
        assert_eq!(discard_blocking(&tmp), None);
    }

    #[tokio::test]
    async fn test_commit_rules() {
        let dir = tempfile::TempDir::new().unwrap();
        let dest = dir.path().join("out.bin");
        let tmp = tmp_path_for(&dest);
        std::fs::write(&dest, b"old").unwrap();

        // A failed transfer discards the temporary file and keeps the destination.
        std::fs::write(&tmp, b"partial").unwrap();
        let failed = Err(DownloadError::transfer(TaskError::Timeout));
        let token = CancellationToken::new();
        assert!(commit(failed, &tmp, &dest, &token).await.result.is_err());
        assert!(!tmp.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"old");

        // Cancellation before the rename wins over a finished transfer.
        std::fs::write(&tmp, b"new").unwrap();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        match commit(Ok(()), &tmp, &dest, &cancelled).await.result {
            Err(DownloadError::Cancelled) => {}
            other => panic!("expected cancellation, got {other:?}"),
        }
        assert!(!tmp.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"old");

        std::fs::write(&tmp, b"new").unwrap();
        commit(Ok(()), &tmp, &dest, &token).await.result.unwrap();
        assert!(!tmp.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
    }
}
