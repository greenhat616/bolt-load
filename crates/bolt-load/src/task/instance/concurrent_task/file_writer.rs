//! a file writer designed for concurrent task
//!
//! Use memmap to write random access file,
//! and use seek write to write the file in large file.

#[cfg(feature = "compio")]
mod compio;
#[cfg(feature = "mmap")]
mod mmap;
mod metrics;
mod null;
mod pool;

use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use bytes::Bytes;
use fs_err::{File, OpenOptions};
use snafu::prelude::*;

#[cfg(feature = "compio")]
pub use self::compio::CompioWriterBuilder;
#[cfg(feature = "mmap")]
pub use self::mmap::MmapWriterBuilder;
use self::{null::NullWriter, pool::PoolWriter};
pub use self::metrics::{FileWriterMetrics, MetricsSnapshot};
// Re-export builders for benchmarking and advanced usage
pub use self::{null::NullWriterBuilder, pool::PoolWriterBuilder};
use crate::runtime::yield_now;

pub const FILE_WRITER_QUEUE_SIZE: usize = 2048;

#[derive(Debug, Snafu)]
pub enum CommandError {
    #[snafu(transparent)]
    Io { source: std::io::Error },
    #[snafu(display("failed to receive command: channel closed"))]
    Recv,
    #[snafu(display("failed to send command: channel closed"))]
    Send,
}

#[derive(Debug, Snafu)]
pub enum FileWriterBuilderError {
    #[snafu(display("validation failed: {message}"))]
    Validation { message: String },
    #[snafu(transparent)]
    RangeWriter { source: FileWriterError },
}

#[derive(Debug, Snafu)]
pub enum FileWriterError {
    #[snafu(display("failed to open or create file {path:?}"))]
    OpenOrCreateFile { source: CommandError, path: PathBuf },
    #[snafu(display("failed to allocate file size {size}"))]
    AllocateFile { source: CommandError, size: u64 },
    #[snafu(display("failed to write range {chunk:?}, path: {path:?}"))]
    WriteRange {
        source: CommandError,
        chunk: Option<Chunk>,
        path: PathBuf,
    },
    #[snafu(display("failed to finalize, sync all data to disk etc, in path: {path:?}"))]
    Finalize { source: CommandError, path: PathBuf },
}

#[enum_dispatch::enum_dispatch(FileRangeWriterImpl)]
#[allow(async_fn_in_trait)] // Only for benchmarking and advanced usage
pub trait FileRangeWriter {
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
    #[cfg(feature = "mmap")]
    Mmap(self::mmap::MmapWriter),
    Pool(PoolWriter),
    Null(NullWriter),
    #[cfg(feature = "compio")]
    Compio(self::compio::CompioWriter),
}

impl FileRangeWriter for Arc<FileRangeWriterImpl> {
    async fn write_range(&self, range: Range<u64>, data: Bytes) -> Result<(), FileWriterError> {
        match &**self {
            #[cfg(feature = "mmap")]
            FileRangeWriterImpl::Mmap(writer) => writer.write_range(range, data).await,
            FileRangeWriterImpl::Pool(writer) => writer.write_range(range, data).await,
            FileRangeWriterImpl::Null(writer) => writer.write_range(range, data).await,
            #[cfg(feature = "compio")]
            FileRangeWriterImpl::Compio(writer) => writer.write_range(range, data).await,
        }
    }

    async fn finalize(mut self) -> Result<(), FileWriterError> {
        // A spinlock to wait all write tasks to finish, and then finalize the file.
        let inner = loop {
            match Arc::try_unwrap(self) {
                Ok(inner) => break inner,
                Err(arc) => {
                    self = arc;
                    yield_now().await;
                }
            }
        };
        inner.finalize().await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileRangeWriterKind {
    #[cfg(feature = "mmap")]
    Mmap,
    Pool,
    /// Null writer for benchmarking - discards all data
    Null,
    #[cfg(feature = "compio")]
    Compio,
}

impl FileRangeWriterKind {
    #[cfg(feature = "compio")]
    pub fn suggest_kind(_file_size: u64) -> Self {
        Self::Compio
    }

    #[cfg(all(feature = "mmap", not(feature = "compio")))]
    pub fn suggest_kind(file_size: u64) -> Self {
        if file_size <= isize::MAX as u64 {
            Self::Mmap
        } else {
            Self::Pool
        }
    }

    #[cfg(not(any(feature = "mmap", feature = "compio")))]
    pub fn suggest_kind(_file_size: u64) -> Self {
        Self::Pool
    }
}

#[derive(derive_more::Debug, Clone)]
pub struct Chunk {
    pub range: Range<u64>,
    #[debug("{} bytes", data.len())]
    pub data: Bytes,
}

async fn open_file(path: PathBuf, size: u64) -> Result<File, FileWriterError> {
    let file = blocking::unblock(move || {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(CommandError::from)
            .with_context(|_| OpenOrCreateFileSnafu { path: path.clone() })?;
        // Pre-allocate the file size
        file.set_len(size)
            .map_err(CommandError::from)
            .with_context(|_| AllocateFileSnafu { size })?;
        Ok::<_, FileWriterError>(file)
    })
    .await?;
    Ok(file)
}

#[derive(Clone)]
pub struct FileWriter {
    pub kind: FileRangeWriterKind,
    inner: Arc<FileRangeWriterImpl>,
    metrics: FileWriterMetrics,
}

impl FileWriter {
    pub async fn new(path: &Path, size: u64) -> Result<Self, FileWriterBuilderError> {
        let kind = FileRangeWriterKind::suggest_kind(size);
        Self::new_with_kind(path, size, kind).await
    }

    pub async fn new_with_kind(
        path: &Path,
        size: u64,
        kind: FileRangeWriterKind,
    ) -> Result<Self, FileWriterBuilderError> {
        let path = path.to_path_buf();
        let metrics = FileWriterMetrics::new();

        let inner = match kind {
            #[cfg(feature = "mmap")]
            FileRangeWriterKind::Mmap => {
                let file = open_file(path.clone(), size).await?;
                FileRangeWriterImpl::Mmap(
                    MmapWriterBuilder::new()
                        .file(file)
                        .metrics(metrics.clone())
                        .build()
                        .await?,
                )
            }
            FileRangeWriterKind::Pool => {
                let file = open_file(path.clone(), size).await?;
                FileRangeWriterImpl::Pool(
                    PoolWriterBuilder::new()
                        .file(file)
                        .metrics(metrics.clone())
                        .build()?,
                )
            }
            FileRangeWriterKind::Null => {
                // For null writer, we don't need a file - just create the writer
                FileRangeWriterImpl::Null(NullWriterBuilder::new().build().await?)
            }
            #[cfg(feature = "compio")]
            FileRangeWriterKind::Compio => {
                use self::compio::CompioWriterBuilder;
                FileRangeWriterImpl::Compio(
                    CompioWriterBuilder::new()
                        .path(path.clone())
                        .metrics(metrics.clone())
                        .build()
                        .await?,
                )
            }
        };
        Ok(Self {
            kind,
            inner: Arc::new(inner),
            metrics,
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

impl FileWriter {
    /// Get the current metrics for this file writer
    pub fn get_metrics(&self) -> &FileWriterMetrics {
        &self.metrics
    }

    /// Get a snapshot of current metrics
    pub fn get_metrics_snapshot(&self) -> MetricsSnapshot {
        self.metrics.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};

    use tempfile::TempDir;

    use super::*;

    #[test]
    #[cfg(feature = "compio")]
    fn test_file_range_writer_kind_suggest_kind_small_file() {
        // With compio feature, always use Compio
        assert_eq!(
            FileRangeWriterKind::suggest_kind(1024),
            FileRangeWriterKind::Compio
        );
        assert_eq!(
            FileRangeWriterKind::suggest_kind(1024 * 1024 * 1024),
            FileRangeWriterKind::Compio
        ); // 1GB
    }

    #[test]
    #[cfg(all(feature = "mmap", not(feature = "compio")))]
    fn test_file_range_writer_kind_suggest_kind_small_file() {
        // Small files should use Mmap
        assert_eq!(
            FileRangeWriterKind::suggest_kind(1024),
            FileRangeWriterKind::Mmap
        );
        assert_eq!(
            FileRangeWriterKind::suggest_kind(1024 * 1024 * 1024),
            FileRangeWriterKind::Mmap
        ); // 1GB
    }

    #[test]
    #[cfg(feature = "compio")]
    fn test_file_range_writer_kind_suggest_kind_large_file() {
        // With compio feature, always use Compio
        assert_eq!(
            FileRangeWriterKind::suggest_kind(isize::MAX as u64 + 1),
            FileRangeWriterKind::Compio
        );
    }

    #[test]
    #[cfg(all(feature = "mmap", not(feature = "compio")))]
    fn test_file_range_writer_kind_suggest_kind_large_file() {
        // Files larger than isize::MAX should use Pool
        assert_eq!(
            FileRangeWriterKind::suggest_kind(isize::MAX as u64 + 1),
            FileRangeWriterKind::Pool
        );
    }

    #[test]
    #[cfg(feature = "compio")]
    fn test_file_range_writer_kind_suggest_kind_boundary() {
        // With compio feature, always use Compio
        assert_eq!(
            FileRangeWriterKind::suggest_kind(isize::MAX as u64),
            FileRangeWriterKind::Compio
        );
    }

    #[test]
    #[cfg(all(feature = "mmap", not(feature = "compio")))]
    fn test_file_range_writer_kind_suggest_kind_boundary() {
        // Boundary test: exactly equal to isize::MAX should use Mmap
        assert_eq!(
            FileRangeWriterKind::suggest_kind(isize::MAX as u64),
            FileRangeWriterKind::Mmap
        );
    }

    #[test]
    fn test_chunk_clone() {
        let chunk = Chunk {
            range: 0..100,
            data: Bytes::from_static(b"test data"),
        };
        let cloned = chunk.clone();
        assert_eq!(cloned.range, chunk.range);
        assert_eq!(cloned.data, chunk.data);
    }

    #[test]
    fn test_chunk_debug() {
        let chunk = Chunk {
            range: 0..100,
            data: Bytes::from_static(b"test"),
        };
        let debug_str = format!("{:?}", chunk);
        assert!(debug_str.contains("Chunk"));
        assert!(debug_str.contains("range"));
        assert!(debug_str.contains("data"));
    }

    #[tokio::test]
    async fn test_file_writer_new_creates_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test_file.bin");
        let file_size = 1024u64;

        let writer = FileWriter::new(&file_path, file_size).await.unwrap();

        // Validate file is created
        assert!(file_path.exists());

        // Check the writer kind based on enabled features
        #[cfg(feature = "compio")]
        assert_eq!(writer.kind, FileRangeWriterKind::Compio);
        #[cfg(all(feature = "mmap", not(feature = "compio")))]
        assert_eq!(writer.kind, FileRangeWriterKind::Mmap);

        writer.finalize().await.unwrap();
    }

    #[tokio::test]
    async fn test_file_writer_write_and_read() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test_write.bin");
        let file_size = 1024u64;

        let writer = FileWriter::new(&file_path, file_size).await.unwrap();

        // Write data
        let data = Bytes::from_static(b"Hello, FileWriter!");
        writer
            .write_range(0..data.len() as u64, data.clone())
            .await
            .unwrap();

        // Write to different position
        let data2 = Bytes::from_static(b"Test!");
        writer
            .write_range(500..500 + data2.len() as u64, data2.clone())
            .await
            .unwrap();

        writer.finalize().await.unwrap();

        // Validate data
        let mut file = std::fs::File::open(&file_path).unwrap();
        let mut buffer = vec![0u8; data.len()];
        file.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, &data[..]);

        file.seek(SeekFrom::Start(500)).unwrap();
        let mut buffer2 = vec![0u8; data2.len()];
        file.read_exact(&mut buffer2).unwrap();
        assert_eq!(&buffer2, &data2[..]);
    }

    #[tokio::test]
    async fn test_file_writer_concurrent_writes() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test_concurrent.bin");
        let file_size = 4096u64;

        let writer = FileWriter::new(&file_path, file_size).await.unwrap();

        // Concurrent write multiple blocks
        let write_futures: Vec<_> = (0..10)
            .map(|i| {
                let offset = i * 100;
                let data = Bytes::from(format!("block_{:03}", i));
                writer.write_range(offset..offset + data.len() as u64, data)
            })
            .collect();

        for future in write_futures {
            future.await.unwrap();
        }

        writer.finalize().await.unwrap();

        // Validate all data
        let mut file = std::fs::File::open(&file_path).unwrap();
        for i in 0..10u64 {
            let offset = i * 100;
            let expected = format!("block_{:03}", i);
            file.seek(SeekFrom::Start(offset)).unwrap();
            let mut buffer = vec![0u8; expected.len()];
            file.read_exact(&mut buffer).unwrap();
            assert_eq!(String::from_utf8(buffer).unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn test_file_writer_clone() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test_clone.bin");
        let file_size = 1024u64;

        let writer = FileWriter::new(&file_path, file_size).await.unwrap();
        let writer_clone = writer.clone();

        // Write with original writer
        let data1 = Bytes::from_static(b"From original");
        writer
            .write_range(0..data1.len() as u64, data1.clone())
            .await
            .unwrap();

        // Write with cloned writer
        let data2 = Bytes::from_static(b"From clone");
        writer_clone
            .write_range(100..100 + data2.len() as u64, data2.clone())
            .await
            .unwrap();

        // Only one can finalize, because they share the same Arc
        drop(writer_clone);
        writer.finalize().await.unwrap();

        // Validate both writes are successful
        let mut file = std::fs::File::open(&file_path).unwrap();
        let mut buffer1 = vec![0u8; data1.len()];
        file.read_exact(&mut buffer1).unwrap();
        assert_eq!(&buffer1, &data1[..]);

        file.seek(SeekFrom::Start(100)).unwrap();
        let mut buffer2 = vec![0u8; data2.len()];
        file.read_exact(&mut buffer2).unwrap();
        assert_eq!(&buffer2, &data2[..]);
    }

    #[test]
    fn test_file_writer_kind_equality() {
        assert_eq!(FileRangeWriterKind::Mmap, FileRangeWriterKind::Mmap);
        assert_eq!(FileRangeWriterKind::Pool, FileRangeWriterKind::Pool);
        assert_ne!(FileRangeWriterKind::Mmap, FileRangeWriterKind::Pool);
    }

    #[test]
    fn test_file_writer_kind_copy() {
        let kind = FileRangeWriterKind::Mmap;
        let copied = kind;
        assert_eq!(kind, copied);
    }

    #[test]
    fn test_file_writer_kind_debug() {
        let kind = FileRangeWriterKind::Mmap;
        let debug_str = format!("{:?}", kind);
        assert!(debug_str.contains("Mmap"));
    }

    #[test]
    fn test_file_writer_error_display() {
        use std::path::PathBuf;

        // Test WriteRange error display
        let chunk = Chunk {
            range: 0..10,
            data: Bytes::from_static(b"test"),
        };
        let write_err = FileWriterError::WriteRange {
            source: CommandError::Send,
            chunk: Some(chunk),
            path: PathBuf::from("/test/path"),
        };
        let write_display = format!("{}", write_err);
        assert!(write_display.contains("failed to write range"));
        assert!(write_display.contains("/test/path"));

        // Test Finalize error display
        let finalize_err = FileWriterError::Finalize {
            source: CommandError::Recv,
            path: PathBuf::from("/test/finalize"),
        };
        let finalize_display = format!("{}", finalize_err);
        assert!(finalize_display.contains("finalize"));
        assert!(finalize_display.contains("/test/finalize"));

        // Test OpenOrCreateFile error display
        let open_err = FileWriterError::OpenOrCreateFile {
            source: CommandError::Io {
                source: std::io::Error::other("test io error"),
            },
            path: PathBuf::from("/test/open"),
        };
        let open_display = format!("{}", open_err);
        assert!(open_display.contains("failed to open or create file"));
    }
}
