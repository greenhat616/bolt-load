use std::{ops::Range, path::PathBuf};

use async_channel::{Receiver, Sender};
use async_waitgroup::WaitGroup;
use bytes::Bytes;
use compio::{BufResult, fs::OpenOptions, io::AsyncWriteAt, runtime::Runtime};
use snafu::prelude::*;

use super::{
    metrics::FileWriterMetrics, Chunk, CommandError, FileRangeWriter, FileWriterBuilderError,
    FileWriterCapability, FileWriterError, FinalizeSnafu, OpenOrCreateFileSnafu, ValidationSnafu,
    WriteRangeSnafu,
};

enum Command {
    Write(Chunk, oneshot::Sender<Result<(), std::io::Error>>),
    Finalize(oneshot::Sender<Result<(), std::io::Error>>),
}

pub struct CompioWriter {
    path: PathBuf,
    tx: Sender<Command>,
    metrics: FileWriterMetrics,
}

impl FileRangeWriter for CompioWriter {
    async fn write_range(&self, range: Range<u64>, data: Bytes) -> Result<(), FileWriterError> {
        let (tx, rx) = oneshot::channel();
        let chunk = Chunk {
            range: range.clone(),
            data: data.clone(),
        };
        let data_len = data.len() as u64;

        // Update queue depth before sending
        self.metrics.set_queue_depth(self.tx.len());

        self.tx
            .send(Command::Write(chunk.clone(), tx))
            .await
            .map_err(|e| {
                let Command::Write(chunk, _) = e.into_inner() else {
                    unreachable!()
                };
                FileWriterError::WriteRange {
                    source: CommandError::Send,
                    chunk: Some(chunk),
                    path: self.path.clone(),
                }
            })?;

        // Wait for write to complete
        rx.await
            .map_err(|_| CommandError::Recv)
            .with_context(|_| WriteRangeSnafu {
                chunk: Some(chunk.clone()),
                path: self.path.clone(),
            })?
            .map_err(CommandError::from)
            .with_context(|_| WriteRangeSnafu {
                chunk: Some(chunk),
                path: self.path.clone(),
            })?;

        // Update bytes written after successful write
        self.metrics.add_bytes_written(data_len);

        Ok(())
    }
    async fn finalize(self) -> Result<(), FileWriterError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Command::Finalize(tx))
            .await
            .map_err(|_| CommandError::Send)
            .with_context(|_| FinalizeSnafu {
                path: self.path.clone(),
            })?;
        rx.await
            .map_err(|_| CommandError::Recv)
            .with_context(|_| FinalizeSnafu {
                path: self.path.clone(),
            })?
            .map_err(CommandError::from)
            .with_context(|_| FinalizeSnafu {
                path: self.path.clone(),
            })?;
        Ok(())
    }
}

impl core::fmt::Debug for CompioWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompioWriter")
            .field("path", &self.path)
            .finish()
    }
}

fn compio_file_writer_task(
    path: PathBuf,
    ready_tx: oneshot::Sender<Result<(), std::io::Error>>,
    command_rx: &Receiver<Command>,
) {
    let rt = Runtime::new().expect("failed to create runtime");
    rt.block_on(async {
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)
            .await
        {
            Ok(file) => {
                let Ok(()) = ready_tx.send(Ok(())) else {
                    return;
                };
                file
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };
        let wg = WaitGroup::new();
        while let Ok(command) = command_rx.recv().await {
            match command {
                Command::Write(chunk, tx) => {
                    let mut file = file.clone();
                    let wg = wg.clone();
                    rt.spawn(async move {
                        let _wg = wg;
                        let BufResult(result, _) =
                            file.write_at(chunk.data, chunk.range.start).await;
                        let _ = match result {
                            Ok(_) => tx.send(Ok(())),
                            Err(e) => tx.send(Err(e)),
                        };
                    })
                    .detach();
                }
                Command::Finalize(tx) => {
                    // Wait for all write tasks to finish
                    wg.wait().await;
                    let _ = match file.sync_all().await {
                        Ok(_) => tx.send(Ok(())),
                        Err(e) => tx.send(Err(e)),
                    };
                    break;
                }
            }
        }
    });
}

#[derive(Default)]
pub struct CompioWriterBuilder {
    path: Option<PathBuf>,
    metrics: Option<FileWriterMetrics>,
}

impl FileWriterCapability for CompioWriterBuilder {}

impl CompioWriterBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn path(mut self, path: PathBuf) -> Self {
        self.path = Some(path);
        self
    }

    pub fn metrics(mut self, metrics: FileWriterMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    pub async fn build(self) -> Result<CompioWriter, FileWriterBuilderError> {
        let Some(path) = self.path else {
            return ValidationSnafu {
                message: "path is not set".to_string(),
            }
            .fail();
        };
        let metrics = self.metrics.unwrap_or_default();
        let (tx, rx) = async_channel::bounded::<Command>(super::FILE_WRITER_QUEUE_SIZE);
        let (ready_tx, ready_rx) = oneshot::channel();
        let path_clone = path.clone();
        blocking::unblock(move || compio_file_writer_task(path_clone, ready_tx, &rx)).detach();
        ready_rx
            .await
            .map_err(|_| CommandError::Recv)
            .with_context(|_| OpenOrCreateFileSnafu { path: path.clone() })?
            .map_err(CommandError::from)
            .with_context(|_| OpenOrCreateFileSnafu { path: path.clone() })?;
        Ok(CompioWriter {
            path,
            tx: tx.clone(),
            metrics,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ============================================================
    // Unit tests that do not require compio runtime (always run)
    // ============================================================

    #[test]
    fn test_compio_writer_builder_new() {
        let builder = CompioWriterBuilder::new();
        assert!(builder.path.is_none());
    }

    #[test]
    fn test_compio_writer_builder_path() {
        let path = std::path::PathBuf::from("/tmp/test.bin");
        let builder = CompioWriterBuilder::new().path(path.clone());
        assert_eq!(builder.path, Some(path));
    }

    #[test]
    fn test_compio_writer_builder_chain() {
        let path1 = std::path::PathBuf::from("/tmp/first.bin");
        let path2 = std::path::PathBuf::from("/tmp/second.bin");
        let builder = CompioWriterBuilder::new().path(path1).path(path2.clone());
        assert_eq!(builder.path, Some(path2));
    }

    #[test]
    fn test_compio_writer_builder_default_state() {
        let builder = CompioWriterBuilder::new();
        assert!(builder.path.is_none());
    }

    #[tokio::test]
    async fn test_compio_writer_builder_build_without_path() {
        let builder = CompioWriterBuilder::new();
        let result = builder.build().await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, FileWriterBuilderError::Validation { .. }));
        assert!(err.to_string().contains("path is not set"));
    }

    #[tokio::test]
    async fn test_compio_writer_build_nonexistent_parent_dir() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let file_path = temp_dir.path().join("nonexistent_dir").join("test.bin");

        let result = CompioWriterBuilder::new().path(file_path).build().await;

        // Should fail because parent directory doesn't exist
        assert!(result.is_err());
    }

    #[test]
    fn test_compio_writer_debug() {
        let writer = CompioWriter {
            path: std::path::PathBuf::from("/tmp/debug_test.bin"),
            tx: async_channel::bounded::<Command>(1).0,
        };
        let debug_str = format!("{:?}", writer);
        assert!(debug_str.contains("CompioWriter"));
        assert!(debug_str.contains("path"));
        assert!(debug_str.contains("debug_test.bin"));
    }

    #[test]
    fn test_file_writer_capability_default_implementation() {
        // FileWriterCapability::is_supported should return true by default
        assert!(CompioWriterBuilder::is_supported(0));
        assert!(CompioWriterBuilder::is_supported(1024));
        assert!(CompioWriterBuilder::is_supported(u64::MAX));
    }

    #[test]
    fn test_chunk_creation() {
        let chunk = Chunk {
            range: 100..200,
            data: Bytes::from_static(b"test chunk data"),
        };
        assert_eq!(chunk.range.start, 100);
        assert_eq!(chunk.range.end, 200);
        assert_eq!(&chunk.data[..], b"test chunk data");
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
            range: 50..150,
            data: Bytes::from_static(b"debug test"),
        };
        let debug_str = format!("{:?}", chunk);
        assert!(debug_str.contains("Chunk"));
        assert!(debug_str.contains("range"));
        assert!(debug_str.contains("data"));
    }

    #[test]
    fn test_command_enum_variants() {
        // Test that Command enum can be constructed with Write variant
        let chunk = Chunk {
            range: 0..10,
            data: Bytes::from_static(b"test"),
        };
        let (tx, _rx) = oneshot::channel();
        let _cmd = Command::Write(chunk, tx);

        // Test Finalize variant
        let (tx2, _rx2) = oneshot::channel();
        let _cmd2 = Command::Finalize(tx2);
    }

    use std::io::{Read, Seek, SeekFrom};

    use tempfile::TempDir;

    use super::super::*;

    #[tokio::test]
    async fn test_compio_writer_builder_build_success() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test_build.bin");

        let writer = CompioWriterBuilder::new()
            .path(file_path.clone())
            .build()
            .await
            .unwrap();

        assert_eq!(writer.path, file_path);
        writer.finalize().await.unwrap();
    }

    #[tokio::test]
    async fn test_compio_writer_write_range_single() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test_write_single.bin");
        let writer = CompioWriterBuilder::new()
            .path(file_path.clone())
            .build()
            .await
            .unwrap();

        // Write data at the beginning
        let data = Bytes::from_static(b"Hello, CompioWriter!");
        writer
            .write_range(0..data.len() as u64, data.clone())
            .await
            .unwrap();

        writer.finalize().await.unwrap();

        // Verify written data
        let mut file = std::fs::File::open(&file_path).unwrap();
        let mut buffer = vec![0u8; data.len()];
        file.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, &data[..]);
    }

    #[tokio::test]
    async fn test_compio_writer_write_range_multiple() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test_write_multiple.bin");
        let writer = CompioWriterBuilder::new()
            .path(file_path.clone())
            .build()
            .await
            .unwrap();

        // Write multiple ranges
        let data1 = Bytes::from_static(b"First block");
        let data2 = Bytes::from_static(b"Second block");
        let data3 = Bytes::from_static(b"Third block");

        writer
            .write_range(0..data1.len() as u64, data1.clone())
            .await
            .unwrap();
        writer
            .write_range(500..500 + data2.len() as u64, data2.clone())
            .await
            .unwrap();
        writer
            .write_range(1000..1000 + data3.len() as u64, data3.clone())
            .await
            .unwrap();

        writer.finalize().await.unwrap();

        // Verify all written data
        let mut file = std::fs::File::open(&file_path).unwrap();

        let mut buffer1 = vec![0u8; data1.len()];
        file.read_exact(&mut buffer1).unwrap();
        assert_eq!(&buffer1, &data1[..]);

        file.seek(SeekFrom::Start(500)).unwrap();
        let mut buffer2 = vec![0u8; data2.len()];
        file.read_exact(&mut buffer2).unwrap();
        assert_eq!(&buffer2, &data2[..]);

        file.seek(SeekFrom::Start(1000)).unwrap();
        let mut buffer3 = vec![0u8; data3.len()];
        file.read_exact(&mut buffer3).unwrap();
        assert_eq!(&buffer3, &data3[..]);
    }

    #[tokio::test]
    async fn test_compio_writer_write_range_overwrite() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test_overwrite.bin");
        let writer = CompioWriterBuilder::new()
            .path(file_path.clone())
            .build()
            .await
            .unwrap();

        // Write initial data
        let data1 = Bytes::from_static(b"AAAAAAAAAA");
        writer
            .write_range(0..data1.len() as u64, data1.clone())
            .await
            .unwrap();

        // Overwrite part of the data
        let data2 = Bytes::from_static(b"BBBB");
        writer
            .write_range(3..3 + data2.len() as u64, data2.clone())
            .await
            .unwrap();

        writer.finalize().await.unwrap();

        // Verify the overwritten result
        let mut file = std::fs::File::open(&file_path).unwrap();
        let mut buffer = vec![0u8; 10];
        file.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, b"AAABBBBAAA");
    }

    #[tokio::test]
    async fn test_compio_writer_finalize() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test_finalize.bin");
        let writer = CompioWriterBuilder::new()
            .path(file_path.clone())
            .build()
            .await
            .unwrap();

        let data = Bytes::from_static(b"Finalize test data");
        writer
            .write_range(0..data.len() as u64, data.clone())
            .await
            .unwrap();

        // Finalize should sync and complete successfully
        let result = writer.finalize().await;
        assert!(result.is_ok());

        // Verify file still exists and has correct content
        assert!(file_path.exists());
        let mut file = std::fs::File::open(&file_path).unwrap();
        let mut buffer = vec![0u8; data.len()];
        file.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, &data[..]);
    }

    #[tokio::test]
    async fn test_compio_writer_concurrent_writes() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test_concurrent.bin");
        let writer = CompioWriterBuilder::new()
            .path(file_path.clone())
            .build()
            .await
            .unwrap();

        // Spawn multiple concurrent writes
        let write_futures: Vec<_> = (0..10)
            .map(|i| {
                let offset = i * 100;
                let data = Bytes::from(format!("block_{:03}", i));
                writer.write_range(offset..offset + data.len() as u64, data)
            })
            .collect();

        // Wait for all writes to complete
        for future in write_futures {
            future.await.unwrap();
        }

        writer.finalize().await.unwrap();

        // Verify all blocks were written correctly
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
    async fn test_compio_writer_large_write() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test_large_write.bin");
        let writer = CompioWriterBuilder::new()
            .path(file_path.clone())
            .build()
            .await
            .unwrap();

        // Write a large block (64KB)
        let data = Bytes::from(vec![0xABu8; 64 * 1024]);
        writer
            .write_range(0..data.len() as u64, data.clone())
            .await
            .unwrap();

        writer.finalize().await.unwrap();

        // Verify written data
        let mut file = std::fs::File::open(&file_path).unwrap();
        let mut buffer = vec![0u8; data.len()];
        file.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, &data[..]);
    }

    #[tokio::test]
    async fn test_compio_writer_write_at_high_offset() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test_write_high_offset.bin");
        let writer = CompioWriterBuilder::new()
            .path(file_path.clone())
            .build()
            .await
            .unwrap();

        // Write at a high offset
        let data = Bytes::from_static(b"High offset data");
        let offset = 1000u64;
        writer
            .write_range(offset..offset + data.len() as u64, data.clone())
            .await
            .unwrap();

        writer.finalize().await.unwrap();

        // Verify written data
        let mut file = std::fs::File::open(&file_path).unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        let mut buffer = vec![0u8; data.len()];
        file.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, &data[..]);
    }

    #[tokio::test]
    async fn test_compio_writer_empty_write() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test_empty_write.bin");
        let writer = CompioWriterBuilder::new()
            .path(file_path.clone())
            .build()
            .await
            .unwrap();

        // Write empty data
        let data = Bytes::new();
        writer.write_range(0..0, data).await.unwrap();

        writer.finalize().await.unwrap();

        // File should still exist
        assert!(file_path.exists());
    }

    #[tokio::test]
    async fn test_compio_writer_write_boundary_conditions() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test_boundary.bin");
        let writer = CompioWriterBuilder::new()
            .path(file_path.clone())
            .build()
            .await
            .unwrap();

        // Write at offset 0
        let data1 = Bytes::from_static(b"START");
        writer
            .write_range(0..data1.len() as u64, data1.clone())
            .await
            .unwrap();

        // Write at a later position
        let data2 = Bytes::from_static(b"END");
        let offset = 250u64;
        writer
            .write_range(offset..offset + data2.len() as u64, data2.clone())
            .await
            .unwrap();

        writer.finalize().await.unwrap();

        // Verify both writes
        let mut file = std::fs::File::open(&file_path).unwrap();

        let mut buffer1 = vec![0u8; data1.len()];
        file.read_exact(&mut buffer1).unwrap();
        assert_eq!(&buffer1, &data1[..]);

        file.seek(SeekFrom::Start(offset)).unwrap();
        let mut buffer2 = vec![0u8; data2.len()];
        file.read_exact(&mut buffer2).unwrap();
        assert_eq!(&buffer2, &data2[..]);
    }

    #[tokio::test]
    async fn test_compio_writer_sequential_writes() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test_sequential.bin");
        let writer = CompioWriterBuilder::new()
            .path(file_path.clone())
            .build()
            .await
            .unwrap();

        // Write sequentially
        let mut offset = 0u64;
        for i in 0..5 {
            let data = Bytes::from(format!("seq_{}", i));
            writer
                .write_range(offset..offset + data.len() as u64, data.clone())
                .await
                .unwrap();
            offset += data.len() as u64;
        }

        writer.finalize().await.unwrap();

        // Verify sequential data
        let content = std::fs::read(&file_path).unwrap();
        assert!(content.starts_with(b"seq_0seq_1seq_2seq_3seq_4"));
    }
}
