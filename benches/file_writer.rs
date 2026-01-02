//! Benchmark for comparing different FileWriter implementations.
//!
//! This benchmark includes two types of tests:
//! 1. **Framework overhead tests** - Using NULL device (NUL on Windows, /dev/null on Unix)
//!    to measure pure framework overhead without filesystem I/O interference.
//! 2. **Real I/O tests** - Using actual temp files to measure real-world performance.
//!
//! Run with: `cargo bench --bench file_writer`
//! With compio: `cargo bench --bench file_writer --features compio`

use std::{path::Path, sync::Arc};

#[cfg(feature = "compio")]
use bolt_load::task::instance::concurrent_task::file_writer::CompioWriterBuilder;
// Import file writer types for benchmarking
use bolt_load::task::instance::concurrent_task::file_writer::{
    FileRangeWriter, FileRangeWriterKind, FileWriter, NullWriterBuilder, PoolWriterBuilder,
};
use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tempfile::TempDir;
use tokio::runtime::Runtime;

/// Get the NULL device path for the current platform
fn null_device_path() -> &'static Path {
    #[cfg(windows)]
    {
        Path::new(r"\\.\NUL")
    }
    #[cfg(unix)]
    {
        Path::new("/dev/null")
    }
}

/// Benchmark configuration
struct BenchConfig {
    /// Total file size in bytes
    file_size: u64,
    /// Size of each write chunk
    chunk_size: usize,
    /// Number of concurrent writers
    concurrency: usize,
}

impl BenchConfig {
    fn new(file_size: u64, chunk_size: usize, concurrency: usize) -> Self {
        Self {
            file_size,
            chunk_size,
            concurrency,
        }
    }

    fn chunks(&self) -> Vec<(u64, Bytes)> {
        let data = Bytes::from(vec![0xABu8; self.chunk_size]);
        let num_chunks = (self.file_size as usize) / self.chunk_size;

        (0..num_chunks)
            .map(|i| {
                let offset = (i * self.chunk_size) as u64;
                (offset, data.clone())
            })
            .collect()
    }
}

/// Run benchmark for a specific writer kind (using real files)
async fn bench_writer_sequential(
    temp_dir: &TempDir,
    kind: FileRangeWriterKind,
    config: &BenchConfig,
) {
    let file_path = temp_dir.path().join(format!("bench_{:?}.bin", kind));

    let writer = FileWriter::new_with_kind(&file_path, config.file_size, kind)
        .await
        .expect("Failed to create writer");

    let chunks = config.chunks();

    for (offset, data) in chunks {
        let end = offset + data.len() as u64;
        writer
            .write_range(offset..end, data)
            .await
            .expect("Write failed");
    }

    writer.finalize().await.expect("Finalize failed");

    let _ = std::fs::remove_file(&file_path);
}

/// Run benchmark for NullWriter (pure framework overhead)
async fn bench_null_writer_impl(config: &BenchConfig) {
    let writer = NullWriterBuilder::new()
        .build()
        .await
        .expect("Failed to create null writer");

    let chunks = config.chunks();

    for (offset, data) in chunks {
        let end = offset + data.len() as u64;
        writer
            .write_range(offset..end, data)
            .await
            .expect("Write failed");
    }

    writer.finalize().await.expect("Finalize failed");
}

/// Run benchmark for PoolWriter using NULL device (no filesystem overhead)
async fn bench_pool_null_device(config: &BenchConfig) {
    let null_path = null_device_path();

    // Open the NULL device
    let file = fs_err::OpenOptions::new()
        .write(true)
        .open(null_path)
        .expect("Failed to open NULL device");

    let writer = PoolWriterBuilder::new()
        .file(file)
        .build()
        .expect("Failed to create pool writer");

    let chunks = config.chunks();

    for (offset, data) in chunks {
        let end = offset + data.len() as u64;
        writer
            .write_range(offset..end, data)
            .await
            .expect("Write failed");
    }

    // Note: finalize() calls sync_all() which is not supported on NUL device
    // We ignore the error here since we're only measuring write performance
    let _ = writer.finalize().await;
}

/// Run benchmark for CompioWriter using NULL device (no filesystem overhead)
#[cfg(feature = "compio")]
async fn bench_compio_null_device(config: &BenchConfig) {
    let null_path = null_device_path();

    let writer = CompioWriterBuilder::new()
        .path(null_path.to_path_buf())
        .build()
        .await
        .expect("Failed to create compio writer");

    let chunks = config.chunks();

    for (offset, data) in chunks {
        let end = offset + data.len() as u64;
        writer
            .write_range(offset..end, data)
            .await
            .expect("Write failed");
    }

    // Note: finalize() calls sync_all() which is not supported on NUL device
    // We ignore the error here since we're only measuring write performance
    let _ = writer.finalize().await;
}

/// Run benchmark with concurrent writes
async fn bench_writer_concurrent(
    temp_dir: &TempDir,
    kind: FileRangeWriterKind,
    config: &BenchConfig,
) {
    let file_path = temp_dir
        .path()
        .join(format!("bench_{:?}_concurrent.bin", kind));

    let writer = FileWriter::new_with_kind(&file_path, config.file_size, kind)
        .await
        .expect("Failed to create writer");

    let writer = Arc::new(writer);
    let chunks = config.chunks();

    let chunk_groups: Vec<_> = chunks
        .chunks(chunks.len() / config.concurrency.max(1))
        .map(|c| c.to_vec())
        .collect();

    let handles: Vec<_> = chunk_groups
        .into_iter()
        .map(|group| {
            let writer = writer.clone();
            tokio::spawn(async move {
                for (offset, data) in group {
                    let end = offset + data.len() as u64;
                    writer
                        .write_range(offset..end, data)
                        .await
                        .expect("Write failed");
                }
            })
        })
        .collect();

    for handle in handles {
        handle.await.expect("Task failed");
    }

    drop(writer);
    let _ = std::fs::remove_file(&file_path);
}

// =============================================================================
// Framework Overhead Benchmarks (using NULL device - no filesystem interference)
// =============================================================================

/// Benchmark pure framework overhead using NullWriter
fn bench_framework_null_writer(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("framework_overhead/null_writer");

    for chunk_size in [4 * 1024, 64 * 1024, 256 * 1024, 1024 * 1024] {
        let file_size = 64 * 1024 * 1024; // 64 MB
        let config = BenchConfig::new(file_size, chunk_size, 1);

        group.throughput(Throughput::Bytes(file_size));
        group.bench_with_input(
            BenchmarkId::new("chunk", format!("{}KB", chunk_size / 1024)),
            &config,
            |b, config| {
                b.to_async(&rt)
                    .iter(|| async { bench_null_writer_impl(config).await });
            },
        );
    }

    group.finish();
}

/// Benchmark PoolWriter with NULL device (measures framework + syscall overhead)
fn bench_framework_pool_null(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("framework_overhead/pool_null_device");

    for chunk_size in [4 * 1024, 64 * 1024, 256 * 1024, 1024 * 1024] {
        let file_size = 64 * 1024 * 1024; // 64 MB
        let config = BenchConfig::new(file_size, chunk_size, 1);

        group.throughput(Throughput::Bytes(file_size));
        group.bench_with_input(
            BenchmarkId::new("chunk", format!("{}KB", chunk_size / 1024)),
            &config,
            |b, config| {
                b.to_async(&rt)
                    .iter(|| async { bench_pool_null_device(config).await });
            },
        );
    }

    group.finish();
}

/// Benchmark CompioWriter with NULL device (measures framework + io_uring overhead)
#[cfg(feature = "compio")]
fn bench_framework_compio_null(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("framework_overhead/compio_null_device");

    for chunk_size in [4 * 1024, 64 * 1024, 256 * 1024, 1024 * 1024] {
        let file_size = 64 * 1024 * 1024; // 64 MB
        let config = BenchConfig::new(file_size, chunk_size, 1);

        group.throughput(Throughput::Bytes(file_size));
        group.bench_with_input(
            BenchmarkId::new("chunk", format!("{}KB", chunk_size / 1024)),
            &config,
            |b, config| {
                b.to_async(&rt)
                    .iter(|| async { bench_compio_null_device(config).await });
            },
        );
    }

    group.finish();
}

/// Compare framework overhead across different writer implementations
fn bench_framework_comparison(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("framework_overhead_comparison");

    let file_size = 64 * 1024 * 1024; // 64 MB
    let chunk_size = 64 * 1024; // 64 KB
    let config = BenchConfig::new(file_size, chunk_size, 1);

    group.throughput(Throughput::Bytes(file_size));

    // NullWriter - pure framework overhead (no syscalls)
    group.bench_with_input(
        BenchmarkId::new("writer", "NullWriter"),
        &config,
        |b, config| {
            b.to_async(&rt)
                .iter(|| async { bench_null_writer_impl(config).await });
        },
    );

    // PoolWriter with NULL device - framework + syscall overhead
    group.bench_with_input(
        BenchmarkId::new("writer", "Pool+NUL"),
        &config,
        |b, config| {
            b.to_async(&rt)
                .iter(|| async { bench_pool_null_device(config).await });
        },
    );

    // CompioWriter with NULL device - framework + io_uring overhead
    #[cfg(feature = "compio")]
    group.bench_with_input(
        BenchmarkId::new("writer", "Compio+NUL"),
        &config,
        |b, config| {
            b.to_async(&rt)
                .iter(|| async { bench_compio_null_device(config).await });
        },
    );

    group.finish();
}

// =============================================================================
// Real I/O Benchmarks (using actual filesystem)
// =============================================================================

fn bench_real_io_mmap(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let temp_dir = TempDir::new().unwrap();

    let mut group = c.benchmark_group("real_io/mmap_writer");

    for chunk_size in [4 * 1024, 64 * 1024, 256 * 1024, 1024 * 1024] {
        let file_size = 64 * 1024 * 1024; // 64 MB
        let config = BenchConfig::new(file_size, chunk_size, 1);

        group.throughput(Throughput::Bytes(file_size));
        group.bench_with_input(
            BenchmarkId::new("chunk", format!("{}KB", chunk_size / 1024)),
            &config,
            |b, config| {
                b.to_async(&rt).iter(|| async {
                    bench_writer_sequential(&temp_dir, FileRangeWriterKind::Mmap, config).await
                });
            },
        );
    }

    group.finish();
}

fn bench_real_io_pool(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let temp_dir = TempDir::new().unwrap();

    let mut group = c.benchmark_group("real_io/pool_writer");

    for chunk_size in [4 * 1024, 64 * 1024, 256 * 1024, 1024 * 1024] {
        let file_size = 64 * 1024 * 1024; // 64 MB
        let config = BenchConfig::new(file_size, chunk_size, 1);

        group.throughput(Throughput::Bytes(file_size));
        group.bench_with_input(
            BenchmarkId::new("chunk", format!("{}KB", chunk_size / 1024)),
            &config,
            |b, config| {
                b.to_async(&rt).iter(|| async {
                    bench_writer_sequential(&temp_dir, FileRangeWriterKind::Pool, config).await
                });
            },
        );
    }

    group.finish();
}

#[cfg(feature = "compio")]
fn bench_real_io_compio(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let temp_dir = TempDir::new().unwrap();

    let mut group = c.benchmark_group("real_io/compio_writer");

    for chunk_size in [4 * 1024, 64 * 1024, 256 * 1024, 1024 * 1024] {
        let file_size = 64 * 1024 * 1024; // 64 MB
        let config = BenchConfig::new(file_size, chunk_size, 1);

        group.throughput(Throughput::Bytes(file_size));
        group.bench_with_input(
            BenchmarkId::new("chunk", format!("{}KB", chunk_size / 1024)),
            &config,
            |b, config| {
                b.to_async(&rt).iter(|| async {
                    bench_writer_sequential(&temp_dir, FileRangeWriterKind::Compio, config).await
                });
            },
        );
    }

    group.finish();
}

/// Compare real I/O performance across different writer implementations
fn bench_real_io_comparison(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let temp_dir = TempDir::new().unwrap();

    let mut group = c.benchmark_group("real_io_comparison");

    let file_size = 64 * 1024 * 1024; // 64 MB
    let chunk_size = 64 * 1024; // 64 KB
    let config = BenchConfig::new(file_size, chunk_size, 1);

    group.throughput(Throughput::Bytes(file_size));

    group.bench_with_input(BenchmarkId::new("writer", "Mmap"), &config, |b, config| {
        b.to_async(&rt).iter(|| async {
            bench_writer_sequential(&temp_dir, FileRangeWriterKind::Mmap, config).await
        });
    });

    group.bench_with_input(BenchmarkId::new("writer", "Pool"), &config, |b, config| {
        b.to_async(&rt).iter(|| async {
            bench_writer_sequential(&temp_dir, FileRangeWriterKind::Pool, config).await
        });
    });

    #[cfg(feature = "compio")]
    group.bench_with_input(
        BenchmarkId::new("writer", "Compio"),
        &config,
        |b, config| {
            b.to_async(&rt).iter(|| async {
                bench_writer_sequential(&temp_dir, FileRangeWriterKind::Compio, config).await
            });
        },
    );

    group.finish();
}

// =============================================================================
// Concurrent Write Benchmarks
// =============================================================================

fn bench_concurrent_null_writer(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let temp_dir = TempDir::new().unwrap();

    let mut group = c.benchmark_group("concurrent/null_writer");

    for concurrency in [1, 2, 4, 8] {
        let file_size = 64 * 1024 * 1024; // 64 MB
        let chunk_size = 64 * 1024; // 64 KB
        let config = BenchConfig::new(file_size, chunk_size, concurrency);

        group.throughput(Throughput::Bytes(file_size));
        group.bench_with_input(
            BenchmarkId::new("threads", concurrency),
            &config,
            |b, config| {
                b.to_async(&rt).iter(|| async {
                    bench_writer_concurrent(&temp_dir, FileRangeWriterKind::Null, config).await
                });
            },
        );
    }

    group.finish();
}

// =============================================================================
// Criterion Groups
// =============================================================================

#[cfg(not(feature = "compio"))]
criterion_group!(
    benches,
    // Framework overhead (NULL device)
    bench_framework_null_writer,
    bench_framework_pool_null,
    bench_framework_comparison,
    // Real I/O
    bench_real_io_mmap,
    bench_real_io_pool,
    bench_real_io_comparison,
    // Concurrent
    bench_concurrent_null_writer,
);

#[cfg(feature = "compio")]
criterion_group!(
    benches,
    // Framework overhead (NULL device)
    bench_framework_null_writer,
    bench_framework_pool_null,
    bench_framework_compio_null,
    bench_framework_comparison,
    // Real I/O
    bench_real_io_mmap,
    bench_real_io_pool,
    bench_real_io_compio,
    bench_real_io_comparison,
    // Concurrent
    bench_concurrent_null_writer,
);

criterion_main!(benches);
