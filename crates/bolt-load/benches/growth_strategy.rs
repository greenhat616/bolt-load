//! Growth-strategy benchmark (T14, design §14.17): whole downloads under
//! the doubling growth strategy, the only one since C14 (design §14.20),
//! against the in-process `SimpleTestAdapter` of `bolt-load-tests`. Its
//! comparison with the strategy before it is in design §14.19.
//!
//! Scenarios (sizes and why: design §14.17):
//! - `s1_max32`: every connection at 1 MiB/s, 16 MiB/s in all, up to 32
//!   connections, so that the total is what limits the growth.
//! - `s1_max8`: the same with the default limit of 8 connections.
//! - `s2_unlimited`: no limit at all.
//! - `s3_429_beyond_4`: 2 MiB/s per connection; a fifth concurrent connection
//!   gets a 429 without `Retry-After`.
//! - `s4_endgame_stall`: 2 MiB/s per connection; the first connection to
//!   reach 1 MiB before the end stops sending without closing (T27).
//! - `s4a_stall_while_splittable`: M1's first case. The first 16 MiB come at
//!   512 KiB/s per connection, the rest at 4 MiB/s; the first connection to
//!   reach 1 MiB before the end stalls while slow chunks are still large.
//! - `s4b_two_runners`: M1's second case. Two connections at most, 2 MiB/s
//!   each; the first to reach 4 MiB before the end stalls.
//! - `s4c_stall_midway`: `s4_endgame_stall`, but the stall is at 40 MiB,
//!   with more than `2 × min_chunk_size` left, among more than two runners:
//!   the case of the slow restart (T13).
//! - `s1_max32_long`: `s1_max32` on 256 MiB, long enough for the doubling
//!   from 16 to 32 to be judged (design §14.18, §14.33).
//! - `s6_plateau_then_more_bandwidth`: 1 MiB/s per connection, 2 MiB/s in
//!   all for the first 20 s, then 8 MiB/s in all: the doubling to four
//!   gains nothing and holds two, the bandwidth grows during the hold, and
//!   the probe 30 s later starts from the four still running (design
//!   §14.33, D31).
//!
//! `s4_nostall`, `s4a_nostall` and `s4b_nostall` run those setups without
//! the stall, as the baseline of what it costs; `s4_nostall` is also the
//! baseline of `s4c_stall_midway`.
//!
//! Recorded per run: completion time (from `spawn`), peak open connections
//! and 429s (from the adapter), hedges, decided pairs, the losers' cancel
//! latency (from the decision to the loser's `Stopped`), restarts and the
//! stops the silent restart requested, the holds a gainless doubling or
//! probe began and the probes of a hold
//! (from the crate's tracing events), the duplicate bytes (bytes the
//! adapter sent beyond the file size) and, with a stall, how long the
//! stalled offset waits until a connection requests it again.
//!
//! `s5_silent_limit_4` runs over HTTP instead, with the reqwest adapter
//! against the limited server of `bolt-load-tests`
//! (`create_limited_range_server`): 1 MiB/s per connection, at most four
//! served at once, the others closed at accept (`Close`) or never
//! answered (`Hang`, which the default 30 s connect timeout ends). Its own
//! table records the completion time, the most connections the server
//! served at once, the connections it turned away, when it turned away the
//! first and the last, and the speed.
//!
//! The probe reads the crate's messages as text. It checks at start that
//! the crate's source still has each message it reads and that its parser
//! reads samples of them right. It fails a stall run whose stall did not
//! happen or was never retried, and a run where the strategy should have
//! hedged but the probe saw no hedge, decision or loser end. Decisions and
//! loser ends it cannot pair are reported as `unmatched`.
//!
//! Run with:
//! `cargo bench -p bolt-load --bench growth_strategy`
//!
//! `GROWTH_RUNS` sets the runs per scenario (10), `GROWTH_SCENARIOS` a
//! comma-separated list of scenario names (all). `GROWTH_TRACE` also prints the
//! trace events of the download loop and the strategies of every run. Without
//! `--bench` (as under `cargo test --benches`) nothing runs.

use std::{
    fmt::Debug,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use bolt_load::{runtime::ThreadedRuntimeImpl, task::Task};
use bolt_load_adapter::reqwest::IntoReqwestAdapter;
use bolt_load_tests::adapter::{
    fault::LimitMode,
    http_server::create_limited_range_server,
    simple::{SimpleTestAdapter, SimpleTestAdapterBuilder, create_deterministic_content},
};
use tracing::{Event, Level, Subscriber, field::Field};
use tracing_subscriber::{
    Layer,
    field::Visit,
    filter::Targets,
    layer::{Context, SubscriberExt},
};

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
/// The adapter's item size; a runner frame is 32 KiB.
const ITEM: usize = 16 * 1024;
/// How long the adapter's limits may burst for: short, so that a limit
/// holds from the first second, but a few periods of the Windows timer
/// (15.6 ms), which would cap a shorter one.
const BURST: Duration = Duration::from_millis(62);

/// A download setup.
#[derive(Clone)]
struct Scenario {
    name: &'static str,
    size: u64,
    max_connections: usize,
    /// The adapter, before the stall.
    adapter: fn(u64) -> SimpleTestAdapterBuilder,
    /// The offset the first connection to reach it stalls at.
    stall_at: Option<u64>,
    /// The strategy hedges the stall: every run must show the hedge, its
    /// decision and its loser's end, or the probe no longer reads them.
    hedged: bool,
}

fn limited(size: u64) -> SimpleTestAdapterBuilder {
    SimpleTestAdapterBuilder::new()
        .content_size(size as usize)
        .support_range(true)
        .chunk_size(ITEM)
        .speed_burst(BURST)
}

fn scenarios() -> Vec<Scenario> {
    let s1 = |size| limited(size).max_per_stream_speed(MIB).max_speed(16 * MIB);
    let s4 = |size| limited(size).max_per_stream_speed(2 * MIB);
    let s4a = |size| {
        limited(size)
            .max_per_stream_speed(4 * MIB)
            .paced_region(0..16 * MIB, 512 * KIB)
    };
    let mut scenarios = vec![
        Scenario {
            name: "s1_max32",
            size: 128 * MIB,
            max_connections: 32,
            adapter: s1,
            stall_at: None,
            hedged: false,
            // 16 MiB/s in all at 1 MiB/s a connection.
        },
        Scenario {
            name: "s1_max8",
            size: 128 * MIB,
            max_connections: 8,
            adapter: s1,
            stall_at: None,
            hedged: false,
            // 16 MiB/s in all at 1 MiB/s a connection.
        },
        Scenario {
            name: "s2_unlimited",
            size: 4096 * MIB,
            max_connections: 8,
            adapter: |size| {
                SimpleTestAdapterBuilder::new()
                    .content_size(size as usize)
                    .support_range(true)
                    .chunk_size(64 * 1024)
            },
            stall_at: None,
            hedged: false,
            // Observed, not by construction: without a limit, two
            // connections were no faster than one on this machine.
        },
        Scenario {
            name: "s3_429_beyond_4",
            size: 96 * MIB,
            max_connections: 8,
            adapter: |size| {
                limited(size)
                    .max_per_stream_speed(2 * MIB)
                    .max_concurrent_streams(4)
            },
            stall_at: None,
            hedged: false,
            // A fifth concurrent connection gets a 429.
        },
        Scenario {
            name: "s4_endgame_stall",
            size: 64 * MIB,
            max_connections: 8,
            adapter: s4,
            stall_at: Some(63 * MIB),
            hedged: true,
        },
        Scenario {
            name: "s4a_stall_while_splittable",
            size: 32 * MIB,
            max_connections: 8,
            adapter: s4a,
            stall_at: Some(31 * MIB),
            hedged: true,
        },
        Scenario {
            name: "s4b_two_runners",
            size: 32 * MIB,
            max_connections: 2,
            adapter: |size| limited(size).max_per_stream_speed(2 * MIB),
            stall_at: Some(28 * MIB),
            hedged: false,
        },
        Scenario {
            name: "s4c_stall_midway",
            size: 64 * MIB,
            max_connections: 8,
            adapter: s4,
            stall_at: Some(40 * MIB),
            hedged: false,
        },
        Scenario {
            name: "s1_max32_long",
            size: 256 * MIB,
            max_connections: 32,
            adapter: s1,
            stall_at: None,
            hedged: false,
        },
        Scenario {
            name: "s6_plateau_then_more_bandwidth",
            size: 160 * MIB,
            max_connections: 8,
            adapter: |size| {
                limited(size)
                    .max_per_stream_speed(MIB)
                    .max_speed(2 * MIB)
                    .raise_max_speed(Duration::from_secs(20), 8 * MIB)
            },
            stall_at: None,
            hedged: false,
        },
    ];
    let baselines: Vec<_> = scenarios
        .iter()
        .filter_map(|scenario| {
            let name = match scenario.name {
                "s4_endgame_stall" => "s4_nostall",
                "s4a_stall_while_splittable" => "s4a_nostall",
                "s4b_two_runners" => "s4b_nostall",
                // The same setup as `s4_endgame_stall`.
                _ => return None,
            };
            Some(Scenario {
                name,
                stall_at: None,
                hedged: false,
                ..scenario.clone()
            })
        })
        .collect();
    scenarios.extend(baselines);
    scenarios
}

// ─────────────────────────── tracing probe ───────────────────────────

/// The messages of the crate's hedge and runner-manager events, with when
/// they were emitted.
#[derive(Clone, Default)]
struct Probe(Arc<Mutex<Vec<(Instant, String)>>>);

impl Probe {
    fn take(&self) -> Vec<(Instant, String)> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

#[derive(Default)]
struct Message(String);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl<S: Subscriber> Layer<S> for Probe {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let at = Instant::now();
        let mut message = Message::default();
        event.record(&mut message);
        self.0.lock().unwrap().push((at, message.0));
    }
}

/// Whether `GROWTH_TRACE` asks to print the trace events of the download loop
/// and the strategies.
fn tracing_all() -> bool {
    std::env::var_os("GROWTH_TRACE").is_some()
}

fn install_probe() -> Probe {
    const CONCURRENT: &str = "bolt_load::task::instance::concurrent_task";
    let probe = Probe::default();
    // Only these modules: none of their events is on the per-frame path,
    // except the frames of a beaten runner.
    let mut targets = Targets::new()
        .with_target(format!("{CONCURRENT}::download_loop::hedge"), Level::DEBUG)
        .with_target(
            format!("{CONCURRENT}::download_loop::restart"),
            Level::DEBUG,
        )
        .with_target(format!("{CONCURRENT}::runner_manager"), Level::TRACE)
        .with_target(
            format!("{CONCURRENT}::strategy::doubling_strategy"),
            Level::DEBUG,
        );
    if tracing_all() {
        targets = targets
            .with_target(format!("{CONCURRENT}::strategy"), Level::TRACE)
            .with_target(format!("{CONCURRENT}::download_loop"), Level::TRACE);
    }
    let subscriber = tracing_subscriber::registry().with(probe.clone().with_filter(targets));
    tracing::subscriber::set_global_default(subscriber).expect("no other subscriber");
    probe
}

/// The runner id after `prefix` in `message`.
fn id_after(message: &str, prefix: &str) -> Option<u64> {
    let rest = &message[message.find(prefix)? + prefix.len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The messages the probe reads, and the source files that emit them: a
/// message renamed in the crate fails here instead of reading as zero.
const PROBED: [(&str, &str); 10] = [
    (DOUBLING_SOURCE, "before; hold {h}, probe again in {:?}\""),
    (
        DOUBLING_SOURCE,
        "\"[STRATEGY] probing the hold of {h} runners from {base} towards {goal}\"",
    ),
    (HEDGE_SOURCE, "\"[TASK] hedging runner {owner}"),
    (
        DATA_SOURCE,
        "\"hedge {runner_id} wins its pair over runner {original}\"",
    ),
    (
        DATA_SOURCE,
        "\"runner {original} wins its pair over hedge {runner_id}: a frame queued\"",
    ),
    (
        DATA_SOURCE,
        "\"runner {runner_id} wins its pair over hedge {}\"",
    ),
    (
        DATA_SOURCE,
        "\"dropping a frame of beaten runner {runner_id}\"",
    ),
    (LIFECYCLE_SOURCE, "\"beaten runner {runner_id} ended\""),
    (
        LIFECYCLE_SOURCE,
        "\"runner {runner_id} stopped for being slow, released range",
    ),
    (
        RESTART_SOURCE,
        "\"[TASK] stopping silent runner {runner_id}",
    ),
];
const HEDGE_SOURCE: &str =
    include_str!("../src/task/instance/concurrent_task/download_loop/hedge.rs");
const DATA_SOURCE: &str =
    include_str!("../src/task/instance/concurrent_task/runner_manager/data.rs");
const RESTART_SOURCE: &str =
    include_str!("../src/task/instance/concurrent_task/download_loop/restart.rs");
const LIFECYCLE_SOURCE: &str =
    include_str!("../src/task/instance/concurrent_task/runner_manager/lifecycle.rs");
const DOUBLING_SOURCE: &str =
    include_str!("../src/task/instance/concurrent_task/strategy/doubling_strategy.rs");

fn check_probed_messages() {
    for (source, message) in PROBED {
        assert!(
            source.contains(message),
            "the probe reads {message}, which the crate no longer emits"
        );
    }
}

/// The parser on the three decision messages, a late loser frame, a slow
/// restart, a loser end without a decision and a decision without one.
fn check_parser() {
    let t0 = Instant::now();
    let at = |ms| t0 + Duration::from_millis(ms);
    let events: Vec<(Instant, String)> = [
        (0, "[TASK] hedging runner 3, silent since Some(..)"),
        (10, "hedge 9 wins its pair over runner 3"),
        (11, "dropping a frame of beaten runner 3"),
        (13, "beaten runner 3 ended"),
        (20, "[TASK] hedging runner 4, silent since Some(..)"),
        (30, "runner 4 wins its pair over hedge 12: a frame queued"),
        (34, "beaten runner 12 ended"),
        (40, "[TASK] hedging runner 5, silent since Some(..)"),
        (50, "runner 5 wins its pair over hedge 13"),
        (60, "beaten runner 7 ended"),
        (68, "[TASK] stopping silent runner 6, silent since Some(..)"),
        (
            70,
            "runner 6 stopped for being slow, released range: Some(0..1)",
        ),
        (
            80,
            "[STRATEGY] doubling to 4 runners gained too little: 2 B/s after, 2 B/s before; hold \
             2, probe again in 30s",
        ),
        (
            90,
            "[STRATEGY] probing the hold of 2 runners from 4 towards 8",
        ),
    ]
    .into_iter()
    .map(|(ms, message)| (at(ms), message.to_owned()))
    .collect();
    let counts = count_events(&events);
    assert_eq!(
        (counts.hedges, counts.decisions, counts.late_loser_frames),
        (3, 3, 1)
    );
    assert_eq!((counts.slow_restarts, counts.silent_restarts), (1, 1));
    assert_eq!((counts.holds, counts.probes), (1, 1));
    assert_eq!(
        counts.cancel_latencies,
        [Duration::from_millis(3), Duration::from_millis(4)]
    );
    // Runner 7's end has no decision; runner 13's decision has no end.
    assert_eq!(counts.unmatched, 2);
}

#[derive(Default)]
struct EventCounts {
    hedges: usize,
    decisions: usize,
    /// Frames of a beaten runner dropped as they arrived. The frames still
    /// queued when it is stopped are dropped with its receiver, without an
    /// event, and are not counted.
    late_loser_frames: usize,
    /// Runners stopped and released by the slow or the silent restart.
    slow_restarts: usize,
    /// The stops the silent restart requested. Not a subset of
    /// `slow_restarts`: a runner asked to stop may finish or fail before
    /// it notices.
    silent_restarts: usize,
    /// Holds a gainless doubling or probe began.
    holds: usize,
    /// Probes of a hold.
    probes: usize,
    cancel_latencies: Vec<Duration>,
    /// Decisions without their loser's end, and loser ends without a
    /// decision.
    unmatched: usize,
}

fn count_events(events: &[(Instant, String)]) -> EventCounts {
    let mut counts = EventCounts::default();
    let mut decided: Vec<(Instant, u64)> = Vec::new();
    for (at, message) in events {
        if message.starts_with("[TASK] hedging runner") {
            counts.hedges += 1;
        } else if message.contains("wins its pair over") {
            counts.decisions += 1;
            // "hedge H wins … over runner O" or "runner O wins … over hedge H".
            let loser = if message.starts_with("hedge ") {
                id_after(message, "over runner ")
            } else {
                id_after(message, "over hedge ")
            };
            decided.push((*at, loser.expect("a decision names its loser")));
        } else if message.starts_with("beaten runner") && message.ends_with("ended") {
            let id = id_after(message, "beaten runner ").expect("an id");
            if let Some(index) = decided.iter().position(|&(_, loser)| loser == id) {
                let (decided_at, _) = decided.remove(index);
                counts.cancel_latencies.push(at.duration_since(decided_at));
            } else {
                counts.unmatched += 1;
            }
        } else if message.starts_with("dropping a frame of beaten runner") {
            counts.late_loser_frames += 1;
        } else if message.contains("stopped for being slow") {
            counts.slow_restarts += 1;
        } else if message.starts_with("[TASK] stopping silent runner") {
            counts.silent_restarts += 1;
        } else if message.starts_with("[STRATEGY] doubling to") && message.contains("; hold ") {
            counts.holds += 1;
        } else if message.starts_with("[STRATEGY] probing the hold") {
            counts.probes += 1;
        }
    }
    counts.unmatched += decided.len();
    counts
}

// ─────────────────────────────── runs ───────────────────────────────

struct Run {
    secs: f64,
    peak: usize,
    congested: usize,
    hedges: usize,
    decisions: usize,
    duplicate_bytes: u64,
    late_loser_frames: usize,
    slow_restarts: usize,
    silent_restarts: usize,
    holds: usize,
    probes: usize,
    cancel_latencies: Vec<Duration>,
    unmatched: usize,
    /// From the stall until a connection requests the stalled offset again.
    stall_wait: Option<Duration>,
}

async fn run_once(
    scenario: &Scenario,
    template: &SimpleTestAdapter,
    probe: &Probe,
    dir: &std::path::Path,
) -> Run {
    let adapter = template.clone_reset_count();
    let path = dir.join("download.bin");
    probe.take();
    let builder = Task::builder(adapter.clone())
        .runtime(ThreadedRuntimeImpl::new_tokio_rt())
        .save_to(&path)
        .max_connections(NonZeroUsize::new(scenario.max_connections).unwrap());
    // The task starts in `spawn`.
    let started = Instant::now();
    let task = builder.spawn().expect("a valid task");
    let output = task.await.expect("the download completes");
    let secs = started.elapsed().as_secs_f64();
    let events = probe.take();
    if tracing_all() {
        for (at, message) in &events {
            eprintln!(
                "{:>8.3} {message}",
                at.duration_since(started).as_secs_f64()
            );
        }
    }

    verify(&output.path, template.content(), scenario.name);
    std::fs::remove_file(&output.path).expect("remove the download");

    // Every stall happens, and the download only completes once the
    // stalled offset is requested again: a missing wait is a broken setup.
    let stall_wait = scenario.stall_at.map(|offset| {
        let stalled = adapter
            .stalled_at()
            .unwrap_or_else(|| panic!("{}: no connection stalled", scenario.name));
        adapter
            .streams()
            .iter()
            .find(|stream| {
                stream.at > stalled && stream.range.0 <= offset && offset < stream.range.1
            })
            .map(|stream| stream.at.duration_since(stalled))
            .unwrap_or_else(|| panic!("{}: the stalled offset was never retried", scenario.name))
    });
    let counts = count_events(&events);
    if scenario.hedged {
        assert!(
            counts.hedges >= 1
                && counts.decisions == counts.hedges
                && counts.cancel_latencies.len() == counts.decisions
                && counts.unmatched == 0,
            "{}: the probe read {} hedges, {} decisions, {} loser ends and {} unmatched events \
             where the stall must be hedged",
            scenario.name,
            counts.hedges,
            counts.decisions,
            counts.cancel_latencies.len(),
            counts.unmatched,
        );
    }
    Run {
        secs,
        peak: adapter.peak_streams(),
        congested: adapter.congested(),
        hedges: counts.hedges,
        decisions: counts.decisions,
        duplicate_bytes: adapter.served_bytes().saturating_sub(scenario.size),
        late_loser_frames: counts.late_loser_frames,
        slow_restarts: counts.slow_restarts,
        silent_restarts: counts.silent_restarts,
        holds: counts.holds,
        probes: counts.probes,
        cancel_latencies: counts.cancel_latencies,
        unmatched: counts.unmatched,
        stall_wait,
    }
}

/// Compare the file at `path` with `expected`, a piece at a time.
fn verify(path: &std::path::Path, expected: &[u8], name: &str) {
    use std::io::Read;

    let mut file = std::fs::File::open(path).expect("the downloaded file");
    assert_eq!(
        file.metadata().expect("its metadata").len(),
        expected.len() as u64,
        "{name}: the downloaded size differs"
    );
    let mut buffer = vec![0; 8 * MIB as usize];
    for piece in expected.chunks(buffer.len()) {
        let read = &mut buffer[..piece.len()];
        file.read_exact(read).expect("read the download");
        assert!(read == piece, "{name}: the downloaded content differs");
    }
}

// ───────────────────────────── statistics ─────────────────────────────

fn median(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

/// Nearest rank.
fn p90(sorted: &[f64]) -> f64 {
    let rank = (sorted.len() as f64 * 0.9).ceil() as usize;
    sorted[rank.max(1) - 1]
}

/// `median / p90 (min–max)` of `values`, or `n/a` without any.
fn summary(mut values: Vec<f64>, digits: usize) -> String {
    if values.is_empty() {
        return "n/a".into();
    }
    values.sort_by(f64::total_cmp);
    format!(
        "{:.digits$} / {:.digits$} ({:.digits$}–{:.digits$})",
        median(&values),
        p90(&values),
        values[0],
        values[values.len() - 1],
    )
}

/// [`summary`] with the number of samples, for a metric that a run may
/// have none or several of.
fn counted_summary(values: Vec<f64>, digits: usize) -> String {
    let n = values.len();
    format!("{}, n={n}", summary(values, digits))
}

fn report(scenario: &Scenario, runs: &[Run]) {
    let col = |f: &dyn Fn(&Run) -> f64, digits| summary(runs.iter().map(f).collect(), digits);
    let latencies: Vec<f64> = runs
        .iter()
        .flat_map(|run| run.cancel_latencies.iter().map(|d| d.as_secs_f64() * 1e3))
        .collect();
    let waits: Vec<f64> = runs
        .iter()
        .filter_map(|run| run.stall_wait.map(|d| d.as_secs_f64()))
        .collect();
    let waits = if scenario.stall_at.is_some() {
        counted_summary(waits, 2)
    } else {
        "n/a".into()
    };
    println!(
        "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
        scenario.name,
        col(&|run| run.secs, 2),
        // Counts with one decimal: the median of an even number of runs
        // may fall halfway.
        col(&|run| run.peak as f64, 1),
        col(&|run| run.congested as f64, 1),
        col(&|run| run.hedges as f64, 1),
        col(&|run| run.decisions as f64, 1),
        col(&|run| run.duplicate_bytes as f64 / KIB as f64, 0),
        counted_summary(latencies, 2),
        waits,
        col(&|run| run.slow_restarts as f64, 1),
        col(&|run| run.silent_restarts as f64, 1),
        col(&|run| run.holds as f64, 1),
        col(&|run| run.probes as f64, 1),
        runs.iter().map(|run| run.unmatched).sum::<usize>(),
        col(&|run| (scenario.size as f64 / MIB as f64) / run.secs, 1),
    );
}

// ─────────────────────── the silent limit over HTTP ───────────────────────

/// `s5_silent_limit_4`: a server that serves at most this many connections
/// at once, each at [`S5_RATE`].
const S5_LIMIT: usize = 4;
const S5_RATE: u64 = MIB;
/// About 50 s at four connections: long enough for the first recovery
/// step, 30 s after the limit is lowered.
const S5_SIZE: u64 = 192 * MIB;
/// How long a run may take, whatever the engine's own timeouts: a lost
/// wake-up fails the round instead of hanging the bench.
const S5_TIMEOUT: Duration = Duration::from_secs(300);

struct LimitedRun {
    secs: f64,
    max_served: usize,
    turned_away: usize,
    /// When the first and the last connection were turned away, from the
    /// start.
    first_turned_away: Option<f64>,
    last_turned_away: Option<f64>,
}

async fn run_limited(
    mode: LimitMode,
    content: &bytes::Bytes,
    probe: &Probe,
    dir: &std::path::Path,
) -> LimitedRun {
    let (port, server, log) = create_limited_range_server(content.clone(), S5_LIMIT, mode, S5_RATE)
        .await
        .expect("the limited server");
    let url = reqwest::Url::parse(&format!("http://127.0.0.1:{port}/file")).expect("a URL");
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("a client");
    let adapter = client.into_reqwest_adapter((reqwest::Method::GET, url));
    let path = dir.join("download.bin");
    probe.take();
    let started = Instant::now();
    let task = Task::builder(adapter)
        .runtime(ThreadedRuntimeImpl::new_tokio_rt())
        .save_to(&path)
        .spawn()
        .expect("a valid task");
    let output = tokio::time::timeout(S5_TIMEOUT, task).await;
    server.abort();
    let output = output
        .unwrap_or_else(|_| panic!("s5 {mode:?}: no end within {S5_TIMEOUT:?}"))
        .expect("the download completes");
    let secs = started.elapsed().as_secs_f64();
    if tracing_all() {
        for (at, message) in probe.take() {
            eprintln!(
                "{:>8.3} {message}",
                at.saturating_duration_since(started).as_secs_f64()
            );
        }
    }
    verify(&output.path, content, "s5_silent_limit_4");
    std::fs::remove_file(&output.path).expect("remove the download");
    let turned_away: Vec<f64> = log
        .turned_away()
        .iter()
        .map(|at| at.saturating_duration_since(started).as_secs_f64())
        .collect();
    LimitedRun {
        secs,
        max_served: log.max_open(),
        turned_away: turned_away.len(),
        first_turned_away: turned_away.first().copied(),
        last_turned_away: turned_away.last().copied(),
    }
}

fn run_s5(runtime: &tokio::runtime::Runtime, runs: usize, probe: &Probe, dir: &std::path::Path) {
    let content = bytes::Bytes::from(create_deterministic_content(S5_SIZE as usize));
    println!();
    println!(
        "| scenario | mode | time s | most served | turned away | first turned away s | last \
         turned away s | MiB/s |"
    );
    println!("|---|---|---|---|---|---|---|---|");
    for mode in [LimitMode::Close, LimitMode::Hang] {
        let mut limited = Vec::new();
        for round in 0..runs {
            let run = runtime.block_on(run_limited(mode, &content, probe, dir));
            eprintln!(
                "s5_silent_limit_4 {mode:?} #{round}: {:.2} s, most served {}, turned away {} \
                 (first at {:?} s, last at {:?} s)",
                run.secs,
                run.max_served,
                run.turned_away,
                run.first_turned_away,
                run.last_turned_away,
            );
            assert!(
                run.max_served <= S5_LIMIT,
                "the server served {} at once",
                run.max_served
            );
            limited.push(run);
        }
        let col = |f: &dyn Fn(&LimitedRun) -> f64, digits| {
            summary(limited.iter().map(f).collect(), digits)
        };
        let at = |f: &dyn Fn(&LimitedRun) -> Option<f64>| {
            summary(limited.iter().filter_map(f).collect(), 2)
        };
        println!(
            "| s5_silent_limit_4 | {mode:?} | {} | {} | {} | {} | {} | {} |",
            col(&|run| run.secs, 2),
            col(&|run| run.max_served as f64, 1),
            col(&|run| run.turned_away as f64, 1),
            at(&|run| run.first_turned_away),
            at(&|run| run.last_turned_away),
            col(&|run| (S5_SIZE as f64 / MIB as f64) / run.secs, 1),
        );
    }
}

fn env_list(name: &str) -> Option<Vec<String>> {
    let value = std::env::var(name).ok()?;
    Some(
        value
            .split(',')
            .map(|item| item.trim().to_owned())
            .collect(),
    )
}

fn main() {
    if !std::env::args().any(|arg| arg == "--bench") {
        return;
    }
    let runs: usize = std::env::var("GROWTH_RUNS")
        .ok()
        .map_or(10, |runs| runs.parse().expect("GROWTH_RUNS is a number"));
    let names = env_list("GROWTH_SCENARIOS");
    check_probed_messages();
    check_parser();
    let probe = install_probe();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime");
    let dir = tempfile::tempdir().expect("a temporary directory");

    println!(
        "| scenario | time s | peak conns | 429s | hedges | decided | duplicate KiB | cancel \
         latency ms | stall wait s | restarts | silent stop requests | holds | probes | unmatched \
         | MiB/s |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    for scenario in scenarios() {
        if names
            .as_ref()
            .is_some_and(|names| !names.iter().any(|name| name == scenario.name))
        {
            continue;
        }
        let mut builder = (scenario.adapter)(scenario.size);
        if let Some(offset) = scenario.stall_at {
            builder = builder.stall_once_at(offset);
        }
        let template = builder.build().expect("an adapter");
        let mut runs_of = Vec::new();
        for round in 0..runs {
            let run = runtime.block_on(run_once(&scenario, &template, &probe, dir.path()));
            eprintln!(
                "{} #{round}: {:.2} s, peak {}, 429s {}, hedges {} (decided {}), duplicate {} KiB \
                 ({} late loser frames dropped), cancel {:?}, stall wait {:?}, restarts {} \
                 (silent stop requests {}), holds {}, probes {}, unmatched {}",
                scenario.name,
                run.secs,
                run.peak,
                run.congested,
                run.hedges,
                run.decisions,
                run.duplicate_bytes / KIB,
                run.late_loser_frames,
                run.cancel_latencies,
                run.stall_wait,
                run.slow_restarts,
                run.silent_restarts,
                run.holds,
                run.probes,
                run.unmatched,
            );
            runs_of.push(run);
        }
        report(&scenario, &runs_of);
    }
    if names
        .as_ref()
        .is_none_or(|names| names.iter().any(|name| name == "s5_silent_limit_4"))
    {
        run_s5(&runtime, runs, &probe, dir.path());
    }
}
