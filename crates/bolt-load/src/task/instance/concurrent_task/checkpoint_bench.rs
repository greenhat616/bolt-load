//! The cost of checkpoints: a local download with and without them (design
//! §14.28).
//!
//! Checkpoints are crate-internal until a resumable task is public (C20),
//! so the public C13 bench cannot turn them on; this ignored test drives the
//! engine directly instead. Run it alone, in release:
//!
//! ```text
//! cargo test -p bolt-load --release --lib -- --ignored --nocapture checkpoint_overhead
//! ```
//!
//! `CHECKPOINT_BENCH_RUNS` sets the runs per group (5),
//! `CHECKPOINT_BENCH_SCENARIOS` a comma-separated list of scenarios (all):
//! `local` (256 MiB, unthrottled), `100MiB/s` (256 MiB) and `20MiB/s`
//! (64 MiB).

use std::{
    io::{Seek, SeekFrom, Write},
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use bolt_load_tests::adapter::simple::SimpleTestAdapterBuilder;
use smol_cancellation_token::CancellationToken;

use super::{
    ConcurrentTaskInner,
    checkpoint::{CheckpointBlock, CheckpointReceiver},
};
use crate::{
    adapter::DynBoltLoadAdapter,
    runtime::ThreadedRuntimeImpl,
    task::{
        TaskOptions,
        instance::{output_file::OutputFile, ownership::Ownership},
        status::ProgressReporter,
    },
};

/// Appends a 24-byte record per block to a file and syncs it, as the state
/// file will (design §11.5.5).
struct FileReceiver {
    file: std::fs::File,
    end: u64,
    records: Arc<AtomicUsize>,
    checkpoints: Arc<AtomicUsize>,
}

impl CheckpointReceiver for FileReceiver {
    fn record(&mut self, blocks: &[CheckpointBlock]) -> std::io::Result<()> {
        let mut bytes = Vec::with_capacity(blocks.len() * 24);
        for block in blocks {
            bytes.extend_from_slice(&block.index.to_le_bytes());
            bytes.extend_from_slice(&block.len.to_le_bytes());
            bytes.extend_from_slice(&block.crc.to_le_bytes());
            bytes.extend_from_slice(&[1, 0, 0, 0]);
            let crc = crc32c::crc32c(&bytes[bytes.len() - 20..]);
            bytes.extend_from_slice(&crc.to_le_bytes());
        }
        self.file.seek(SeekFrom::Start(self.end))?;
        self.file.write_all(&bytes)?;
        self.file.sync_all()?;
        self.end += bytes.len() as u64;
        self.records.fetch_add(blocks.len(), Ordering::Relaxed);
        self.checkpoints.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn roll_back(&mut self) -> std::io::Result<()> {
        self.file.set_len(self.end)?;
        self.file.sync_all()
    }
}

/// The groups of the benchmark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    /// No checkpoints: no CRC, no sync before the final one.
    Off,
    /// CRCs of every write, and one checkpoint, at teardown.
    Teardown,
    /// The default schedule: every 8 MiB or 2 s.
    On,
}

/// A source: its name, its total rate in bytes per second (`None`: as fast
/// as it goes), and the file size in MiB.
#[derive(Debug, Clone, Copy)]
struct Scenario {
    name: &'static str,
    rate: Option<u64>,
    mib: u64,
}

const SCENARIOS: [Scenario; 3] = [
    Scenario {
        name: "local",
        rate: None,
        mib: 256,
    },
    Scenario {
        name: "100MiB/s",
        rate: Some(100 << 20),
        mib: 256,
    },
    Scenario {
        name: "20MiB/s",
        rate: Some(20 << 20),
        mib: 64,
    },
];

/// What one run measured.
struct Run {
    elapsed: Duration,
    checkpoints: usize,
    records: usize,
    /// How long each sync of a checkpoint took: the writer's stall, for the
    /// mmap writer, whose one worker does not write meanwhile.
    syncs: Vec<Duration>,
}

/// One download of `scenario` into a temporary directory, in `group`.
async fn download(scenario: Scenario, group: Group) -> Run {
    let total = scenario.mib << 20;
    let mut adapter = SimpleTestAdapterBuilder::new()
        .content_size(total as usize)
        .chunk_size(32 << 10)
        .support_range(true);
    if let Some(rate) = scenario.rate {
        // As the C13 bench: a burst well over a timer tick (§14.17).
        adapter = adapter
            .max_speed(rate)
            .speed_burst(Duration::from_millis(62));
    }
    let adapter = adapter.build().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let output = OutputFile::new(dir.path().join("file.bin"), Ownership::new());
    let tmp_path = output.tmp.clone();
    let (reporter, _status) = ProgressReporter::for_test();
    let mut options = TaskOptions::default().max_connections(NonZeroUsize::new(8).unwrap());
    if group == Group::Teardown {
        options = options
            .checkpoint_bytes(std::num::NonZeroU64::new(u64::MAX).unwrap())
            .checkpoint_interval(Duration::from_secs(3600));
    }
    let mut inner = ConcurrentTaskInner::new(
        DynBoltLoadAdapter::new_arc(adapter),
        output,
        total,
        ThreadedRuntimeImpl::new_tokio_rt(),
        options,
        reporter,
    );
    let records = Arc::new(AtomicUsize::new(0));
    let checkpoints = Arc::new(AtomicUsize::new(0));
    if group != Group::Off {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(dir.path().join("file.bin.boltload"))
            .unwrap();
        inner = inner.checkpoint_to(Box::new(FileReceiver {
            file,
            end: 0,
            records: records.clone(),
            checkpoints: checkpoints.clone(),
        }));
    }
    let sync_times = inner.sync_times.clone();
    let started = Instant::now();
    inner
        .download_to(&tmp_path, &CancellationToken::new())
        .await
        .unwrap();
    let elapsed = started.elapsed();
    let syncs = sync_times.lock().unwrap().clone();
    Run {
        elapsed,
        checkpoints: checkpoints.load(Ordering::Relaxed),
        records: records.load(Ordering::Relaxed),
        syncs,
    }
}

fn median_p90(mut values: Vec<f64>) -> (f64, f64) {
    values.sort_by(f64::total_cmp);
    let rank = |q: f64| values[((q * values.len() as f64).ceil() as usize).max(1) - 1];
    (rank(0.5), rank(0.9))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "a benchmark: run it alone, in release"]
async fn checkpoint_overhead() {
    let runs: usize = std::env::var("CHECKPOINT_BENCH_RUNS")
        .map_or(5, |runs| runs.parse().expect("a number of runs"));
    let names: Option<Vec<String>> = std::env::var("CHECKPOINT_BENCH_SCENARIOS")
        .ok()
        .map(|names| names.split(',').map(str::to_owned).collect());

    // The CRC alone, over 256 MiB, from memory.
    let data = vec![0x5au8; 256 << 20];
    let started = Instant::now();
    let crc = crc32c::crc32c(&data);
    let crc_time = started.elapsed();
    drop(data);
    println!(
        "crc32c of 256 MiB: {:.1} ms, {:.2} GiB/s (crc {crc:08x})",
        crc_time.as_secs_f64() * 1e3,
        f64::from(256) / 1024.0 / crc_time.as_secs_f64()
    );

    let groups = [Group::Off, Group::Teardown, Group::On];
    for scenario in SCENARIOS {
        if names
            .as_ref()
            .is_some_and(|names| !names.iter().any(|name| name == scenario.name))
        {
            continue;
        }
        // A warm-up run of each, not counted.
        for group in groups {
            download(scenario, group).await;
        }
        // The groups alternate, so that drift on the machine hits them alike.
        let mut results: Vec<Vec<Run>> = groups.iter().map(|_| Vec::new()).collect();
        for _ in 0..runs {
            for (i, &group) in groups.iter().enumerate() {
                results[i].push(download(scenario, group).await);
            }
        }
        println!(
            "scenario {}: {} MiB, {runs} runs per group, 8 connections, block 1 MiB",
            scenario.name, scenario.mib
        );
        let seconds = |runs: &[Run]| -> Vec<f64> {
            runs.iter().map(|run| run.elapsed.as_secs_f64()).collect()
        };
        let (off_median, _) = median_p90(seconds(&results[0]));
        for (group, runs) in groups.iter().zip(&results) {
            let (median, p90) = median_p90(seconds(runs));
            let syncs: Vec<f64> = runs
                .iter()
                .flat_map(|run| run.syncs.iter().map(|sync| sync.as_secs_f64() * 1e3))
                .collect();
            let per_run: Vec<f64> = runs
                .iter()
                .map(|run| run.syncs.iter().map(Duration::as_secs_f64).sum::<f64>() * 1e3)
                .collect();
            let sync_line = if syncs.is_empty() {
                String::from("no checkpoint syncs")
            } else {
                let (sync_median, sync_p90) = median_p90(syncs.clone());
                let max = syncs.iter().copied().fold(0.0, f64::max);
                let (total_median, _) = median_p90(per_run);
                format!(
                    "sync median {sync_median:.1} ms, p90 {sync_p90:.1} ms, max {max:.1} ms, \
                     total per run (median) {total_median:.0} ms"
                )
            };
            println!(
                "  {group:?}: median {median:.3} s ({:.0} MiB/s), p90 {p90:.3} s, {:+.1}% over \
                 Off; checkpoints: {:?}; {sync_line}; runs: {:?}",
                scenario.mib as f64 / median,
                (median / off_median - 1.0) * 100.0,
                runs.iter().map(|run| run.checkpoints).collect::<Vec<_>>(),
                seconds(runs)
            );
            if *group != Group::Off {
                assert!(runs.iter().all(|run| run.records as u64 == scenario.mib));
            }
        }
    }
}
