use std::{io, ops::Range, sync::Arc, time::Duration};

use bolt_load_core::adapter::AdapterError;

/// What the user can do about a file error, appended to its message: free
/// up space when the disk is full or the quota is exceeded.
pub(crate) fn storage_hint(error: &io::Error) -> &'static str {
    match error.kind() {
        io::ErrorKind::StorageFull => "; free up disk space, then download again",
        io::ErrorKind::QuotaExceeded => {
            "; free up space or raise the disk quota, then download again"
        }
        _ => "",
    }
}

/// The kind of the task failed
#[derive(Debug, Clone, snafu::Snafu)]
#[snafu(visibility(pub(in crate::runner)))]
pub enum TaskError {
    /// The task is cancelled
    #[snafu(display("task is cancelled"))]
    Cancelled,
    /// The task is timeout, only happen when a stream is not sent in a period
    #[snafu(display("task is timeout"))]
    Timeout,
    /// The channel is closed
    #[snafu(display("channel is closed"))]
    ChannelClosed,
    /// The task is exceeded the total size
    ///
    /// Possible reason:
    /// - The total sized while the downloaded chunk is larger than the total size
    #[snafu(display("task is exceeded the total size"))]
    ExceededTotalSize,
    /// The task is smaller than the total size
    ///
    /// Possible reason:
    /// - The total sized while the downloaded chunk is smaller than the total size
    #[snafu(display("task is smaller than the total size"))]
    SmallerThanTotalSize,
    #[snafu(display("stream error: {source}"))]
    StreamError { source: AdapterError },
    /// No new bytes were accepted within the configured period
    #[snafu(display("no progress within {timeout:?}"))]
    NoProgress { timeout: Duration },
    /// Too many consecutive attempts failed without progress in between
    #[snafu(display("retry limit of {limit} consecutive failures exceeded, last error: {source}"))]
    RetryLimitExceeded {
        limit: usize,
        source: Arc<TaskError>,
    },
    /// Writing `range` of the file failed. No write failure is retried: the
    /// task ends with the first one. A full disk or an exceeded quota adds a
    /// hint to the message.
    ///
    /// Only the concurrent mode reports it. The singleton mode buffers its
    /// writes, so a failure may surface on a later write or on the flush
    /// and has no range: it is
    /// [`DownloadError::Io`](crate::task::DownloadError::Io).
    #[snafu(display(
        "failed to write bytes {}..{}: {source}{}",
        range.start,
        range.end,
        storage_hint(source)
    ))]
    Write {
        range: Range<u64>,
        source: Arc<io::Error>,
    },
    /// The other error
    #[snafu(display("other error: {message}"))]
    Other { message: String },
}

impl TaskError {
    #[inline]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::StreamError {
                source: AdapterError::Retryable { .. }
            } | Self::Timeout
        )
    }

    #[inline]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    #[inline]
    pub const fn is_stream_error(&self) -> bool {
        matches!(self, Self::StreamError { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_error(kind: io::ErrorKind) -> TaskError {
        TaskError::Write {
            range: 10..20,
            source: Arc::new(io::Error::from(kind)),
        }
    }

    #[test]
    fn a_write_error_names_its_range_and_hints_at_storage() {
        for (kind, hint) in [
            (io::ErrorKind::StorageFull, "free up disk space"),
            (io::ErrorKind::QuotaExceeded, "raise the disk quota"),
        ] {
            let message = write_error(kind).to_string();
            assert!(
                message.starts_with("failed to write bytes 10..20: "),
                "{message}"
            );
            assert!(message.contains(hint), "{message}");
        }
        let message = write_error(io::ErrorKind::PermissionDenied).to_string();
        assert!(!message.contains("; "), "{message}");
    }

    #[test]
    fn a_write_error_is_not_retried() {
        for kind in [
            io::ErrorKind::StorageFull,
            io::ErrorKind::QuotaExceeded,
            io::ErrorKind::Other,
        ] {
            assert!(!write_error(kind).is_retryable(), "{kind:?}");
        }
    }
}
