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
        match MmapMut::map_mut(&*file) {
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
                        range.start,
                        range.end - range.start,
                    ) {
                        error!("failed to advise range: {e:?}");
                    }
                }
                let _ = tx.send(Ok(()));
            }
            Command::Finalize(tx) => {
                match mmap.flush() {
                    Ok(_) => tx.send(Ok(())),
                    Err(e) => tx.send(Err(e)),
                };
                break;
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
