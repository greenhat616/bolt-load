//! a file writer designed for concurrent task
//!
//! Use memmap to write random access file,
//! and use seek write to write the file in large file.

use std::{
    fs::OpenOptions,
    io::{Seek, Write},
    ops::Range,
    path::{Path, PathBuf},
};

use async_channel::Sender;
use blocking::unblock;
use bytes::Bytes;
use memmap2::MmapMut;

use crate::utils::logging::*;

const FILE_WRITER_QUEUE_SIZE: usize = 2048;

#[derive(Debug, thiserror::Error)]
pub enum FileWriterError {
    #[error(transparent)]
    Io(std::io::Error),
    #[error(transparent)]
    Send(async_channel::SendError<Payload>),
    #[error(transparent)]
    Recv(oneshot::RecvError),
}

type PayloadResult = Result<(), std::io::Error>;

pub struct Payload(Range<u64>, Bytes, oneshot::Sender<PayloadResult>);

impl Payload {
    pub fn new(range: Range<u64>, data: Bytes, tx: oneshot::Sender<PayloadResult>) -> Self {
        Self(range, data, tx)
    }
}

pub struct FileWriterGuard(
    pub blocking::Task<std::io::Result<()>>,
    pub FileWriterControl,
);

#[derive(Debug, Clone)]
pub struct FileWriterControl {
    tx: Sender<Payload>,
}

impl FileWriterControl {
    pub fn new(tx: Sender<Payload>) -> Self {
        Self { tx }
    }

    /// write data to file
    ///
    /// # Errors
    ///
    /// This function will return an error if the channel is closed or the oneshot channel is dropped.
    pub async fn write(&self, range: Range<u64>, data: Bytes) -> Result<(), FileWriterError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Payload::new(range, data, tx))
            .await
            .map_err(FileWriterError::Send)?;
        rx.await
            .map_err(FileWriterError::Recv)?
            .map_err(FileWriterError::Io)?;
        Ok(())
    }
}

#[derive(Default, PartialEq, Eq, Debug)]
#[allow(dead_code)]
enum Mode {
    #[default]
    Mmap,
    SeekWrite,
}

#[derive(Debug)]
pub struct FileWriter {
    total: u64,
    path: PathBuf,
    mode: Mode,
}

impl FileWriter {
    pub fn new(path: impl AsRef<Path>, total: u64) -> Self {
        Self {
            total,
            path: path.as_ref().to_path_buf(),
            mode: Mode::default(),
        }
    }

    #[allow(unused)]
    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    #[cfg_attr(feature = "tracing", tracing::instrument)]
    fn handle_seek_write(
        file: &mut std::fs::File,
        range: Range<u64>,
        data: Bytes,
    ) -> Result<(), std::io::Error> {
        file.seek(std::io::SeekFrom::Start(range.start))?;
        file.write_all(&data)?;
        Ok(())
    }

    #[cfg_attr(feature = "tracing", tracing::instrument)]
    pub async fn start(self) -> Result<FileWriterGuard, std::io::Error> {
        let (ready_tx, ready_rx) = oneshot::channel();
        let (tx, rx) = async_channel::bounded(FILE_WRITER_QUEUE_SIZE);
        let total = self.total;
        let mode = self.mode;
        trace!("start file writer: {:?}", self.path);
        let handle: blocking::Task<std::io::Result<()>> = unblock(move || {
            let file_size = std::fs::metadata(&self.path)
                .map(|meta| meta.len())
                .unwrap_or(0);

            trace!("file size: {file_size:?}");
            let mut opts = OpenOptions::new();
            opts.read(true).write(true).create(true).truncate(false);

            let mut file = match opts.open(&self.path) {
                Ok(file) => file,
                Err(e) => {
                    error!("failed to open file: {e:?}");

                    let _ = ready_tx.send(Err(e));
                    return Ok(());
                }
            };
            if file_size != total {
                if let Err(e) = file.set_len(total) {
                    error!("failed to set file length: {e:?}");
                    let _ = ready_tx.send(Err(e));
                    return Ok(());
                }
            }

            // In 32-bit machine, the pointer size is 4 bytes, some large file may exceed the pointer size
            // so we use the seek write to write the file
            if file_size <= isize::MAX as u64 && mode == Mode::Mmap {
                trace!("use mmap mode");
                let mut mmap = unsafe {
                    match MmapMut::map_mut(&file) {
                        Ok(mmap) => mmap,
                        Err(e) => {
                            error!("failed to map file: {e:?}");
                            let _ = ready_tx.send(Err(e));
                            return Ok(());
                        }
                    }
                };
                let _ = ready_tx.send(Ok(()));
                while let Ok(Payload(range, data, tx)) = rx.recv_blocking() {
                    mmap[range.start as usize..range.end as usize].copy_from_slice(&data);
                    let _ = tx.send(Ok(()));
                }
                mmap.flush()?;
            } else {
                trace!("use seek write mode");
                let _ = ready_tx.send(Ok(()));
                while let Ok(Payload(range, data, tx)) = rx.recv_blocking() {
                    let _ = tx.send(Self::handle_seek_write(&mut file, range, data));
                }
                file.flush()?;
            }
            Ok(())
        });
        let control = FileWriterControl::new(tx);
        ready_rx
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Channel closed"))??;
        Ok(FileWriterGuard(handle, control))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_mmap_file_writer() {
        let tmp_file = tempfile::tempdir().unwrap();
        let file_path = tmp_file.path().join("test.txt");

        let bytes = Bytes::from_static(b"Hello, world!");
        let bytes_len = bytes.len();

        let file_writer = FileWriter::new(&file_path, bytes_len as u64);
        let FileWriterGuard(handle, guard) = file_writer.start().await.unwrap();

        guard
            .write(0..bytes_len as u64, bytes.clone())
            .await
            .unwrap();

        drop(guard);
        handle.await.unwrap();

        let file = OpenOptions::new().read(true).open(&file_path).unwrap();
        let mmap = unsafe { memmap2::Mmap::map(&file).unwrap() };
        assert_eq!(mmap[0..bytes_len], bytes);
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_seek_write_file_writer() {
        let tmp_file = tempfile::tempdir().unwrap();
        let file_path = tmp_file.path().join("test.txt");

        let bytes = Bytes::from_static(b"Hello, world!");
        let bytes_len = bytes.len();

        let mut file_writer = FileWriter::new(&file_path, bytes_len as u64);
        file_writer.set_mode(Mode::SeekWrite);
        let FileWriterGuard(handle, guard) = file_writer.start().await.unwrap();

        guard
            .write(0..bytes_len as u64, bytes.clone())
            .await
            .unwrap();

        drop(guard);
        handle.await.unwrap();

        let file = OpenOptions::new().read(true).open(&file_path).unwrap();
        let mmap = unsafe { memmap2::Mmap::map(&file).unwrap() };
        assert_eq!(mmap[0..bytes_len], bytes);
    }
}
