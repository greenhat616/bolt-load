//! Null writer implementation for benchmarking purposes.
//!
//! This writer accepts data but discards it immediately, simulating
//! a block device with unlimited write speed (like /dev/null on Unix
//! or NUL on Windows). This is useful for measuring pure framework
//! overhead without actual I/O latency.

use std::{ops::Range, path::PathBuf};

use async_channel::{Receiver, Sender};
use bytes::Bytes;
use snafu::prelude::*;

use super::{
    Chunk, CommandError, FileRangeWriter, FileWriterBuilderError, FileWriterCapability,
    FileWriterError, FileWriterFuture, FinalizeSnafu, OpenOrCreateFileSnafu, WriteRangeSnafu,
};

enum Command {
    Write(Chunk, oneshot::Sender<Result<(), std::io::Error>>),
    Finalize(oneshot::Sender<Result<(), std::io::Error>>),
}

/// A null writer that discards all data.
///
/// This implementation uses the same channel-based architecture as other
/// writers to ensure fair comparison in benchmarks.
pub struct NullWriter {
    tx: Sender<Command>,
}

/// A sentinel path used for NullWriter since it doesn't have an actual file.
const NULL_PATH: &str = "<null>";

impl FileRangeWriter for NullWriter {
    fn write_range(&self, range: Range<u64>, data: Bytes) -> FileWriterFuture<'_> {
        Box::pin(async move {
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
                        path: PathBuf::from(NULL_PATH),
                    }
                })?;
            rx.await
                .map_err(|_| CommandError::Recv)
                .with_context(|_| WriteRangeSnafu {
                    chunk: Some(chunk.clone()),
                    path: PathBuf::from(NULL_PATH),
                })?
                .map_err(CommandError::from)
                .with_context(|_| WriteRangeSnafu {
                    chunk: Some(chunk),
                    path: PathBuf::from(NULL_PATH),
                })?;
            Ok(())
        })
    }

    fn finalize(self) -> FileWriterFuture<'static> {
        Box::pin(async move {
            let (tx, rx) = oneshot::channel();
            self.tx
                .send(Command::Finalize(tx))
                .await
                .map_err(|_| CommandError::Send)
                .with_context(|_| FinalizeSnafu {
                    path: PathBuf::from(NULL_PATH),
                })?;
            rx.await
                .map_err(|_| CommandError::Recv)
                .with_context(|_| FinalizeSnafu {
                    path: PathBuf::from(NULL_PATH),
                })?
                .map_err(CommandError::from)
                .with_context(|_| FinalizeSnafu {
                    path: PathBuf::from(NULL_PATH),
                })?;
            Ok(())
        })
    }
}

impl core::fmt::Debug for NullWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NullWriter").finish()
    }
}

fn null_writer_task(
    ready_tx: oneshot::Sender<Result<(), std::io::Error>>,
    command_rx: &Receiver<Command>,
) {
    let _ = ready_tx.send(Ok(()));
    while let Ok(command) = command_rx.recv_blocking() {
        match command {
            Command::Write(_chunk, tx) => {
                // Discard the data immediately - simulate /dev/null
                let _ = tx.send(Ok(()));
            }
            Command::Finalize(tx) => {
                let _ = tx.send(Ok(()));
                break;
            }
        }
    }
}

pub struct NullWriterBuilder {
    /// Number of parallel worker threads
    pub parallel: Option<usize>,
}

impl FileWriterCapability for NullWriterBuilder {}

impl NullWriterBuilder {
    pub fn new() -> Self {
        Self { parallel: None }
    }

    pub fn parallel(mut self, parallel: usize) -> Self {
        self.parallel = Some(parallel);
        self
    }

    pub async fn build(self) -> Result<NullWriter, FileWriterBuilderError> {
        let parallel = self.parallel.unwrap_or(1);
        let (tx, rx) = async_channel::bounded::<Command>(super::FILE_WRITER_QUEUE_SIZE);

        for _ in 0..parallel {
            let rx = rx.clone();
            let (ready_tx, ready_rx) = oneshot::channel();
            blocking::unblock(move || null_writer_task(ready_tx, &rx)).detach();
            ready_rx
                .await
                .map_err(|_| CommandError::Recv)
                .with_context(|_| OpenOrCreateFileSnafu {
                    path: NULL_PATH.to_string(),
                })?
                .map_err(CommandError::from)
                .with_context(|_| OpenOrCreateFileSnafu {
                    path: NULL_PATH.to_string(),
                })?;
        }

        Ok(NullWriter { tx })
    }
}

impl Default for NullWriterBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_null_writer_builder_new() {
        let builder = NullWriterBuilder::new();
        assert!(builder.parallel.is_none());
    }

    #[test]
    fn test_null_writer_builder_parallel() {
        let builder = NullWriterBuilder::new().parallel(4);
        assert_eq!(builder.parallel, Some(4));
    }

    #[test]
    fn test_null_writer_capability() {
        assert!(NullWriterBuilder::is_supported(0));
        assert!(NullWriterBuilder::is_supported(u64::MAX));
    }

    #[tokio::test]
    async fn test_null_writer_write_and_finalize() {
        let writer = NullWriterBuilder::new().build().await.unwrap();

        // Write data - should succeed immediately
        let data = Bytes::from_static(b"Hello, NullWriter!");
        writer
            .write_range(0..data.len() as u64, data.clone())
            .await
            .unwrap();

        // Write to different position
        let data2 = Bytes::from_static(b"More data!");
        writer
            .write_range(100..100 + data2.len() as u64, data2.clone())
            .await
            .unwrap();

        // Finalize
        writer.finalize().await.unwrap();
    }

    #[tokio::test]
    async fn test_null_writer_concurrent_writes() {
        let writer = NullWriterBuilder::new().parallel(4).build().await.unwrap();

        // Concurrent write multiple blocks
        let write_futures: Vec<_> = (0..100)
            .map(|i| {
                let offset = i * 1024;
                let data = Bytes::from(vec![0xABu8; 1024]);
                writer.write_range(offset..offset + data.len() as u64, data)
            })
            .collect();

        for future in write_futures {
            future.await.unwrap();
        }

        writer.finalize().await.unwrap();
    }

    #[tokio::test]
    async fn test_null_writer_large_write() {
        let writer = NullWriterBuilder::new().build().await.unwrap();

        // Write a large block (1MB)
        let data = Bytes::from(vec![0xABu8; 1024 * 1024]);
        writer
            .write_range(0..data.len() as u64, data.clone())
            .await
            .unwrap();

        writer.finalize().await.unwrap();
    }

    #[test]
    fn test_null_writer_debug() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let writer = rt.block_on(NullWriterBuilder::new().build()).unwrap();
        let debug_str = format!("{:?}", writer);
        assert!(debug_str.contains("NullWriter"));
    }
}
