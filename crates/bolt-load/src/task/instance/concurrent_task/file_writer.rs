//! a file writer designed for concurrent task
//!
//! Use memmap to write random access file,
//! and use seek write to write the file in large file.

#[cfg(feature = "compio")]
mod compio_writer;
#[cfg(feature = "mmap")]
mod mmap_writer;
mod null_writer;
mod pending_writer;
mod pool_writer;
#[cfg(test)]
mod slow;

use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use bytes::Bytes;
use fs_err::{File, OpenOptions};
use snafu::prelude::*;

#[cfg(feature = "compio")]
pub use self::compio_writer::CompioWriterBuilder;
#[cfg(feature = "mmap")]
pub use self::mmap_writer::MmapWriterBuilder;
use self::{null_writer::NullWriter, pool_writer::PoolWriter};
// Re-export builders for benchmarking and advanced usage
pub use self::{
    null_writer::NullWriterBuilder,
    pending_writer::{PendingWrite, PendingWriter, WriteCompletion, WriteStatus, WriterFullError},
    pool_writer::PoolWriterBuilder,
};
use crate::runtime::yield_now;

const FILE_WRITER_QUEUE_SIZE: usize = 2048;

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

pub trait FileRangeWriter
where
    Self: Send + Sync,
{
    /// Write data to file
    ///
    /// # Errors
    ///
    /// This function will return an error if the file is not writable or the disk is full.
    fn write_range(
        &self,
        range: Range<u64>,
        data: Bytes,
    ) -> impl Future<Output = Result<(), FileWriterError>> + Send;
    /// Sync all data to disk
    ///
    /// # Errors
    ///
    /// This function will return an error if the file is not writable or the disk is full.
    fn finalize(self) -> impl Future<Output = Result<(), FileWriterError>> + Send;
}

trait FileWriterCapability {
    fn is_supported(_file_size: u64) -> bool {
        true
    }
}

pub enum FileRangeWriterImpl {
    #[cfg(feature = "mmap")]
    Mmap(self::mmap_writer::MmapWriter),
    Pool(PoolWriter),
    Null(NullWriter),
    #[cfg(feature = "compio")]
    Compio(self::compio_writer::CompioWriter),
    #[cfg(test)]
    Slow(self::slow::SlowWriter),
}

impl FileRangeWriter for FileRangeWriterImpl {
    #[inline]
    async fn write_range(&self, range: Range<u64>, data: Bytes) -> Result<(), FileWriterError> {
        match self {
            #[cfg(feature = "mmap")]
            FileRangeWriterImpl::Mmap(writer) => writer.write_range(range, data).await,
            FileRangeWriterImpl::Pool(writer) => writer.write_range(range, data).await,
            FileRangeWriterImpl::Null(writer) => writer.write_range(range, data).await,
            #[cfg(feature = "compio")]
            FileRangeWriterImpl::Compio(writer) => writer.write_range(range, data).await,
            #[cfg(test)]
            FileRangeWriterImpl::Slow(writer) => writer.write_range(range, data).await,
        }
    }

    #[inline]
    async fn finalize(self) -> Result<(), FileWriterError> {
        match self {
            #[cfg(feature = "mmap")]
            FileRangeWriterImpl::Mmap(writer) => writer.finalize().await,
            FileRangeWriterImpl::Pool(writer) => writer.finalize().await,
            FileRangeWriterImpl::Null(writer) => writer.finalize().await,
            #[cfg(feature = "compio")]
            FileRangeWriterImpl::Compio(writer) => writer.finalize().await,
            #[cfg(test)]
            FileRangeWriterImpl::Slow(writer) => writer.finalize().await,
        }
    }
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
            #[cfg(test)]
            FileRangeWriterImpl::Slow(writer) => writer.write_range(range, data).await,
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
    #[cfg(test)]
    Slow(slow::SlowDiskConfig),
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

        let inner = match kind {
            #[cfg(feature = "mmap")]
            FileRangeWriterKind::Mmap => {
                let file = open_file(path.clone(), size).await?;
                FileRangeWriterImpl::Mmap(MmapWriterBuilder::new().file(file).build().await?)
            }
            FileRangeWriterKind::Pool => {
                let file = open_file(path.clone(), size).await?;
                FileRangeWriterImpl::Pool(PoolWriterBuilder::new().file(file).build()?)
            }
            FileRangeWriterKind::Null => {
                // For null writer, we don't need a file - just create the writer
                FileRangeWriterImpl::Null(NullWriterBuilder::new().build().await?)
            }
            #[cfg(feature = "compio")]
            FileRangeWriterKind::Compio => {
                use self::compio_writer::CompioWriterBuilder;
                FileRangeWriterImpl::Compio(
                    CompioWriterBuilder::new()
                        .path(path.clone())
                        .build()
                        .await?,
                )
            }
            #[cfg(test)]
            FileRangeWriterKind::Slow(cfg) => {
                let file = open_file(path.clone(), size).await?;
                FileRangeWriterImpl::Slow(
                    slow::SlowWriterBuilder::new()
                        .file(file)
                        .path(path.clone())
                        .config(cfg)
                        .build()
                        .await
                        .map_err(|e| FileWriterBuilderError::Validation {
                            message: e.to_string(),
                        })?,
                )
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
    use std::{
        collections::{BTreeSet, HashMap},
        io::{Read, Seek, SeekFrom},
        sync::Arc,
        time::Duration,
    };

    use async_waitgroup::WaitGroup;
    use bolt_load_tests::adapter::simple::{SimpleTestAdapterBuilder, calculate_blake3};
    use smol_cancellation_token::CancellationToken;
    use tempfile::TempDir;

    use super::{
        super::{ConcurrentTaskInner, RunnerManager, runner_manager::TaskState},
        *,
    };
    use crate::{adapter::BoltLoadAdapter, runtime::ThreadedRuntimeImpl};

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

    #[tokio::test]
    async fn test_slow_writer_works() {
        use std::time::Duration;
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test_slow.bin");

        let cfg = slow::SlowDiskConfig {
            latency: Duration::from_millis(5),
            max_jitter: Duration::from_millis(1),
        };

        let writer = FileWriter::new_with_kind(&file_path, 1024, FileRangeWriterKind::Slow(cfg))
            .await
            .unwrap();

        let data = Bytes::from_static(b"slow!");
        writer
            .write_range(10..10 + data.len() as u64, data.clone())
            .await
            .unwrap();
        writer.finalize().await.unwrap();

        let mut file = std::fs::File::open(&file_path).unwrap();
        file.seek(SeekFrom::Start(10)).unwrap();
        let mut buf = vec![0u8; data.len()];
        file.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, &data[..]);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[n0_tracing_test::traced_test]
    async fn test_slow_writer_backpressure_multi_runner_byte_integrity() {
        const TOTAL_SIZE: usize = 256 * 1024;
        const CHUNK_SIZE: usize = 1024;
        const RUNNER_COUNT: usize = 4;

        fn record_completions(
            completions: Vec<WriteCompletion>,
            written_bytes: &mut usize,
            write_count: &mut usize,
        ) {
            for completion in completions {
                let range = completion.range;
                completion
                    .result
                    .unwrap_or_else(|e| panic!("write failed at {range:?}: {e}"));
                *written_bytes += (range.end - range.start) as usize;
                *write_count += 1;
            }
        }

        let rt = ThreadedRuntimeImpl::new_tokio_rt();
        let adapter = SimpleTestAdapterBuilder::new()
            .content_size(TOTAL_SIZE)
            .chunk_size(CHUNK_SIZE)
            .support_range(true)
            .build()
            .expect("adapter should build");
        let expected_hash = adapter.expected_hash().to_string();
        let adapter = Arc::new(Box::new(adapter) as Box<dyn BoltLoadAdapter + Send>);

        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("slow_backpressure_multi_runner.bin");
        let slow_cfg = slow::SlowDiskConfig {
            latency: Duration::from_millis(10),
            max_jitter: Duration::from_millis(2),
        };
        let file_writer = FileWriter::new_with_kind(
            &file_path,
            TOTAL_SIZE as u64,
            FileRangeWriterKind::Slow(slow_cfg),
        )
        .await
        .expect("slow file writer should build");

        let mut pending_writer = PendingWriter::new(Arc::new(file_writer.clone()), rt.clone(), 1);
        let mut runner_manager = RunnerManager::new(TOTAL_SIZE as u64, RUNNER_COUNT);
        let runners_cancel_token = CancellationToken::new();
        let wg = WaitGroup::new();
        let partition_size = TOTAL_SIZE as u64 / RUNNER_COUNT as u64;

        for idx in 0..RUNNER_COUNT {
            let start = idx as u64 * partition_size;
            let end = if idx + 1 == RUNNER_COUNT {
                TOTAL_SIZE as u64
            } else {
                start + partition_size
            };
            let range = start..end;
            let runner_range = range.clone();
            let adapter = adapter.clone();
            let token = runners_cancel_token.clone();

            runner_manager
                .allocate_pending_runner_with_chunk(range, |runner_id, control_rx| {
                    ConcurrentTaskInner::create_background_range_runner(
                        &rt,
                        &wg,
                        runner_range,
                        adapter,
                        control_rx,
                        runner_id,
                        token,
                    )
                })
                .expect("runner slot should be available");
        }

        let mut meters = HashMap::with_capacity(RUNNER_COUNT);
        let mut written_bytes = 0usize;
        let mut write_count = 0usize;
        let mut downloaded_chunks = 0usize;
        let mut touched_partitions = BTreeSet::new();
        let mut observed_backpressure = false;

        let download_result = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                record_completions(
                    pending_writer.try_tick(),
                    &mut written_bytes,
                    &mut write_count,
                );

                if !pending_writer.can_write() {
                    observed_backpressure = true;
                    let completions = pending_writer
                        .tick()
                        .await
                        .expect("writer completion channel should stay open");
                    record_completions(completions, &mut written_bytes, &mut write_count);
                    continue;
                }

                let tick = runner_manager.tick(&mut meters).await;
                if let Some(chunk) = tick.downloaded {
                    downloaded_chunks += 1;
                    touched_partitions.insert(
                        (chunk.range.start / partition_size).min((RUNNER_COUNT - 1) as u64),
                    );

                    let status = pending_writer
                        .write_range(chunk.range, chunk.bytes)
                        .expect("backpressure gate should only write when capacity is available");
                    if status == WriteStatus::Pending || !pending_writer.can_write() {
                        observed_backpressure = true;
                    }
                }

                if tick.state == TaskState::Finished {
                    break;
                }
            }
        })
        .await;

        if download_result.is_err() {
            runners_cancel_token.cancel();
        }
        download_result.expect("multi-runner download should not time out");

        wg.wait().await;
        let completions = pending_writer.flush().await;
        record_completions(completions, &mut written_bytes, &mut write_count);
        drop(pending_writer);

        file_writer
            .finalize()
            .await
            .expect("slow writer should finalize");

        assert!(
            observed_backpressure,
            "slow writer should force the runner loop to wait for write capacity"
        );
        assert_eq!(
            touched_partitions.len(),
            RUNNER_COUNT,
            "all runner ranges should contribute bytes"
        );
        assert!(
            downloaded_chunks >= RUNNER_COUNT,
            "expected chunks from multiple runner ranges, got {downloaded_chunks}"
        );
        assert!(
            write_count >= RUNNER_COUNT,
            "expected multiple writes, got {write_count}"
        );
        assert_eq!(written_bytes, TOTAL_SIZE);

        let content = std::fs::read(&file_path).unwrap();
        assert_eq!(content.len(), TOTAL_SIZE);
        assert_eq!(calculate_blake3(&content), expected_hash);
    }
}
