use std::ops::Range;

use async_channel::{Receiver, Sender};
use bytes::Bytes;
use fs::File;
use fs_err as fs;
use snafu::prelude::*;

use super::{
    Chunk, CommandError, FileRangeWriter, FileWriterBuilderError, FileWriterCapability,
    FileWriterError, FinalizeSnafu, OpenOrCreateFileSnafu, ValidationSnafu, WriteRangeSnafu,
};

pub struct PoolWriter {
    pub file: File,
    tx: Sender<Command>,
}

enum Command {
    Write(Chunk, oneshot::Sender<Result<(), std::io::Error>>),
    Finalize(oneshot::Sender<Result<(), std::io::Error>>),
}

impl FileRangeWriter for PoolWriter {
    async fn finalize(self) -> Result<(), FileWriterError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Command::Finalize(tx))
            .await
            .map_err(|_| CommandError::Send)
            .with_context(|_| FinalizeSnafu {
                path: self.file.path().to_path_buf(),
            })?;
        rx.await
            .map_err(|_| CommandError::Recv)
            .with_context(|_| FinalizeSnafu {
                path: self.file.path().to_path_buf(),
            })?
            .map_err(CommandError::from)
            .with_context(|_| FinalizeSnafu {
                path: self.file.path().to_path_buf(),
            })?;
        self.tx.close();
        Ok(())
    }

    async fn write_range(&self, range: Range<u64>, data: Bytes) -> Result<(), FileWriterError> {
        let (tx, rx) = oneshot::channel();
        let chunk = Chunk { range, data };
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
                    path: self.file.path().to_path_buf(),
                }
            })?;
        rx.await
            .map_err(|_| CommandError::Recv)
            .with_context(|_| WriteRangeSnafu {
                chunk: Some(chunk.clone()),
                path: self.file.path().to_path_buf(),
            })?
            .map_err(CommandError::from)
            .with_context(|_| WriteRangeSnafu {
                chunk: Some(chunk),
                path: self.file.path().to_path_buf(),
            })?;
        Ok(())
    }
}

#[derive(Default)]
pub struct PoolWriterBuilder {
    pub file: Option<File>,
    pub parallel: Option<usize>,
}

impl FileWriterCapability for PoolWriterBuilder {}

fn file_writer_task(file: &File, command_rx: &Receiver<Command>) {
    while let Ok(command) = command_rx.recv_blocking() {
        match command {
            Command::Write(chunk, tx) => {
                #[cfg(windows)]
                {
                    use fs::os::windows::fs::FileExt;
                    let _ = match file.seek_write(&chunk.data, chunk.range.start) {
                        Ok(_) => tx.send(Ok(())),
                        Err(e) => tx.send(Err(e)),
                    };
                }
                #[cfg(unix)]
                {
                    use fs::os::unix::fs::FileExt;
                    let _ = match file.write_at(&chunk.data, chunk.range.start) {
                        Ok(_) => tx.send(Ok(())),
                        Err(e) => tx.send(Err(e)),
                    };
                }
            }
            Command::Finalize(tx) => {
                // The `flush` on unix and windows is a no-op,
                // and we can just use `sync_all` for synchronizing all data to disk.
                let _ = match file.sync_all() {
                    Ok(_) => tx.send(Ok(())),
                    Err(e) => tx.send(Err(e)),
                };
                break;
            }
        }
    }
}

impl PoolWriterBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn file(mut self, file: File) -> Self {
        self.file = Some(file);
        self
    }

    pub fn parallel(mut self, parallel: usize) -> Self {
        self.parallel = Some(parallel);
        self
    }

    pub fn build(self) -> Result<PoolWriter, FileWriterBuilderError> {
        let Some(file) = self.file else {
            return ValidationSnafu {
                message: "file is not set".to_string(),
            }
            .fail();
        };
        let parallel = self
            .parallel
            .unwrap_or(std::thread::available_parallelism().unwrap().get());
        let (tx, rx) = async_channel::bounded::<Command>(super::FILE_WRITER_QUEUE_SIZE);
        for _ in 0..parallel {
            let file = file
                .try_clone()
                .map_err(CommandError::from)
                .with_context(|_| OpenOrCreateFileSnafu {
                    path: file.path().to_path_buf(),
                })?;
            let rx = rx.clone();
            blocking::unblock(move || file_writer_task(&file, &rx)).detach();
        }
        Ok(PoolWriter {
            file,
            tx: tx.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};

    use tempfile::NamedTempFile;

    use super::*;

    #[test]
    fn test_pool_writer_builder_new() {
        let builder = PoolWriterBuilder::new();
        assert!(builder.file.is_none());
        assert!(builder.parallel.is_none());
    }

    #[test]
    fn test_pool_writer_capability_is_supported() {
        // PoolWriter should support any size of file
        assert!(PoolWriterBuilder::is_supported(1024));
        assert!(PoolWriterBuilder::is_supported(isize::MAX as u64));
        assert!(PoolWriterBuilder::is_supported(isize::MAX as u64 + 1));
        assert!(PoolWriterBuilder::is_supported(u64::MAX));
    }

    #[test]
    fn test_pool_writer_builder_without_file() {
        let builder = PoolWriterBuilder::new();
        let result = builder.build();
        match result {
            Err(err) => assert!(matches!(err, FileWriterBuilderError::Validation { .. })),
            Ok(_) => panic!("Expected error, got Ok"),
        }
    }

    #[test]
    fn test_pool_writer_builder_with_file() {
        let temp_file = NamedTempFile::new().unwrap();

        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(temp_file.path())
            .unwrap();

        let builder = PoolWriterBuilder::new().file(file);
        assert!(builder.file.is_some());
    }

    #[test]
    fn test_pool_writer_builder_with_parallel() {
        let builder = PoolWriterBuilder::new().parallel(4);
        assert_eq!(builder.parallel, Some(4));
    }

    #[tokio::test]
    async fn test_pool_writer_write_and_finalize() {
        let temp_file = NamedTempFile::new().unwrap();
        let file_size = 1024u64;

        // Pre-allocate the file size
        temp_file.as_file().set_len(file_size).unwrap();

        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(temp_file.path())
            .unwrap();

        let writer = PoolWriterBuilder::new()
            .file(file)
            .parallel(2)
            .build()
            .unwrap();

        // Write data
        let data = Bytes::from_static(b"Hello, Pool!");
        writer
            .write_range(0..data.len() as u64, data.clone())
            .await
            .unwrap();

        // Write to different position
        let data2 = Bytes::from_static(b"World!");
        writer
            .write_range(100..100 + data2.len() as u64, data2.clone())
            .await
            .unwrap();

        // Finalize
        writer.finalize().await.unwrap();

        // Validate written data
        let mut verify_file = std::fs::File::open(temp_file.path()).unwrap();
        let mut buffer = vec![0u8; data.len()];
        verify_file.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, &data[..]);

        // Validate data at second position
        verify_file.seek(SeekFrom::Start(100)).unwrap();
        let mut buffer2 = vec![0u8; data2.len()];
        verify_file.read_exact(&mut buffer2).unwrap();
        assert_eq!(&buffer2, &data2[..]);
    }

    #[tokio::test]
    async fn test_pool_writer_concurrent_writes() {
        let temp_file = NamedTempFile::new().unwrap();
        let file_size = 4096u64;
        temp_file.as_file().set_len(file_size).unwrap();

        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(temp_file.path())
            .unwrap();

        let writer = PoolWriterBuilder::new()
            .file(file)
            .parallel(4) // 使用 4 个并行写入线程
            .build()
            .unwrap();

        // Concurrent write multiple blocks
        let write_tasks: Vec<_> = (0..10)
            .map(|i| {
                let offset = i * 100;
                let data = Bytes::from(format!("chunk_{:03}", i));
                writer.write_range(offset..offset + data.len() as u64, data)
            })
            .collect();

        for task in write_tasks {
            task.await.unwrap();
        }

        writer.finalize().await.unwrap();

        // Validate all data are written
        let mut verify_file = std::fs::File::open(temp_file.path()).unwrap();
        for i in 0..10u64 {
            let offset = i * 100;
            let expected = format!("chunk_{:03}", i);
            verify_file.seek(SeekFrom::Start(offset)).unwrap();
            let mut buffer = vec![0u8; expected.len()];
            verify_file.read_exact(&mut buffer).unwrap();
            assert_eq!(String::from_utf8(buffer).unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn test_pool_writer_default_parallelism() {
        let temp_file = NamedTempFile::new().unwrap();

        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(temp_file.path())
            .unwrap();

        // Without setting parallel, the default value should be used
        let writer = PoolWriterBuilder::new().file(file).build().unwrap();

        // If it can be created successfully, it means the default parallelism is set correctly
        writer.finalize().await.unwrap();
    }

    #[tokio::test]
    async fn test_pool_writer_single_thread() {
        let temp_file = NamedTempFile::new().unwrap();
        let file_size = 1024u64;
        temp_file.as_file().set_len(file_size).unwrap();

        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(temp_file.path())
            .unwrap();

        // Use single thread
        let writer = PoolWriterBuilder::new()
            .file(file)
            .parallel(1)
            .build()
            .unwrap();

        let data = Bytes::from_static(b"Single thread test");
        writer
            .write_range(0..data.len() as u64, data.clone())
            .await
            .unwrap();

        writer.finalize().await.unwrap();

        let mut verify_file = std::fs::File::open(temp_file.path()).unwrap();
        let mut buffer = vec![0u8; data.len()];
        verify_file.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, &data[..]);
    }
}
