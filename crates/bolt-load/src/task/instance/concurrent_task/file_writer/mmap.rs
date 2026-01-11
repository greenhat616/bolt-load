use std::ops::Range;

use async_channel::{Receiver, Sender};
use bolt_load_utils::telemetry::*;
use bytes::Bytes;
use fs::File;
use fs_err as fs;
use memmap2::MmapMut;
use snafu::prelude::*;

use super::{
    Chunk, CommandError, FileRangeWriter, FileWriterBuilderError, FileWriterCapability,
    FileWriterError, FinalizeSnafu, OpenOrCreateFileSnafu, ValidationSnafu, WriteRangeSnafu,
};

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

enum Command {
    Write(Chunk, oneshot::Sender<Result<(), std::io::Error>>),
    Finalize(oneshot::Sender<Result<(), std::io::Error>>),
}

#[derive(Default)]
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
                    unsafe {
                        if let Err(e) = mmap.unchecked_advise_range(
                            UncheckedAdvice::DontNeed,
                            range.start as usize,
                            (range.end - range.start) as usize,
                        ) {
                            error!("failed to advise range: {e:?}");
                        }
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
        Self::default()
    }

    pub fn file(mut self, file: File) -> Self {
        self.file = Some(file);
        self
    }

    pub async fn build(self) -> Result<MmapWriter, FileWriterBuilderError> {
        let Some(file) = self.file else {
            return Err(ValidationSnafu {
                message: "file is not set".to_string(),
            }
            .build());
        };

        let (chunk_tx, chunk_rx) = async_channel::bounded::<Command>(super::FILE_WRITER_QUEUE_SIZE);
        let (ready_tx, ready_rx) = oneshot::channel();
        let file_clone = file
            .try_clone()
            .map_err(CommandError::from)
            .with_context(|_| OpenOrCreateFileSnafu {
                path: file.path().to_path_buf(),
            })?;
        blocking::unblock(move || mmap_writer_task(ready_tx, &file_clone, &chunk_rx)).detach();

        ready_rx
            .await
            .map_err(|_| CommandError::Recv)
            .with_context(|_| OpenOrCreateFileSnafu {
                path: file.path().to_path_buf(),
            })?
            .map_err(CommandError::from)
            .with_context(|_| OpenOrCreateFileSnafu {
                path: file.path().to_path_buf(),
            })?;
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

    #[tokio::test]
    async fn test_mmap_writer_builder_without_file() {
        let builder = MmapWriterBuilder::new();
        let result = builder.build().await;
        match result {
            Err(FileWriterBuilderError::Validation { message }) => {
                assert!(message.contains("file is not set"));
            }
            Err(_) => panic!("Expected Validation error"),
            Ok(_) => panic!("Expected error, got Ok"),
        }
    }

    #[tokio::test]
    async fn test_mmap_writer_builder_with_file() {
        let temp_file = NamedTempFile::new().unwrap();
        let file_size = 1024u64;

        // Pre-allocate the file size for mmap
        temp_file.as_file().set_len(file_size).unwrap();

        let file = fs::File::open(temp_file.path()).unwrap();
        let builder = MmapWriterBuilder::new().file(file);
        assert!(builder.file.is_some());
    }

    #[tokio::test]
    async fn test_mmap_writer_write_and_finalize() {
        let temp_file = NamedTempFile::new().unwrap();
        let file_size = 1024u64;

        // Pre-allocate the file size for mmap
        temp_file.as_file().set_len(file_size).unwrap();

        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(temp_file.path())
            .unwrap();

        let writer = MmapWriterBuilder::new().file(file).build().await.unwrap();

        // Write data
        let data = Bytes::from_static(b"Hello, Mmap!");
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
    async fn test_mmap_writer_concurrent_writes() {
        let temp_file = NamedTempFile::new().unwrap();
        let file_size = 4096u64;
        temp_file.as_file().set_len(file_size).unwrap();

        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(temp_file.path())
            .unwrap();

        let writer = MmapWriterBuilder::new().file(file).build().await.unwrap();

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
}
