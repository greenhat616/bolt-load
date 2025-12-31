use std::ops::Range;

use async_channel::{Receiver, Sender};
use bytes::Bytes;
use fs::File;
use fs_err as fs;
use memmap2::MmapMut;

use super::{Chunk, FileRangeWriter, FileWriterCapability, FileWriterError};
use crate::utils::logging::*;

pub struct MmapWriter {
    pub file: File,
    tx: Sender<Command>,
}

impl FileRangeWriter for MmapWriter {
    async fn finalize(self) -> Result<(), FileWriterError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Command::Finalize(tx))
            .await
            .map_err(|_| FileWriterError::Finalize)?;
        rx.await
            .map_err(FileWriterError::Recv)?
            .map_err(FileWriterError::Io)?;
        Ok(())
    }

    async fn write_range(&self, range: Range<u64>, data: Bytes) -> Result<(), FileWriterError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Command::Write(
                Chunk {
                    range: range.clone(),
                    data: data.clone(),
                },
                tx,
            ))
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

enum Command {
    Write(Chunk, oneshot::Sender<Result<(), std::io::Error>>),
    Finalize(oneshot::Sender<Result<(), std::io::Error>>),
}

pub struct MmapWriterBuilder {
    pub file: Option<File>,
}

impl FileWriterCapability for MmapWriterBuilder {
    fn is_supported(file_size: u64) -> bool {
        file_size <= isize::MAX as u64
    }
}

fn mmap_writer_task(
    ready_tx: oneshot::Sender<Result<(), std::io::Error>>,
    file: &File,
    command_rx: &Receiver<Command>,
) {
    let mut mmap = unsafe {
        match MmapMut::map_mut(file) {
            Ok(mmap) => mmap,
            Err(e) => {
                error!("failed to map file: {e:?}");
                let _ = ready_tx.send(Err(e));
                return;
            }
        }
    };
    let _ = ready_tx.send(Ok(()));
    while let Ok(command) = command_rx.recv_blocking() {
        match command {
            Command::Write(Chunk { range, data }, tx) => {
                mmap[range.start as usize..range.end as usize].copy_from_slice(&data);
                #[cfg(unix)]
                {
                    use memmap2::UncheckedAdvice;
                    if let Err(e) = mmap.unchecked_advise_range(
                        UncheckedAdvice::DontNeed,
                        range.start as usize,
                        (range.end - range.start) as usize,
                    ) {
                        error!("failed to advise range: {e:?}");
                    }
                }
                #[cfg(windows)]
                {
                    evict_working_set_range(
                        &mut mmap,
                        range.start as usize,
                        (range.end - range.start) as usize,
                    );
                }
                let _ = tx.send(Ok(()));
            }
            Command::Finalize(tx) => {
                let _ = match mmap.flush() {
                    Ok(_) => tx.send(Ok(())),
                    Err(e) => tx.send(Err(e)),
                };
                break;
            }
        }
    }
}

#[cfg(windows)]
fn evict_working_set_range(mmap: &mut memmap2::MmapMut, start: usize, len: usize) {
    use windows_sys::Win32::{
        Foundation::{ERROR_NOT_LOCKED, GetLastError},
        System::Memory::VirtualUnlock,
    };

    unsafe {
        let addr = mmap.as_mut_ptr().add(start) as *mut core::ffi::c_void;
        let ok = VirtualUnlock(addr, len);

        if ok == 0 {
            let err = GetLastError();
            if err != ERROR_NOT_LOCKED {
                error!("failed to unlock range: {err:?}");
            }
        }
    }
}

impl MmapWriterBuilder {
    pub fn new() -> Self {
        Self { file: None }
    }

    pub fn file(mut self, file: File) -> Self {
        self.file = Some(file);
        self
    }

    pub async fn build(self) -> Result<MmapWriter, std::io::Error> {
        let Some(file) = self.file else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "file is not set",
            ));
        };

        let (chunk_tx, chunk_rx) = async_channel::bounded::<Command>(super::FILE_WRITER_QUEUE_SIZE);
        let (ready_tx, ready_rx) = oneshot::channel();
        let file_clone = file.try_clone()?;
        blocking::unblock(move || mmap_writer_task(ready_tx, &file_clone, &chunk_rx)).detach();

        ready_rx
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Channel closed"))??;
        Ok(MmapWriter { file, tx: chunk_tx })
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};

    use tempfile::NamedTempFile;

    use super::*;

    #[test]
    fn test_mmap_writer_builder_new() {
        let builder = MmapWriterBuilder::new();
        assert!(builder.file.is_none());
    }

    #[test]
    fn test_mmap_writer_capability_is_supported() {
        // 小文件应该被支持
        assert!(MmapWriterBuilder::is_supported(1024));
        assert!(MmapWriterBuilder::is_supported(1024 * 1024 * 1024)); // 1GB

        // 大于 isize::MAX 的文件不应该被支持
        assert!(!MmapWriterBuilder::is_supported(isize::MAX as u64 + 1));
    }

    #[test]
    fn test_mmap_writer_builder_without_file() {
        futures_lite::future::block_on(async {
            let builder = MmapWriterBuilder::new();
            let result = builder.build().await;
            match result {
                Err(err) => assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput),
                Ok(_) => panic!("Expected error, got Ok"),
            }
        });
    }

    #[test]
    fn test_mmap_writer_builder_with_file() {
        futures_lite::future::block_on(async {
            let temp_file = NamedTempFile::new().unwrap();
            let file_size = 1024u64;

            // 预先设置文件大小以便 mmap 工作
            temp_file.as_file().set_len(file_size).unwrap();

            let file = fs::File::open(temp_file.path()).unwrap();
            let builder = MmapWriterBuilder::new().file(file);
            assert!(builder.file.is_some());
        });
    }

    #[test]
    fn test_mmap_writer_write_and_finalize() {
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

            let writer = MmapWriterBuilder::new().file(file).build().await.unwrap();

            // 写入数据
            let data = Bytes::from_static(b"Hello, Mmap!");
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
    fn test_mmap_writer_concurrent_writes() {
        futures_lite::future::block_on(async {
            let temp_file = NamedTempFile::new().unwrap();
            let file_size = 4096u64;
            temp_file.as_file().set_len(file_size).unwrap();

            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(temp_file.path())
                .unwrap();

            let writer = MmapWriterBuilder::new().file(file).build().await.unwrap();

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
}
