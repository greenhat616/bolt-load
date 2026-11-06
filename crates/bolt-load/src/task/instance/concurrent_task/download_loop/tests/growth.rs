//! The growth strategies in the loop (design §9.2, §9.3, §14.14).
//!
//! Runners are parked and never connect: the tests start them and accept
//! their bytes through the manager, and pass every tick its time.

use std::ops::Range;

use pretty_assertions::assert_eq;

use super::{
    super::super::{
        chunk_planner::ChunkStatus,
        strategy::{GROW_INTERVAL, MIN_GROWTH_GAIN, RunnerFailureKind, StrategyAction},
    },
    *,
};
use crate::{adapter::UnretryableError, task::GrowthStrategy};

const TOTAL: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
const NANO: Duration = Duration::from_nanos(1);
const TICK: Duration = DEFAULT_STRATEGY_TICK_INTERVAL;

/// A loop with `growth` and `max_connections` on `rt`, whose runners are
/// parked and never connect.
async fn growth_loop(
    rt: &ScriptedRuntime,
    growth: GrowthStrategy,
    max_connections: usize,
) -> (DownloadLoop, FileWriter) {
    let options = TaskOptions::default()
        .max_connections(NonZeroUsize::new(max_connections).unwrap())
        .growth_strategy(growth);
    loop_with_options(
        ThreadedRuntimeImpl::new_other_rt(rt.clone()),
        DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(1).with_range_support(true)),
        TOTAL,
        &options,
    )
    .await
}

/// The runners whose chunk has `status`, by ID.
fn runners(download: &DownloadLoop, status: ChunkStatus) -> Vec<crate::task::RunnerId> {
    (0..64)
        .filter(|&id| {
            download
                .manager
                .get_runner_state(id)
                .is_some_and(|state| state.status == status)
        })
        .collect()
}

/// Start the connecting runners, and let every running one deliver
/// `bytes`.
fn deliver(download: &mut DownloadLoop, bytes: u64) {
    for runner_id in runners(download, ChunkStatus::Pending) {
        download.manager.mark_started(runner_id);
    }
    for runner_id in runners(download, ChunkStatus::Running) {
        download.manager.update_progress(runner_id, bytes).unwrap();
    }
}

fn allocated(download: &DownloadLoop, runner_id: crate::task::RunnerId) -> Range<u64> {
    download
        .manager
        .get_runner_state(runner_id)
        .unwrap()
        .allocated
        .clone()
}

/// Every chunk and unassigned range, in order, covers the file once.
fn assert_covered(download: &DownloadLoop) {
    let mut ranges = download.manager.chunk_ranges();
    ranges.extend(download.manager.get_available_ranges());
    ranges.sort_by_key(|range| range.start);
    let mut end = 0;
    for range in ranges {
        assert_eq!(range.start, end, "{range:?}");
        end = range.end;
    }
    assert_eq!(end, TOTAL);
}

/// Under `Doubling` the loop goes 1, 2, 4 through the real split, at most
/// every 2 s, and stops at the limit.
#[tokio::test]
async fn doubling_grows_through_split_up_to_the_limit() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, file_writer) = growth_loop(&rt, GrowthStrategy::Doubling, 4).await;
    let t0 = Instant::now();
    // The first tick creates the first runner.
    download.on_strategy_tick(t0);
    assert_eq!(download.manager.allocated_runner_count(), 1);

    // One runner at 1 MiB a tick: room for one more.
    deliver(&mut download, MIB);
    download.on_strategy_tick(t0 + TICK);
    assert_eq!(download.manager.get_active_runners_count(), 2);
    let half = MIB + (TOTAL - MIB) / 2;
    assert_eq!(allocated(&download, 0), 0..half);
    assert_eq!(allocated(&download, 1), half..TOTAL);
    assert_eq!(runners(&download, ChunkStatus::Pending), [1]);
    // Kept, so the runners still take their limits.
    let mut parked = rt.take_parked();
    assert_eq!(parked.len(), 2, "the split spawned a runner");

    // Too early, though both deliver.
    deliver(&mut download, MIB);
    download.on_strategy_tick(t0 + TICK * 2);
    assert_eq!(download.manager.get_active_runners_count(), 2);

    // Twice the speed: up to four, the limit.
    deliver(&mut download, MIB);
    download.on_strategy_tick(t0 + TICK * 3);
    assert_eq!(download.manager.get_active_runners_count(), 4);
    assert_eq!(download.manager.allocated_runner_count(), 4);
    assert_eq!(runners(&download, ChunkStatus::Pending), [2, 3]);
    assert_covered(&download);

    for n in 4..10 {
        deliver(&mut download, MIB);
        download.on_strategy_tick(t0 + TICK * n);
        assert_eq!(download.manager.allocated_runner_count(), 4, "tick {n}");
    }
    parked.extend(rt.take_parked());
    assert_eq!(parked.len(), 4);
    assert_covered(&download);
    drop(download);
    drop(parked);
    file_writer.finalize().await.unwrap();
}

/// A growth only counts the runners a free slot took: the refused split
/// is tried again at the next tick, and the next growth waits 2 s from
/// the one that added a runner.
#[tokio::test]
async fn doubling_counts_only_the_splits_a_slot_took() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, file_writer) = growth_loop(&rt, GrowthStrategy::Doubling, 4).await;
    let t0 = Instant::now();
    download.on_strategy_tick(t0);
    deliver(&mut download, MIB);
    download.on_strategy_tick(t0 + TICK);
    assert_eq!(download.manager.get_active_runners_count(), 2);

    // A runner that finished but has not stopped holds a slot.
    let held = download.manager.try_reserve_slot().unwrap();
    deliver(&mut download, 2 * MIB);
    let grown = t0 + TICK * 3;
    download.on_strategy_tick(grown);
    assert_eq!(download.manager.get_active_runners_count(), 3);
    assert_eq!(download.manager.allocated_runner_count(), 4);

    // A step short of a doubling is not judged; no slot is free: no
    // growth.
    deliver(&mut download, 3 * MIB);
    download.on_strategy_tick(grown + GROW_INTERVAL);
    assert_eq!(download.manager.get_active_runners_count(), 3);
    assert_eq!(download.manager.allocated_runner_count(), 4);

    // The slot is free again: the next tick grows, without waiting 2 s.
    download.manager.release(held);
    deliver(&mut download, 3 * MIB);
    download.on_strategy_tick(grown + GROW_INTERVAL + TICK);
    assert_eq!(download.manager.get_active_runners_count(), 4);
    assert_covered(&download);
    drop(download);
    file_writer.finalize().await.unwrap();
}

/// Start the connecting runners, and let every running one deliver
/// `bytes(running)`, where `running` counts the running runners.
fn deliver_each(download: &mut DownloadLoop, bytes: impl Fn(u64) -> u64) {
    for runner_id in runners(download, ChunkStatus::Pending) {
        download.manager.mark_started(runner_id);
    }
    let running = runners(download, ChunkStatus::Running);
    let each = bytes(running.len() as u64);
    for runner_id in running {
        download.manager.update_progress(runner_id, each).unwrap();
    }
}

/// Eight runners of `max_connections = 16`; the real concurrency control
/// degrades the limit to them on a 503, then recovers it one runner at a
/// time: 30 s to the first step, then 15 s a step. Every tick, the running
/// runners deliver `bytes(running)` each. Returns the limits in order and
/// the active runners at the end.
async fn recover_a_step_at_a_time(bytes: impl Fn(u64) -> u64) -> (Vec<usize>, usize) {
    const RUNNERS: u64 = 8;
    const MAX: usize = 16;
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, file_writer) = growth_loop(&rt, GrowthStrategy::Doubling, MAX).await;
    let chunk = TOTAL / RUNNERS;
    let _slots: Vec<_> = (0..RUNNERS)
        .map(|i| {
            download
                .manager
                .allocate_runner_with_chunk(i * chunk..(i + 1) * chunk)
                .unwrap()
        })
        .collect();
    download
        .manager
        .runner_outcome_sampler_mut()
        .record_connect_failed(RunnerFailureKind::Unretryable);
    let t0 = Instant::now();
    // The first step only creates runners; the next degrades to the eight.
    download.on_strategy_tick(t0);
    deliver_each(&mut download, &bytes);
    download.on_strategy_tick(t0 + TICK);
    assert_eq!(download.max_concurrency, 8);
    // Few enough failures since for the recovery.
    for _ in 0..6 {
        download
            .manager
            .runner_outcome_sampler_mut()
            .record_completed();
    }

    let mut limits = vec![download.max_concurrency];
    for n in 2..122 {
        deliver_each(&mut download, &bytes);
        download.on_strategy_tick(t0 + TICK * n);
        let active = download.manager.get_active_runners_count();
        assert!(
            active <= download.max_concurrency,
            "tick {n}: {active} runners"
        );
        if limits.last() != Some(&download.max_concurrency) {
            limits.push(download.max_concurrency);
        }
    }
    assert_covered(&download);
    let active = download.manager.get_active_runners_count();
    drop(download);
    file_writer.finalize().await.unwrap();
    (limits, active)
}

/// The task's speed grows with its runners, so from ten on each step of
/// the recovery gains less than 10%; the steps are short of a doubling, so
/// none is judged, and the loop grows back to `max_connections`.
#[tokio::test]
async fn doubling_grows_back_as_the_limit_recovers_a_step_at_a_time() {
    const KIB: u64 = 1 << 10;
    let (limits, active) = recover_a_step_at_a_time(|_| 64 * KIB).await;
    assert_eq!(limits, (8..=16).collect::<Vec<_>>());
    assert_eq!(active, 16);
}

/// The accepted blind spot (design §14.14): with the task's speed fixed
/// whatever the runners, a recovery one step at a time is never judged
/// either, so no plateau is found, and the loop keeps splitting up to
/// each new limit.
#[tokio::test]
async fn doubling_refills_a_step_at_a_time_without_any_gain() {
    const KIB: u64 = 1 << 10;
    // 512 KiB a tick in all.
    let (limits, active) = recover_a_step_at_a_time(|running| 512 * KIB / running).await;
    assert_eq!(limits, (8..=16).collect::<Vec<_>>());
    assert_eq!(active, 16);
}

/// No growth while a runner connects or delivers nothing, nor before the
/// congestion ends; the tick at its end grows.
#[tokio::test]
async fn doubling_waits_for_every_runner_and_the_congestion() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, file_writer) = growth_loop(&rt, GrowthStrategy::Doubling, 4).await;
    let t0 = Instant::now();
    download.on_strategy_tick(t0);
    // Still connecting.
    download.on_strategy_tick(t0 + TICK);
    assert_eq!(download.manager.get_active_runners_count(), 1);
    // Running, but nothing accepted in the tick.
    download.manager.mark_started(0);
    download.on_strategy_tick(t0 + TICK * 2);
    assert_eq!(download.manager.get_active_runners_count(), 1);

    // A 503 elsewhere holds back new connections for 2 s.
    download.backoff.on_failure(
        &TaskError::StreamError {
            source: AdapterError::Unretryable {
                source: UnretryableError::Congestion {
                    status: 503,
                    retry_after: Some(Duration::from_secs(2)),
                },
            },
        },
        1,
        t0 + TICK * 2,
    );
    let until = download.backoff.congestion_until().unwrap();
    assert_eq!(until, t0 + TICK * 2 + Duration::from_secs(2));
    deliver(&mut download, MIB);
    download.on_strategy_tick(t0 + TICK * 3);
    assert_eq!(download.manager.get_active_runners_count(), 1);

    // The boundary, by the strategies alone: a tick's sample with every
    // runner delivering.
    let tick = super::super::super::strategy::TickSample {
        speed: Some(MIB as f64),
        all_delivering: true,
        rates: Default::default(),
    };
    let grows_at = |download: &mut DownloadLoop, now: Instant| {
        download
            .strategy
            .execute(
                download.max_concurrency,
                0.0,
                0.0,
                &mut download.manager,
                now,
                download.backoff.is_congested(now),
                Some(&tick),
            )
            .map(|(name, actions)| (name, format!("{actions:?}")))
    };
    assert_eq!(grows_at(&mut download, until - NANO), None);
    assert_eq!(
        grows_at(&mut download, until),
        Some(("doubling", format!("{:?}", [StrategyAction::Grow(1)])))
    );

    // The tick at its end grows.
    deliver(&mut download, MIB);
    download.on_strategy_tick(until);
    assert_eq!(download.manager.get_active_runners_count(), 2);
    drop(download);
    file_writer.finalize().await.unwrap();
}

/// A growth that gained under 10% stops the growth for good.
#[tokio::test]
async fn doubling_stops_at_a_plateau() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, file_writer) = growth_loop(&rt, GrowthStrategy::Doubling, 8).await;
    let t0 = Instant::now();
    download.on_strategy_tick(t0);
    deliver(&mut download, 10 * MIB);
    download.on_strategy_tick(t0 + TICK);
    assert_eq!(download.manager.get_active_runners_count(), 2);

    // Two runners at 5 MiB a tick each: no faster than one at 10 MiB.
    deliver(&mut download, 5 * MIB);
    download.on_strategy_tick(t0 + TICK * 2);
    deliver(&mut download, 5 * MIB);
    download.on_strategy_tick(t0 + TICK * 3);
    assert_eq!(download.manager.get_active_runners_count(), 2);
    for n in 4..10 {
        deliver(&mut download, 50 * MIB);
        download.on_strategy_tick(t0 + TICK * n);
    }
    assert_eq!(download.manager.get_active_runners_count(), 2);
    drop(download);
    file_writer.finalize().await.unwrap();
}

/// A chunk is split, at its half, only when it holds at least the planned
/// size for its runner, 2 s at that runner's own rate: a byte less is not
/// split, exactly that is.
#[tokio::test]
async fn doubling_splits_only_a_chunk_of_the_planned_size() {
    const SECOND: Duration = Duration::from_secs(1);
    // 14 MiB/s plans 28 MiB, a byte more than is left after it; the next
    // rate plans exactly what is left after it.
    const FIRST: u64 = 14 * MIB;
    const SECOND_RATE: u64 = (2 * FIRST - 1) / 3;
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, file_writer) = growth_loop(&rt, GrowthStrategy::Doubling, 4).await;
    let t0 = Instant::now();
    let mut slot = download
        .manager
        .allocate_runner_with_chunk(0..TOTAL)
        .unwrap();
    download
        .manager
        .update_progress(0, TOTAL - (3 * FIRST - 1))
        .unwrap();
    download.on_strategy_tick(t0);

    // A byte under the planned size: not split, and the refused growth
    // does not count.
    download.manager.update_progress(0, FIRST).unwrap();
    download.on_strategy_tick(t0 + SECOND);
    assert_eq!(download.planned_split_size(0), 2 * FIRST);
    let left = |download: &DownloadLoop| download.manager.get_runner_state(0).unwrap().remaining();
    assert_eq!(left(&download), 2 * FIRST - 1);
    assert_eq!(download.manager.get_active_runners_count(), 1);
    assert_eq!(slot.limits.try_recv(), None);

    // Exactly the planned size: split at the half, a second later.
    download.manager.update_progress(0, SECOND_RATE).unwrap();
    assert_eq!(left(&download), 2 * SECOND_RATE);
    download.on_strategy_tick(t0 + 2 * SECOND);
    assert_eq!(download.planned_split_size(0), 2 * SECOND_RATE);
    assert_eq!(download.manager.get_active_runners_count(), 2);
    assert_eq!(slot.limits.try_recv(), Some(TOTAL - SECOND_RATE));
    drop(download);
    file_writer.finalize().await.unwrap();
}

/// The planned size follows the split runner's own rate, whatever the
/// task's speed per runner; before the runner has a rate, the task's.
#[tokio::test]
async fn planned_split_size_follows_the_donor_rate() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let mut planned = Vec::new();
    for active in [2, 8] {
        let (mut download, file_writer) = growth_loop(&rt, GrowthStrategy::Doubling, 8).await;
        let t0 = Instant::now();
        let chunk = TOTAL / active;
        let _slots: Vec<_> = (0..active)
            .map(|i| {
                download
                    .manager
                    .allocate_runner_with_chunk(i * chunk..(i + 1) * chunk)
                    .unwrap()
            })
            .collect();
        // 16 MiB/s over the runners: 8 or 2 MiB/s each.
        download.throughput.per_runner_speed = 16.0 * MIB as f64 / active as f64;
        download.on_strategy_tick(t0);
        // No rate yet: the task's speed per runner.
        assert_eq!(
            download.planned_split_size(0),
            2 * (16 * MIB / active),
            "{active} runners"
        );
        // Every runner at 3 MiB/s: the task at 6 or 24 MiB/s.
        for runner_id in 0..active as usize {
            download
                .manager
                .update_progress(runner_id, 3 * MIB)
                .unwrap();
        }
        download.on_strategy_tick(t0 + Duration::from_secs(1));
        planned.push(download.planned_split_size(0));
        drop(download);
        file_writer.finalize().await.unwrap();
    }
    assert_eq!(planned, [6 * MIB, 6 * MIB]);
}

/// Without the switch, or with `Legacy`, the loop splits as it always did:
/// the dynamic strategy on the task's speed.
#[tokio::test]
async fn legacy_is_the_default_and_unchanged() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    for options in [
        TaskOptions::default(),
        TaskOptions::default().growth_strategy(GrowthStrategy::Legacy),
    ] {
        assert_eq!(options.resolved_growth_strategy(), GrowthStrategy::Legacy);
        let (mut download, file_writer) = loop_with_options(
            ThreadedRuntimeImpl::new_other_rt(rt.clone()),
            DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(1).with_range_support(true)),
            TOTAL,
            &options,
        )
        .await;
        assert!(!download.strategy.is_doubling());
        assert!(download.rates.is_none());
        let _slot = download
            .manager
            .allocate_runner_with_chunk(0..TOTAL)
            .unwrap();
        let execute = |download: &mut DownloadLoop, speed: f64| {
            download
                .strategy
                .execute(
                    download.max_concurrency,
                    speed,
                    speed,
                    &mut download.manager,
                    Instant::now(),
                    false,
                    None,
                )
                .map(|(name, actions)| (name, format!("{actions:?}")))
        };
        // Planned at twice the task's speed.
        assert_eq!(
            execute(&mut download, 1.5 * MIB as f64),
            Some((
                "dynamic",
                format!("{:?}", [StrategyAction::SplitAllTask(3 * MIB)])
            ))
        );
        // `abs()`: a drop splits too.
        assert_eq!(
            execute(&mut download, 0.5 * MIB as f64),
            Some((
                "dynamic",
                format!("{:?}", [StrategyAction::SplitAllTask(MIB)])
            ))
        );
        drop(download);
        file_writer.finalize().await.unwrap();
    }
}

/// The gain is judged by the task's speed in the tick before the growth
/// and in the first tick at least 2 s after it. The strategy's tests take
/// the boundary exactly; here the speeds are measured, so a KiB either
/// side of it.
#[tokio::test]
async fn doubling_judges_the_gain_at_ten_percent() {
    for (bytes, grows) in [
        // 10 MiB a tick before; 11 MiB over two runners is a gain of 1.10.
        (11 * MIB / 2 + 1024, true),
        (11 * MIB / 2 - 1024, false),
    ] {
        let rt = ScriptedRuntime::new(TimerKind::Scripted);
        let (mut download, file_writer) = growth_loop(&rt, GrowthStrategy::Doubling, 8).await;
        let t0 = Instant::now();
        download.on_strategy_tick(t0);
        deliver(&mut download, 10 * MIB);
        download.on_strategy_tick(t0 + TICK);
        deliver(&mut download, bytes);
        download.on_strategy_tick(t0 + TICK * 2);
        deliver(&mut download, bytes);
        download.on_strategy_tick(t0 + TICK * 3);
        assert_eq!(
            download.manager.get_active_runners_count(),
            if grows { 4 } else { 2 },
            "{bytes} bytes a runner, {MIN_GROWTH_GAIN}"
        );
        drop(download);
        file_writer.finalize().await.unwrap();
    }
}
