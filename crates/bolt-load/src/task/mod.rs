#![doc = include_str!("../../docs/task.md")]
use std::ops::Range;

mod builder;
mod destination;
mod error;
mod handle;
/// Task instance implementations (singleton and concurrent).
pub mod instance;
mod meta;
mod options;
mod status;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

pub use builder::*;
pub use error::*;
pub use handle::*;
pub use options::*;
pub use status::{Speed, StatusReceiver, TaskInfo, TaskState, TaskStatus};

pub type RunnerId = usize;

#[derive(Debug, Clone)]
pub enum ControlEvent {
    /// limit the total size of the task
    ///
    /// The limit number should be smaller than the total size of the task
    LimitTotal(u64),
}

pub type DownloadedChunks = Vec<Range<u64>>;

/// the progress of the download task
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct Progress {
    /// the total size of the content
    /// possible None if the total size is unknown
    /// It requires the single-thread task can finished until the stream is None
    pub total: Option<u64>,

    /// the current downloaded size
    pub downloaded: u64,

    /// the downloaded chunks,
    /// for the single-thread task, it is the downloaded chunk
    /// for the concurrent task, it is the downloaded chunks
    pub downloaded_chunks: DownloadedChunks,
}

impl Progress {
    pub fn new(total: Option<u64>, downloaded: u64, downloaded_chunks: DownloadedChunks) -> Self {
        Self {
            total,
            downloaded,
            downloaded_chunks,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum DownloadMode {
    /// the download mode of single-thread download,
    /// Just use one runner task to fetch the content.
    Singleton,
    /// the download mode of concurrent download
    /// Due to we use multiple async tasks to download the content,
    /// and the async tasks is M:N thread model,
    /// so the download mode is concurrent, not called multi-thread.
    /// Because possible some runtime support single-thread async mode.
    #[default]
    Concurrent,
}
