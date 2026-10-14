//! Frame-level benchmark of the production `TaskRunner` event loop.
//!
//! Each case builds a real runner through the public builder, feeds it an in-memory
//! stream, and consumes its lifecycle and data channels like the runner manager does.
//! Timings cover a whole transfer: building and spawning the runner, consuming its
//! channels and tearing it down. The stream items share one payload and the consumer
//! only counts bytes.
//!
//! Cases (`runner_loop` group, 32 MiB per iteration unless noted):
//! - `item_{8k,32k,128k}`: item smaller than, equal to and larger than the 32 KiB
//!   frame buffer (a 128 KiB item spans four frames); known total, fast consumer.
//! - `unknown_total`: 32 KiB items, no total.
//! - `backpressure`: the consumer waits until the data channel is full before draining
//!   it, so the runner repeatedly finds the channel full.
//! - `limit_every_64`: the consumer lowers the runner's limit by one frame every 64
//!   frames; reported per transfer, since the limits shorten it.
//! - `smol_item_32k`: `item_32k` on smol.
//! - `{tokio,smol}_runners_8`: eight runners at once, 4 MiB in total.
//! - `setup`: build, start and stop a runner on a one-byte stream.
//! - `cancel_latency`: time from cancelling a runner waiting on its stream until it
//!   reports `Stopped`.
//!
//! Run with: `cargo bench -p bolt-load --bench runner_loop`
//!
//! Profiling mode (bypasses criterion): `RUNNER_PROFILE=<case>:<iterations>` prints
//! ns/frame; built with `RUSTFLAGS="--cfg coord_alloc"` it also prints allocations per
//! frame from a separate pass.

use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use async_ringbuf::traits::{AsyncConsumer, Consumer};
use bolt_load::{
    adapter::{AdapterError, AnyBytesStream},
    runner::{
        DATA_FRAME_CHANNEL_CAPACITY, DataFrameReceiver, LifecycleEvent, LifecycleReceiver,
        StoppedReason, TaskError, TaskRunner,
    },
    task::ControlEvent,
};
use bytes::Bytes;
use criterion::{Criterion, Throughput, criterion_group};
use futures::StreamExt;
use smol_cancellation_token::CancellationToken;
use tokio::runtime::Runtime;

const TOTAL: u64 = 32 * 1024 * 1024;
/// Mirrors the runner's `BUFFER_SIZE`.
const FRAME: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Consume {
    /// Pop every frame as soon as it arrives.
    Fast,
    /// Wait for a full channel, then drain it; needs a known total of whole frames.
    Batched,
    /// Lower the limit by one frame every `n` frames.
    Limit(usize),
}

#[derive(Clone, Copy, Debug)]
struct Case {
    name: &'static str,
    item: usize,
    known_total: bool,
    consume: Consume,
}

const CASES: &[Case] = &[
    Case {
        name: "item_8k",
        item: 8 * 1024,
        known_total: true,
        consume: Consume::Fast,
    },
    Case {
        name: "item_32k",
        item: 32 * 1024,
        known_total: true,
        consume: Consume::Fast,
    },
    Case {
        name: "item_128k",
        item: 128 * 1024,
        known_total: true,
        consume: Consume::Fast,
    },
    Case {
        name: "unknown_total",
        item: 32 * 1024,
        known_total: false,
        consume: Consume::Fast,
    },
    Case {
        name: "backpressure",
        item: 32 * 1024,
        known_total: true,
        consume: Consume::Batched,
    },
    Case {
        name: "limit_every_64",
        item: 32 * 1024,
        known_total: true,
        consume: Consume::Limit(64),
    },
];

fn stream_of(total: u64, item: usize) -> AnyBytesStream {
    let bytes = Bytes::from(vec![0x5a; item]);
    let items = (total as usize).div_ceil(item);
    Box::pin(futures::stream::iter(
        std::iter::repeat_n(bytes, items).map(Ok::<_, AdapterError>),
    ))
}

struct Built {
    runner: TaskRunner,
    lifecycle_rx: LifecycleReceiver,
    data_rx: DataFrameReceiver,
    control_tx: async_channel::Sender<ControlEvent>,
    cancel_token: CancellationToken,
}

fn build(stream: AnyBytesStream, total: Option<u64>) -> Built {
    let (control_tx, control_rx) = async_channel::bounded(8);
    let cancel_token = CancellationToken::new();
    let mut builder = TaskRunner::builder()
        .stream(stream)
        .runner_id(0)
        .control_signal(control_rx)
        .cancel_token(cancel_token.clone());
    if let Some(total) = total {
        builder = builder.total(total);
    }
    let (runner, lifecycle_rx, data_rx) = builder.build().expect("complete builder");
    Built {
        runner,
        lifecycle_rx,
        data_rx,
        control_tx,
        cancel_token,
    }
}

/// Consume a runner's output until it stops; returns the delivered bytes and frames.
async fn consume(
    mut lifecycle_rx: LifecycleReceiver,
    mut data_rx: DataFrameReceiver,
    control_tx: async_channel::Sender<ControlEvent>,
    mut limit: u64,
    mode: Consume,
) -> (u64, u64) {
    let initial_limit = limit;
    let (mut bytes, mut frames) = (0u64, 0u64);
    loop {
        if mode == Consume::Batched {
            // The runner waits for the channel to drain before its last frames
            // are counted as delivered, so do not wait for more than remain.
            let remaining = (limit - bytes).div_ceil(FRAME as u64) as usize;
            let _ = data_rx
                .wait_occupied(remaining.min(DATA_FRAME_CHANNEL_CAPACITY))
                .await;
        }
        let Some(frame) = data_rx.next().await else {
            break;
        };
        let mut next = Some(frame);
        while let Some(frame) = next {
            bytes += frame.data.len() as u64;
            frames += 1;
            if let Consume::Limit(every) = mode
                && frames.is_multiple_of(every as u64)
                // Above anything the runner may have pushed or buffered meanwhile.
                && limit - FRAME as u64
                    > bytes + ((DATA_FRAME_CHANNEL_CAPACITY + 2) * FRAME) as u64
            {
                limit -= FRAME as u64;
                control_tx
                    .try_send(ControlEvent::LimitTotal(limit))
                    .expect("the runner keeps up with the limits");
            }
            next = data_rx.try_pop();
        }
    }
    let mut stopped = None;
    while let Some(event) = lifecycle_rx.next().await {
        if let LifecycleEvent::Stopped(reason) = event {
            stopped = Some(reason);
        }
    }
    assert!(
        matches!(stopped, Some(StoppedReason::Finished)),
        "{mode:?}: {stopped:?}"
    );
    if matches!(mode, Consume::Limit(_)) {
        assert!(limit < initial_limit, "no limit was sent");
    }
    assert_eq!(bytes, limit);
    (bytes, frames)
}

/// Run one runner of `case` over `total` bytes; returns the delivered frames.
async fn run_case(case: Case, total: u64, spawn: impl FnOnce(TaskRunner)) -> u64 {
    let Built {
        runner,
        lifecycle_rx,
        data_rx,
        control_tx,
        cancel_token: _,
    } = build(
        stream_of(total, case.item),
        case.known_total.then_some(total),
    );
    spawn(runner);
    let (_, frames) = consume(lifecycle_rx, data_rx, control_tx, total, case.consume).await;
    black_box(frames)
}

fn tokio_spawn(rt: &Runtime) -> impl Fn(TaskRunner) + '_ {
    move |mut runner| {
        rt.spawn(async move { runner.run().await });
    }
}

#[cfg(feature = "smol")]
fn smol_spawn(mut runner: TaskRunner) {
    smol::spawn(async move { runner.run().await }).detach();
}

async fn run_many(runners: usize, spawn: impl Fn(TaskRunner)) -> u64 {
    let case = CASES[1];
    let per_runner = TOTAL / 8 / runners as u64;
    let consumers = (0..runners).map(|_| run_case(case, per_runner, &spawn));
    futures::future::join_all(consumers).await.into_iter().sum()
}

/// Time from cancelling a runner waiting on its stream until it reports `Stopped`.
async fn cancel_latency(spawn: impl FnOnce(TaskRunner)) -> Duration {
    let (waiting_tx, waiting_rx) = async_channel::bounded(1);
    // The runner polls its stream after its cancellation.
    let stream = Box::pin(futures::stream::poll_fn(move |_| {
        let _ = waiting_tx.try_send(());
        std::task::Poll::Pending
    }));
    let Built {
        runner,
        mut lifecycle_rx,
        data_rx,
        control_tx,
        cancel_token,
    } = build(stream, Some(1));
    spawn(runner);
    waiting_rx
        .recv()
        .await
        .expect("the runner polls its stream");
    let start = Instant::now();
    cancel_token.cancel();
    let stopped = loop {
        match lifecycle_rx.next().await {
            Some(LifecycleEvent::Started) => {}
            stopped => break stopped,
        }
    };
    let elapsed = start.elapsed();
    assert!(
        matches!(
            stopped,
            Some(LifecycleEvent::Stopped(StoppedReason::Failed(
                TaskError::Cancelled
            )))
        ),
        "{stopped:?}"
    );
    // Wait for the runner to exit before the next iteration.
    assert!(lifecycle_rx.next().await.is_none());
    drop((data_rx, control_tx));
    elapsed
}

fn bench_runner_loop(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group("runner_loop");
    for &case in CASES {
        group.throughput(match case.consume {
            Consume::Limit(_) => Throughput::Elements(1),
            _ => Throughput::Bytes(TOTAL),
        });
        group.bench_function(case.name, |b| {
            b.to_async(&rt)
                .iter(|| run_case(case, TOTAL, tokio_spawn(&rt)));
        });
    }
    group.throughput(Throughput::Bytes(TOTAL));
    #[cfg(feature = "smol")]
    group.bench_function("smol_item_32k", |b| {
        b.iter(|| smol::block_on(run_case(CASES[1], TOTAL, smol_spawn)));
    });

    group.throughput(Throughput::Bytes(TOTAL / 8));
    group.bench_function("tokio_runners_8", |b| {
        b.to_async(&rt).iter(|| run_many(8, tokio_spawn(&rt)));
    });
    #[cfg(feature = "smol")]
    group.bench_function("smol_runners_8", |b| {
        b.iter(|| smol::block_on(run_many(8, smol_spawn)));
    });

    group.throughput(Throughput::Elements(1));
    group.bench_function("setup", |b| {
        let case = Case {
            item: 1,
            ..CASES[1]
        };
        b.to_async(&rt).iter(|| run_case(case, 1, tokio_spawn(&rt)));
    });
    let rt_ref = &rt;
    group.bench_function("cancel_latency", |b| {
        b.to_async(rt_ref).iter_custom(|iters| async move {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += cancel_latency(tokio_spawn(rt_ref)).await;
            }
            total
        });
    });
    group.finish();
}

criterion_group!(benches, bench_runner_loop);

#[cfg(coord_alloc)]
mod alloc_counter {
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    pub static ALLOCS: AtomicUsize = AtomicUsize::new(0);
    pub static COUNT_ALLOCS: AtomicBool = AtomicBool::new(false);

    struct CountingAlloc;

    #[inline]
    fn count() {
        if COUNT_ALLOCS.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
    }

    // SAFETY: forwards every call to `System` unchanged.
    unsafe impl GlobalAlloc for CountingAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            count();
            // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            count();
            // SAFETY: the caller upholds `GlobalAlloc::alloc_zeroed`'s contract.
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            count();
            // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static GLOBAL: CountingAlloc = CountingAlloc;
}

/// `RUNNER_PROFILE=<case>:<iterations>` runs one case outside criterion.
fn profile(spec: &str) {
    let (name, iterations) = spec.split_once(':').expect("<case>:<iterations>");
    let iterations: usize = iterations.parse().expect("iterations");
    let case = *CASES.iter().find(|c| c.name == name).expect("unknown case");
    let rt = Runtime::new().expect("tokio runtime");
    let start = Instant::now();
    let mut frames = 0;
    for _ in 0..iterations {
        frames += rt.block_on(run_case(case, TOTAL, tokio_spawn(&rt)));
    }
    let elapsed = start.elapsed();
    println!(
        "{name}: {frames} frames in {elapsed:?} -> {:.1} ns/frame",
        elapsed.as_nanos() as f64 / frames as f64
    );
    #[cfg(coord_alloc)]
    {
        use std::sync::atomic::Ordering;

        use alloc_counter::{ALLOCS, COUNT_ALLOCS};
        let mut frames = 0;
        COUNT_ALLOCS.store(true, Ordering::Relaxed);
        for _ in 0..iterations {
            frames += rt.block_on(run_case(case, TOTAL, tokio_spawn(&rt)));
        }
        COUNT_ALLOCS.store(false, Ordering::Relaxed);
        println!(
            "  {:.2} allocs/frame (process-wide, amortized)",
            ALLOCS.load(Ordering::Relaxed) as f64 / frames as f64
        );
    }
}

fn main() {
    if let Ok(spec) = std::env::var("RUNNER_PROFILE") {
        profile(&spec);
        return;
    }
    benches();
    Criterion::default().configure_from_args().final_summary();
}
