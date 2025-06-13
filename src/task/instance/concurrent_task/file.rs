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
use bytes::Bytes;
use memmap2::MmapMut;

const FILE_WRITER_QUEUE_SIZE: usize = 1024;

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
    pub std::thread::JoinHandle<Result<(), std::io::Error>>,
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
        rx.await.map_err(FileWriterError::Recv)?;
        Ok(())
    }
}

pub struct FileWriter {
    total: u64,
    path: PathBuf,
}

impl FileWriter {
    pub fn new(path: impl AsRef<Path>, total: u64) -> Self {
        Self {
            total,
            path: path.as_ref().to_path_buf(),
        }
    }

    fn handle_seek_write(
        file: &mut std::fs::File,
        range: Range<u64>,
        data: Bytes,
    ) -> Result<(), std::io::Error> {
        file.seek(std::io::SeekFrom::Start(range.start))?;
        file.write_all(&data)?;
        Ok(())
    }

    pub fn start(self) -> FileWriterGuard {
        let (tx, rx) = async_channel::bounded(FILE_WRITER_QUEUE_SIZE);
        let total = self.total;
        let handle = std::thread::spawn(move || {
            let meta = std::fs::metadata(&self.path)?;
            let mut file = OpenOptions::new().write(true).open(&self.path)?;
            if meta.len() != total {
                file.set_len(total)?;
            }

            // In 32-bit machine, the pointer size is 4 bytes, some large file may exceed the pointer size
            // so we use the seek write to write the file
            if meta.len() <= isize::MAX as u64 {
                let mut mmap = unsafe { MmapMut::map_mut(&file)? };
                while let Ok(Payload(range, data, tx)) = rx.recv_blocking() {
                    mmap[range.start as usize..range.end as usize].copy_from_slice(&data);
                    let _ = tx.send(mmap.flush_async_range(
                        range.start as usize,
                        range.end as usize - range.start as usize,
                    ));
                }
            } else {
                while let Ok(Payload(range, data, tx)) = rx.recv_blocking() {
                    let _ = tx.send(Self::handle_seek_write(&mut file, range, data));
                }
            }

            Ok(())
        });
        FileWriterGuard(handle, FileWriterControl::new(tx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_file_writer() {
        let tmp_file = tempfile::tempdir().unwrap();
        let file_path = tmp_file.path().join("test.txt");

        let bytes = Bytes::from_static(b"Hello, world!");
        let bytes_len = bytes.len();

        let file_writer = FileWriter::new(&file_path, bytes_len as u64);
        let FileWriterGuard(handle, guard) = file_writer.start();
        handle.join().unwrap().unwrap();

        guard
            .write(0..bytes_len as u64, bytes.clone())
            .await
            .unwrap();

        let file = OpenOptions::new().read(true).open(&file_path).unwrap();
        let mmap = unsafe { memmap2::Mmap::map(&file).unwrap() };
        assert_eq!(mmap[0..bytes_len], bytes);
    }
}
