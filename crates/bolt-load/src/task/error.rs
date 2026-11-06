use std::{fmt, io, path::PathBuf, sync::Arc};

use crate::{
    adapter::AdapterError,
    runner::{TaskError, storage_hint},
};

/// The file operation of a [`DownloadError::Io`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IoOp {
    /// Reading the metadata of the destination or its directory.
    Inspect,
    /// Creating or opening the temporary file. The concurrent mode also
    /// reports here a failure to give its writer workers their own handles
    /// or mapping of the file.
    Create,
    /// Sizing the temporary file.
    Allocate,
    /// Writing the content in the singleton mode. The concurrent mode
    /// reports [`TaskError::Write`] instead, with the range.
    Write,
    /// Flushing the content. The singleton mode buffers its writes, so a
    /// write may also fail here. The concurrent mode joins its writer
    /// workers and syncs the file to disk here.
    Flush,
    /// Moving the temporary file to the destination.
    Rename,
}

impl fmt::Display for IoOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Inspect => "inspect",
            Self::Create => "create",
            Self::Allocate => "allocate",
            Self::Write => "write",
            Self::Flush => "flush",
            Self::Rename => "rename to",
        })
    }
}

/// Why a task downloads over one connection although the server seemed to
/// take ranges, see [`TaskInfo::fallback`](super::TaskInfo::fallback).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum FallbackReason {
    /// The server answered a range request with the whole resource.
    RangeIgnored,
    /// The server content-codes its responses despite
    /// `Accept-Encoding: identity`, so ranges count encoded bytes. Also
    /// reported when range bodies do not match their `Content-Range`, which
    /// is what a client that decodes them sees.
    ContentCoded,
}

impl fmt::Display for FallbackReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::RangeIgnored => "the server ignores ranges",
            Self::ContentCoded => "the server content-codes the response",
        })
    }
}

/// Why a task did not finish.
#[derive(Debug, Clone, snafu::Snafu)]
#[non_exhaustive]
pub enum DownloadError {
    /// The task was cancelled before the output file was committed.
    #[snafu(display("the download was cancelled"))]
    Cancelled,
    /// The metadata could not be retrieved.
    #[snafu(display("failed to retrieve the metadata: {source}"))]
    Metadata { source: AdapterError },
    /// The transfer failed: a stream error, a timeout, no progress, too
    /// many consecutive failures, a range that failed too often
    /// ([`TaskError::RangeExhausted`]), or a failed write in the concurrent
    /// mode ([`TaskError::Write`]).
    #[snafu(display("the transfer failed: {source}"))]
    Transfer { source: Arc<TaskError> },
    /// Ranges cannot carry the file, for `reason`. The concurrent mode ends
    /// with it, and the task then falls back to one connection, so a task
    /// does not end with it.
    #[snafu(display("ranges are unusable, {reason}: {source}"))]
    RangeUnsupported {
        reason: FallbackReason,
        source: AdapterError,
    },
    /// A file operation failed. A full disk or an exceeded quota adds a
    /// hint to the message.
    #[snafu(display("failed to {op} {}: {source}{}", path.display(), storage_hint(source)))]
    Io {
        op: IoOp,
        path: PathBuf,
        source: Arc<io::Error>,
    },
    /// The destination cannot be written, for example its directory is
    /// missing or it is a directory.
    #[snafu(display("invalid destination {}: {reason}", path.display()))]
    InvalidDestination { path: PathBuf, reason: &'static str },
    /// [`save_into`](super::TaskBuilder::save_into) got a filename from the
    /// adapter that is not a single safe path component.
    #[snafu(display("the suggested filename {name:?} is not a safe file name"))]
    InvalidFilename { name: String },
    /// [`save_into`](super::TaskBuilder::save_into) needs a filename, but the
    /// adapter suggested none.
    #[snafu(display("the adapter suggested no filename to save into the directory"))]
    MissingFilename,
    /// The runtime could not run the task.
    #[snafu(display("runtime error: {message}"))]
    Runtime { message: String },
    /// The task panicked; its workers were signalled but not joined.
    #[snafu(display("the download panicked: {message}"))]
    Panicked { message: String },
    /// The task ended without publishing an outcome.
    #[snafu(display("the download ended without an outcome"))]
    Aborted,
}

impl DownloadError {
    /// Whether the task was cancelled.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    /// A transfer failure; a cancellation becomes [`Self::Cancelled`].
    pub(crate) fn transfer(error: TaskError) -> Self {
        match error {
            TaskError::Cancelled => Self::Cancelled,
            source => Self::Transfer {
                source: Arc::new(source),
            },
        }
    }

    pub(crate) fn other(message: impl Into<String>) -> Self {
        Self::transfer(TaskError::Other {
            message: message.into(),
        })
    }

    pub(crate) fn io(op: IoOp, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            op,
            path: path.into(),
            source: Arc::new(source),
        }
    }

    /// The transfer error, if this is [`Self::Transfer`].
    pub fn as_transfer(&self) -> Option<&TaskError> {
        match self {
            Self::Transfer { source } => Some(source),
            _ => None,
        }
    }
}

/// Why [`TaskBuilder::spawn`](super::TaskBuilder::spawn) rejected the
/// configuration.
#[derive(Debug, Clone, PartialEq, Eq, snafu::Snafu)]
#[snafu(module)]
#[non_exhaustive]
pub enum BuildError {
    #[snafu(display("no runtime is set, see `TaskBuilder::runtime`"))]
    MissingRuntime,
    #[snafu(display("no destination is set, see `TaskBuilder::save_to` and `save_into`"))]
    MissingDestination,
    #[snafu(display("invalid options: {reason}"))]
    InvalidOptions { reason: &'static str },
    #[snafu(display("invalid destination {}: {reason}", path.display()))]
    InvalidDestination { path: PathBuf, reason: &'static str },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_disk_adds_a_hint_to_a_file_error() {
        let error = DownloadError::io(IoOp::Flush, "file.tmp", io::ErrorKind::StorageFull.into());
        let message = error.to_string();
        assert!(
            message.starts_with("failed to flush file.tmp: "),
            "{message}"
        );
        assert!(
            message.ends_with("; free up disk space, then download again"),
            "{message}"
        );
    }
}
