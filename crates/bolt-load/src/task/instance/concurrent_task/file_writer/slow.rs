//! Test-only writer that simulates a slow disk by sleeping before each write.
//!
//! Enabled in tests via `#[cfg(test)]` or explicitly via the `slow_disk` feature.

use std::{
    io::{Seek, SeekFrom, Write},
    ops::Range,
    path::PathBuf,
    time::Duration,
};

use bytes::Bytes;
use fs_err::File;
use snafu::prelude::*;

use super::{CommandError, FileRangeWriter, FileWriterError, FinalizeSnafu, WriteRangeSnafu};
use crate::runtime::yield_now;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SlowDiskConfig {
    /// Base latency injected before every write (and before finalize).
    pub latency: Duration,
    /// Optional per-write extra latency, derived from `range.start` to avoid RNG in tests.
    /// (Keeps tests deterministic.)
    pub max_jitter: Duration,
}

impl Default for SlowDiskConfig {
    fn default() -> Self {
        Self {
            latency: Duration::from_millis(10),
            max_jitter: Duration::from_millis(0),
        }
    }
}

fn deterministic_jitter(max: Duration, seed: u64) -> Duration {
    if max.is_zero() {
        return Duration::ZERO;
    }
    // Deterministic "jitter" without RNG: LCG-ish scramble.
    let x = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    let nanos = (x % (max.as_nanos() as u64).max(1)) as u128;
    Duration::from_nanos(nanos as u64)
}

#[derive(Debug)]
pub struct SlowWriter {
    path: PathBuf,
    file: File,
    cfg: SlowDiskConfig,
}

#[derive(Debug, Default)]
pub struct SlowWriterBuilder {
    file: Option<File>,
    path: Option<PathBuf>,
    cfg: SlowDiskConfig,
}

#[derive(Debug, Snafu)]
pub enum SlowWriterBuilderError {
    #[snafu(display("validation failed: {message}"))]
    Validation { message: String },
    #[snafu(transparent)]
    RangeWriter { source: FileWriterError },
}

impl SlowWriterBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Provide an already-opened file (preferred; consistent with Pool/Mmap builder style).
    pub fn file(mut self, file: File) -> Self {
        self.file = Some(file);
        self
    }

    /// Provide a path (for error messages only; optional if you pass `file`).
    pub fn path(mut self, path: PathBuf) -> Self {
        self.path = Some(path);
        self
    }

    /// Configure artificial latency.
    pub fn config(mut self, cfg: SlowDiskConfig) -> Self {
        self.cfg = cfg;
        self
    }

    pub async fn build(self) -> Result<SlowWriter, SlowWriterBuilderError> {
        let file = self
            .file
            .ok_or_else(|| SlowWriterBuilderError::Validation {
                message: "file must be set".to_string(),
            })?;

        Ok(SlowWriter {
            path: self.path.unwrap_or_else(|| PathBuf::from("<unknown>")),
            file,
            cfg: self.cfg,
        })
    }
}

impl SlowWriter {
    async fn inject_delay(&self, seed: u64) {
        let jitter = deterministic_jitter(self.cfg.max_jitter, seed);
        let d = self.cfg.latency + jitter;
        if !d.is_zero() {
            // Use tokio sleep in async context
            tokio::time::sleep(d).await;
        } else {
            // be nice to scheduler if latency is 0
            yield_now().await;
        }
    }
}

impl FileRangeWriter for SlowWriter {
    async fn write_range(&self, range: Range<u64>, data: Bytes) -> Result<(), FileWriterError> {
        self.inject_delay(range.start).await;

        // NOTE: We are doing blocking IO in a blocking section to keep async runtime healthy.
        let mut file = self
            .file
            .try_clone()
            .map_err(CommandError::from)
            .with_context(|_| WriteRangeSnafu {
                chunk: None,
                path: self.path.clone(),
            })?;

        let path = self.path.clone();
        blocking::unblock(move || {
            file.seek(SeekFrom::Start(range.start))
                .map_err(CommandError::from)
                .with_context(|_| WriteRangeSnafu {
                    chunk: None,
                    path: path.clone(),
                })?;

            file.write_all(&data)
                .map_err(CommandError::from)
                .with_context(|_| WriteRangeSnafu { chunk: None, path })?;

            Ok::<_, FileWriterError>(())
        })
        .await
    }

    async fn finalize(self) -> Result<(), FileWriterError> {
        self.inject_delay(0).await;

        let path = self.path.clone();
        blocking::unblock(move || {
            self.file
                .sync_all()
                .map_err(CommandError::from)
                .with_context(|_| FinalizeSnafu { path })?;
            Ok::<_, FileWriterError>(())
        })
        .await
    }
}
