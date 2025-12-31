//! a file writer designed for concurrent task
//!
//! Use memmap to write random access file,
//! and use seek write to write the file in large file.

mod mmap;
mod pool;

use std::{ops::Range, path::Path, sync::Arc};

use bytes::Bytes;
use fs_err::OpenOptions;

use self::{
    mmap::{MmapWriter, MmapWriterBuilder},
    pool::{PoolWriter, PoolWriterBuilder},
};

const FILE_WRITER_QUEUE_SIZE: usize = 2048;

#[derive(Debug, thiserror::Error)]
pub enum FileWriterError {
    #[error(transparent)]
    Io(std::io::Error),
    #[error("failed to send chunk: {0:?}; channel closed")]
    Write(Chunk),
    #[error("failed to send finalize command; channel closed")]
    Finalize,
    #[error(transparent)]
    Recv(oneshot::RecvError),
}

#[enum_dispatch::enum_dispatch(FileRangeWriterImpl)]
pub(crate) trait FileRangeWriter {
    /// Write data to file
    ///
    /// # Errors
    ///
    /// This function will return an error if the file is not writable or the disk is full.
    async fn write_range(&self, range: Range<u64>, data: Bytes) -> Result<(), FileWriterError>;
    /// Sync all data to disk
    ///
    /// # Errors
    ///
    /// This function will return an error if the file is not writable or the disk is full.
    async fn finalize(self) -> Result<(), FileWriterError>;
}

trait FileWriterCapability {
    fn is_supported(_file_size: u64) -> bool {
        true
    }
}

#[enum_dispatch::enum_dispatch]
pub enum FileRangeWriterImpl {
    Mmap(MmapWriter),
    Pool(PoolWriter),
}

impl FileRangeWriter for Arc<FileRangeWriterImpl> {
    async fn write_range(&self, range: Range<u64>, data: Bytes) -> Result<(), FileWriterError> {
        match &**self {
            FileRangeWriterImpl::Mmap(writer) => writer.write_range(range, data).await,
            FileRangeWriterImpl::Pool(writer) => writer.write_range(range, data).await,
        }
    }

    async fn finalize(mut self) -> Result<(), FileWriterError> {
        // A spinlock to wait all write tasks to finish, and then finalize the file.
        let inner = loop {
            match Arc::try_unwrap(self) {
                Ok(inner) => break inner,
                Err(arc) => {
                    self = arc;
                    futures_lite::future::yield_now().await;
                }
            }
        };
        inner.finalize().await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileRangeWriterKind {
    Mmap,
    Pool,
}

impl FileRangeWriterKind {
    // TODO: disable mmap on windows for default
    pub fn suggest_kind(file_size: u64) -> Self {
        if file_size <= isize::MAX as u64 {
            Self::Mmap
        } else {
            Self::Pool
        }
    }
}

#[derive(Debug, Clone)]
pub struct Chunk {
    pub range: Range<u64>,
    pub data: Bytes,
}

#[derive(Clone)]
pub struct FileWriter {
    pub kind: FileRangeWriterKind,
    inner: Arc<FileRangeWriterImpl>,
}

impl FileWriter {
    pub async fn new(path: &Path, size: u64) -> Result<Self, std::io::Error> {
        let kind = FileRangeWriterKind::suggest_kind(size);
        let path = path.to_path_buf();
        let file =
            blocking::unblock(move || OpenOptions::new().create_new(true).write(true).open(path))
                .await?;
        let inner = match kind {
            FileRangeWriterKind::Mmap => {
                FileRangeWriterImpl::Mmap(MmapWriterBuilder::new().file(file).build().await?)
            }
            FileRangeWriterKind::Pool => {
                FileRangeWriterImpl::Pool(PoolWriterBuilder::new().file(file).build()?)
            }
        };
        Ok(Self {
            kind,
            inner: Arc::new(inner),
        })
    }
}

impl FileRangeWriter for FileWriter {
    async fn write_range(&self, range: Range<u64>, data: Bytes) -> Result<(), FileWriterError> {
        self.inner.write_range(range, data).await
    }

    async fn finalize(self) -> Result<(), FileWriterError> {
        self.inner.finalize().await
    }
}
