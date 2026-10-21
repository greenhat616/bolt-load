use std::{io, ops::Range};

use async_channel::{Receiver, Sender};
use async_waitgroup::WaitGroup;
use bytes::Bytes;
use fs::File;
use fs_err as fs;
use snafu::prelude::*;

use super::{
    Chunk, CloseOnExit, Command, CommandError, FileRangeWriter, FileWriterBuilderError,
    FileWriterCapability, FileWriterError, FinalizeSnafu, OpenOrCreateFileSnafu, ValidationSnafu,
    WriteRangeSnafu, WriteReply, finalize_and_join,
};

pub struct PoolWriter {
    pub file: File,
    pub(super) tx: Sender<Command>,
    worker_wg: WaitGroup,
}

impl FileRangeWriter for PoolWriter {
    async fn finalize(self) -> Result<(), FileWriterError> {
        let Self {
            file,
            tx,
            worker_wg,
        } = self;
        let path = file.path().to_path_buf();
        let joined = finalize_and_join(tx, worker_wg, path.clone(), Command::Finalize).await;
        // Sync once every worker is gone: one may still have been writing
        // when another took the finalize command.
        let synced = blocking::unblock(move || file.sync_all())
            .await
            .map_err(CommandError::from)
            .context(FinalizeSnafu { path });
        joined.and(synced)
    }

    async fn write_range(&self, range: Range<u64>, data: Bytes) -> Result<(), FileWriterError> {
        let (tx, rx) = oneshot::channel();
        let chunk = Chunk { range, data };
        self.tx
            .send(Command::Write(chunk.clone(), WriteReply::Awaited(tx)))
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

/// A file written at explicit offsets.
trait WriteAt {
    /// Write a prefix of `buf` at `offset`, returning its length.
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize>;
}

impl WriteAt for File {
    #[cfg(unix)]
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        fs::os::unix::fs::FileExt::write_at(self, buf, offset)
    }

    #[cfg(windows)]
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        fs::os::windows::fs::FileExt::seek_write(self, buf, offset)
    }
}

/// Write the whole of `buf` at `offset`, retrying short and interrupted writes.
fn write_all_at(file: &impl WriteAt, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match file.write_at(buf, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write whole buffer",
                ));
            }
            Ok(written) => {
                buf = &buf[written..];
                offset += written as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn execute_write(file: &File, chunk: Chunk) -> io::Result<()> {
    write_all_at(file, &chunk.data, chunk.range.start)
}

fn file_writer_task(file: &File, command_rx: &Receiver<Command>) {
    let _close_on_exit = CloseOnExit(command_rx);
    while let Ok(command) = command_rx.recv_blocking() {
        match command {
            Command::Write(chunk, reply) => {
                reply.complete(execute_write(file, chunk), file.path());
            }
            Command::Finalize(tx) => {
                // The owner syncs the file once all workers exited.
                let _ = tx.send(Ok(()));
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
        // Clone every handle before spawning, so a failure never leaves workers behind.
        let worker_files = (0..parallel)
            .map(|_| {
                file.try_clone()
                    .map_err(CommandError::from)
                    .with_context(|_| OpenOrCreateFileSnafu {
                        path: file.path().to_path_buf(),
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let (tx, rx) = async_channel::bounded::<Command>(super::FILE_WRITER_QUEUE_SIZE);
        let worker_wg = WaitGroup::new();
        for worker_file in worker_files {
            let rx = rx.clone();
            let worker_guard = worker_wg.clone();
            blocking::unblock(move || {
                file_writer_task(&worker_file, &rx);
                drop(worker_file);
                drop(worker_guard);
            })
            .detach();
        }
        Ok(PoolWriter {
            file,
            tx,
            worker_wg,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};

    use tempfile::NamedTempFile;

    use super::*;

    /// One `write_at` call of [`ScriptedFile`].
    enum Step {
        /// Write at most this many bytes.
        Short(usize),
        Interrupted,
        Zero,
    }

    /// An in-memory file whose writes follow a script, then complete in full.
    struct ScriptedFile {
        content: std::cell::RefCell<Vec<u8>>,
        script: std::cell::RefCell<std::collections::VecDeque<Step>>,
    }

    impl ScriptedFile {
        fn new(len: usize, script: impl IntoIterator<Item = Step>) -> Self {
            Self {
                content: vec![0; len].into(),
                script: std::cell::RefCell::new(script.into_iter().collect()),
            }
        }
    }

    impl WriteAt for ScriptedFile {
        fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
            let written = match self.script.borrow_mut().pop_front() {
                None => buf.len(),
                Some(Step::Short(max)) => buf.len().min(max),
                Some(Step::Interrupted) => return Err(io::ErrorKind::Interrupted.into()),
                Some(Step::Zero) => return Ok(0),
            };
            let offset = offset as usize;
            self.content.borrow_mut()[offset..offset + written].copy_from_slice(&buf[..written]);
            Ok(written)
        }
    }

    #[test]
    fn test_write_all_at_retries_short_and_interrupted_writes() {
        let file = ScriptedFile::new(
            12,
            [
                Step::Short(3),
                Step::Interrupted,
                Step::Short(1),
                Step::Short(2),
            ],
        );
        write_all_at(&file, b"abcdefghij", 2).unwrap();
        assert_eq!(&file.content.borrow()[..], b"\0\0abcdefghij");
        assert!(file.script.borrow().is_empty());
    }

    #[test]
    fn test_write_all_at_fails_when_nothing_is_written() {
        let file = ScriptedFile::new(4, [Step::Short(1), Step::Zero]);
        let err = write_all_at(&file, b"abcd", 0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WriteZero);
        assert_eq!(&file.content.borrow()[..], b"a\0\0\0");
    }

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
            .parallel(4) // Use 4 parallel write threads
            .build()
            .unwrap();

        // Concurrently write multiple blocks
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

        // Validate that all data are written
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

        // If creation succeeds, it means the default parallelism is set correctly
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

        // Use a single thread
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
