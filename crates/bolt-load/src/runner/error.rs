use bolt_load_core::adapter::AdapterError;

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
    /// The task is empty
    #[snafu(display("task is empty"))]
    Empty,
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

    #[inline]
    pub const fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }
}
