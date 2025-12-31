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
