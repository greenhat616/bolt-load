use std::ops::Range;

use async_channel::{Receiver, Sender};
use bytes::Bytes;
use fs::File;
use fs_err as fs;

use super::{Chunk, FileRangeWriter, FileWriterCapability, FileWriterError};

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
            .map_err(|_| FileWriterError::Finalize)?;
        rx.await
            .map_err(FileWriterError::Recv)?
            .map_err(FileWriterError::Io)?;
        self.tx.close();
        Ok(())
    }

    async fn write_range(&self, range: Range<u64>, data: Bytes) -> Result<(), FileWriterError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Command::Write(Chunk { range, data }, tx))
            .await
            .map_err(|e| {
                let Command::Write(chunk, _) = e.into_inner() else {
                    unreachable!()
                };
                FileWriterError::Write(chunk)
            })?;
        rx.await
            .map_err(FileWriterError::Recv)?
            .map_err(FileWriterError::Io)?;
        Ok(())
    }
}

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
                match file.sync_all() {
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
        Self {
            file: None,
            parallel: None,
        }
    }

    pub fn file(mut self, file: File) -> Self {
        self.file = Some(file);
        self
    }

    pub fn parallel(mut self, parallel: usize) -> Self {
        self.parallel = Some(parallel);
        self
    }

    pub fn build(self) -> Result<PoolWriter, std::io::Error> {
        let Some(file) = self.file else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "file is not set",
            ));
        };
        let parallel = self
            .parallel
            .unwrap_or(std::thread::available_parallelism().unwrap().get());
        let (tx, rx) = async_channel::bounded::<Command>(super::FILE_WRITER_QUEUE_SIZE);
        for _ in 0..parallel {
            let file = file.try_clone()?;
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
    use super::*;
    use std::io::{Read, Seek, SeekFrom};
    use tempfile::NamedTempFile;

    #[test]
    fn test_pool_writer_builder_new() {
        let builder = PoolWriterBuilder::new();
        assert!(builder.file.is_none());
        assert!(builder.parallel.is_none());
    }

    #[test]
    fn test_pool_writer_capability_is_supported() {
        // PoolWriter 应该支持任意大小的文件
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
            Err(err) => assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput),
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

    #[test]
    fn test_pool_writer_write_and_finalize() {
        futures_lite::future::block_on(async {
            let temp_file = NamedTempFile::new().unwrap();
            let file_size = 1024u64;

            // 预先设置文件大小
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

            // 写入数据
            let data = Bytes::from_static(b"Hello, Pool!");
            writer
                .write_range(0..data.len() as u64, data.clone())
                .await
                .unwrap();

            // 写入到不同位置
            let data2 = Bytes::from_static(b"World!");
            writer
                .write_range(100..100 + data2.len() as u64, data2.clone())
                .await
                .unwrap();

            // 完成写入
            writer.finalize().await.unwrap();

            // 验证写入的数据
            let mut verify_file = std::fs::File::open(temp_file.path()).unwrap();
            let mut buffer = vec![0u8; data.len()];
            verify_file.read_exact(&mut buffer).unwrap();
            assert_eq!(&buffer, &data[..]);

            // 验证第二个位置的数据
            verify_file.seek(SeekFrom::Start(100)).unwrap();
            let mut buffer2 = vec![0u8; data2.len()];
            verify_file.read_exact(&mut buffer2).unwrap();
            assert_eq!(&buffer2, &data2[..]);
        });
    }

    #[test]
    fn test_pool_writer_concurrent_writes() {
        futures_lite::future::block_on(async {
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

            // 并发写入多个块
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

            // 验证所有数据都已写入
            let mut verify_file = std::fs::File::open(temp_file.path()).unwrap();
            for i in 0..10u64 {
                let offset = i * 100;
                let expected = format!("chunk_{:03}", i);
                verify_file.seek(SeekFrom::Start(offset)).unwrap();
                let mut buffer = vec![0u8; expected.len()];
                verify_file.read_exact(&mut buffer).unwrap();
                assert_eq!(String::from_utf8(buffer).unwrap(), expected);
            }
        });
    }

    #[test]
    fn test_pool_writer_default_parallelism() {
        let temp_file = NamedTempFile::new().unwrap();

        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(temp_file.path())
            .unwrap();

        // 不设置 parallel，应该使用默认值
        let writer = PoolWriterBuilder::new().file(file).build().unwrap();

        // 只要能成功创建就说明默认并行度设置正确
        futures_lite::future::block_on(async {
            writer.finalize().await.unwrap();
        });
    }

    #[test]
    fn test_pool_writer_single_thread() {
        futures_lite::future::block_on(async {
            let temp_file = NamedTempFile::new().unwrap();
            let file_size = 1024u64;
            temp_file.as_file().set_len(file_size).unwrap();

            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(temp_file.path())
                .unwrap();

            // 使用单线程
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
        });
    }
}
