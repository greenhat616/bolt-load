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
use crate::runtime::yield_now;

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
                    yield_now().await;
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
        let file = blocking::unblock(move || {
            let file = OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(path)?;
            // Pre-allocate the file size
            // TODO: make pre-allocation transparent on upper layer
            file.set_len(size)?;
            Ok::<_, std::io::Error>(file)
        })
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

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn test_file_range_writer_kind_suggest_kind_small_file() {
        // 小文件应该使用 Mmap
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
    fn test_file_range_writer_kind_suggest_kind_large_file() {
        // 大于 isize::MAX 的文件应该使用 Pool
        assert_eq!(
            FileRangeWriterKind::suggest_kind(isize::MAX as u64 + 1),
            FileRangeWriterKind::Pool
        );
    }

    #[test]
    fn test_file_range_writer_kind_suggest_kind_boundary() {
        // 边界测试：恰好等于 isize::MAX 应该使用 Mmap
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

        // Small file should use Mmap
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

    #[tokio::test]
    async fn test_file_writer_error_file_exists() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("existing_file.bin");

        // Create a file
        std::fs::write(&file_path, b"existing content").unwrap();

        // Try to create a file with the same name should fail (using create_new)
        let result = FileWriter::new(&file_path, 1024).await;
        assert!(result.is_err());
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
        let io_err = std::io::Error::other("test error");
        let err = FileWriterError::Io(io_err);
        let display = format!("{}", err);
        assert!(display.contains("test error"));

        let chunk = Chunk {
            range: 0..10,
            data: Bytes::from_static(b"test"),
        };
        let write_err = FileWriterError::Write(chunk);
        let write_display = format!("{}", write_err);
        assert!(write_display.contains("failed to send chunk"));

        let finalize_err = FileWriterError::Finalize;
        let finalize_display = format!("{}", finalize_err);
        assert!(finalize_display.contains("finalize"));
    }
}
