//! The growth strategies in the loop (design §9.2, §9.3, §14.14).
//!
//! Runners are parked and never connect: the tests start them and accept
//! their bytes through the manager, and pass every tick its time.

use std::ops::Range;

use pretty_assertions::assert_eq;

use super::{
    super::super::{
        chunk_planner::ChunkStatus,
        strategy::{
            AdmissionClass, BASE_REPROBE_WAIT, GROW_INTERVAL, MIN_GROWTH_GAIN, StrategyAction,
            UNSEEN, growth_deadline,
        },
    },
    *,
};
use crate::adapter::UnretryableError;

const TOTAL: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
const NANO: Duration = Duration::from_nanos(1);
const TICK: Duration = DEFAULT_STRATEGY_TICK_INTERVAL;

/// A loop with `max_connections` on `rt`, whose runners are parked and
/// never connect.
async fn growth_loop(rt: &ScriptedRuntime, max_connections: usize) -> (DownloadLoop, FileWriter) {
    let options =
        TaskOptions::default().max_connections(NonZeroUsize::new(max_connections).unwrap());
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
/// `bytes`. A runner that delivers at every tick reads all along: it is
/// never silent.
fn deliver(download: &mut DownloadLoop, bytes: u64) {
    for runner_id in runners(download, ChunkStatus::Pending) {
        download.manager.mark_started(runner_id);
    }
    for runner_id in runners(download, ChunkStatus::Running) {
        download.manager.update_progress(runner_id, bytes).unwrap();
        if let Some(reads) = download.manager.read_activity(runner_id) {
            reads.start_emitting();
        }
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
    let (mut download, file_writer) = growth_loop(&rt, 4).await;
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
    let (mut download, file_writer) = growth_loop(&rt, 4).await;
    let t0 = Instant::now();
    download.on_strategy_tick(t0);
    deliver(&mut download, MIB);
    download.on_strategy_tick(t0 + TICK);
    assert_eq!(download.manager.get_active_runners_count(), 2);
    // Too early to grow; the new runner connects, so the next tick
    // measures the two (design §14.33).
    deliver(&mut download, MIB);
    download.on_strategy_tick(t0 + TICK * 2);
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
/// lowers the limit to them on a 503, then recovers it one runner at a
/// time (design §8.3): the first step 30 s after the lowering, and each
/// step accepted once its trial, which the doubling opens by a split, has
/// been connected and delivering for 15 s; the next step begins at the
/// tick after. Every tick, the running runners deliver `bytes(running)`
/// each. Returns the limits in order and the active runners at the end.
async fn recover_a_step_at_a_time(bytes: impl Fn(u64) -> u64) -> (Vec<usize>, usize) {
    const RUNNERS: u64 = 8;
    const MAX: usize = 16;
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, file_writer) = growth_loop(&rt, MAX).await;
    let chunk = TOTAL / RUNNERS;
    let _slots: Vec<_> = (0..RUNNERS)
        .map(|i| {
            download
                .manager
                .allocate_runner_with_chunk(i * chunk..(i + 1) * chunk)
                .unwrap()
        })
        .collect();
    // Started as their lifecycle would report it.
    for id in 0..RUNNERS as usize {
        download.manager.admission_mut().on_started(id, id);
    }
    // A 503 beside them.
    download
        .manager
        .admission_mut()
        .on_attempt_ended(UNSEEN, AdmissionClass::Congestion, 8, 8);
    let t0 = Instant::now();
    // The first step only creates runners; the next lowers to the eight.
    download.on_strategy_tick(t0);
    deliver_each(&mut download, &bytes);
    download.on_strategy_tick(t0 + TICK);
    assert_eq!(download.max_concurrency, 8);

    let mut limits = vec![download.max_concurrency];
    for n in 2..202 {
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
    assert_eq!(download.strategy.limit_state().0, 16);
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
    let (mut download, file_writer) = growth_loop(&rt, 4).await;
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
        silent: false,
        stable: true,
        rates: Default::default(),
    };
    let grows_at = |download: &mut DownloadLoop, now: Instant| {
        let signals = download.signals(now);
        download
            .strategy
            .execute(
                download.max_concurrency,
                &mut download.manager,
                now,
                signals,
                &tick,
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

/// A growth that gained under 10% holds the runners it doubled from
/// (design §14.33): the one it added runs on, nothing is added while two
/// run, however fast the task gets, and the growth wants one connection.
#[tokio::test]
async fn doubling_holds_at_a_plateau() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, file_writer) = growth_loop(&rt, 8).await;
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
    assert_eq!(download.strategy.want(), 1);
    for n in 4..10 {
        deliver(&mut download, 50 * MIB);
        download.on_strategy_tick(t0 + TICK * n);
    }
    assert_eq!(download.manager.get_active_runners_count(), 2);
    assert_eq!(download.strategy.want(), 1);
    drop(download);
    file_writer.finalize().await.unwrap();
}

/// A doubling up to the limit is judged there (design §14.18): two
/// runners no faster than one hold one, so when one finishes, the other
/// is not split to replace it, however fast it gets, until the hold is
/// probed 30 s after the verdict (design §14.33).
#[tokio::test]
async fn doubling_judges_the_doubling_at_the_limit() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, file_writer) = growth_loop(&rt, 2).await;
    let t0 = Instant::now();
    download.on_strategy_tick(t0);
    deliver(&mut download, 10 * MIB);
    download.on_strategy_tick(t0 + TICK);
    assert_eq!(download.manager.get_active_runners_count(), 2);

    // At the limit and no faster: judged at the first tick 2 s later.
    for n in 2..4 {
        deliver(&mut download, 5 * MIB);
        download.on_strategy_tick(t0 + TICK * n);
    }
    // Runner 1 finishes and stops.
    download.manager.update_progress(1, u64::MAX).unwrap();
    download.manager.release_runner_full(1, true).unwrap();
    // The verdict at 4.5 s: the probe at 34.5 s.
    for n in 4..23 {
        deliver(&mut download, 20 * MIB);
        download.on_strategy_tick(t0 + TICK * n);
        assert_eq!(download.manager.get_active_runners_count(), 1, "tick {n}");
    }
    deliver(&mut download, 20 * MIB);
    download.on_strategy_tick(t0 + TICK * 23);
    assert_eq!(download.manager.get_active_runners_count(), 2);
    assert_eq!(download.strategy.want(), 2);
    assert_covered(&download);
    drop(download);
    file_writer.finalize().await.unwrap();
}

/// [`deliver_each`], every running runner reading all along: none is
/// silent.
fn deliver_reading(download: &mut DownloadLoop, bytes: impl Fn(u64) -> u64) {
    deliver_each(download, bytes);
    for runner_id in runners(download, ChunkStatus::Running) {
        if let Some(reads) = download.manager.read_activity(runner_id) {
            reads.start_emitting();
        }
    }
}

/// Grow 1, 2, 4 under `max_connections = 8`, the task's speed growing
/// with its runners up to two; the doubling to four gains nothing, and
/// holds two. Every tick, the running runners deliver
/// `bytes(running)` each. Returns the loop, the time of the verdict, and
/// the writer.
async fn held_at_two(
    rt: &ScriptedRuntime,
    bytes: impl Fn(u64) -> u64,
) -> (DownloadLoop, Instant, FileWriter) {
    let (mut download, file_writer) = growth_loop(rt, 8).await;
    let t0 = Instant::now();
    download.on_strategy_tick(t0);
    for n in 1..=5 {
        deliver_reading(&mut download, &bytes);
        download.on_strategy_tick(t0 + TICK * n);
    }
    assert_eq!(download.manager.get_active_runners_count(), 4);
    assert_eq!(download.strategy.want(), 2);
    (download, t0 + TICK * 5, file_writer)
}

/// The task's speed: a MiB a tick per runner, up to two MiB a tick.
fn two_mib_a_tick(running: u64) -> u64 {
    2 * MIB / running.max(2)
}

/// Complete and stop the running runner with the least left.
fn complete_the_smallest(download: &mut DownloadLoop) {
    let running = runners(download, ChunkStatus::Running);
    let smallest = *running
        .iter()
        .min_by_key(|&&id| download.manager.get_runner_state(id).unwrap().remaining())
        .unwrap();
    download
        .manager
        .update_progress(smallest, u64::MAX)
        .unwrap();
    download
        .manager
        .release_runner_full(smallest, true)
        .unwrap();
}

/// A long download whose bandwidth returns (design §14.33): a gainless
/// doubling from two holds two; the two it added finish, and the hold is
/// probed 30 s after the verdict. By then the server serves twice as
/// much: the probe gains, and the ramp doubles on to the limit.
#[tokio::test]
async fn a_long_download_grows_again_when_capacity_returns() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, judged, file_writer) = held_at_two(&rt, two_mib_a_tick).await;
    complete_the_smallest(&mut download);
    complete_the_smallest(&mut download);
    let probe = judged + Duration::from_secs(30);
    let mut now = judged;
    while now + TICK < probe {
        now += TICK;
        deliver_reading(&mut download, two_mib_a_tick);
        download.on_strategy_tick(now);
        assert_eq!(download.manager.get_active_runners_count(), 2, "{now:?}");
    }
    assert_eq!(now + TICK, probe);
    deliver_reading(&mut download, two_mib_a_tick);
    download.on_strategy_tick(probe);
    assert_eq!(download.manager.get_active_runners_count(), 4);
    assert_eq!(download.strategy.want(), 4);

    // A MiB a tick per runner now, judged at the first tick 2 s later.
    let mut active = Vec::new();
    for n in 1..=4 {
        deliver_reading(&mut download, |_| MIB);
        download.on_strategy_tick(probe + TICK * n);
        active.push(download.manager.get_active_runners_count());
    }
    assert_eq!(active, [4, 8, 8, 8]);
    assert_eq!(download.strategy.want(), 8);
    assert_covered(&download);
    drop(download);
    file_writer.finalize().await.unwrap();
}

/// A hold is probed from the runners active at its time, more than it
/// holds when the runners a gainless doubling added still run (design
/// §14.33, D31): a doubling from two to four gains nothing, all four run
/// on, and 30 s after the verdict the probe doubles the four to the limit
/// of eight. With the bandwidth doubled by then it gains and ramps on;
/// without, it holds the four it probed from.
#[tokio::test]
async fn a_hold_is_probed_from_the_runners_still_running() {
    for (bytes_after, want_after) in [(MIB, 8), (MIB / 4, 4)] {
        let rt = ScriptedRuntime::new(TimerKind::Scripted);
        let (mut download, judged, file_writer) = held_at_two(&rt, two_mib_a_tick).await;
        let probe = judged + Duration::from_secs(30);
        let mut now = judged;
        while now + TICK < probe {
            now += TICK;
            deliver_reading(&mut download, two_mib_a_tick);
            download.on_strategy_tick(now);
            assert_eq!(download.manager.get_active_runners_count(), 4, "{now:?}");
            assert_eq!(download.strategy.want(), 2);
        }
        deliver_reading(&mut download, two_mib_a_tick);
        download.on_strategy_tick(probe);
        assert_eq!(download.manager.get_active_runners_count(), 8);
        assert_eq!(download.strategy.want(), 8);

        // Judged at the first tick 2 s later: eight MiB a tick in all, or
        // the same two.
        for n in 1..=2 {
            deliver_reading(&mut download, |_| bytes_after);
            download.on_strategy_tick(probe + TICK * n);
        }
        assert_eq!(download.manager.get_active_runners_count(), 8);
        assert_eq!(
            download.strategy.want(),
            want_after,
            "{bytes_after} B a tick"
        );
        assert_covered(&download);
        drop(download);
        file_writer.finalize().await.unwrap();
    }
}

/// Let the pending runners connect, and each running one deliver
/// `bytes(runner_id)`, reading all along.
fn deliver_by_runner(download: &mut DownloadLoop, bytes: impl Fn(crate::task::RunnerId) -> u64) {
    for runner_id in runners(download, ChunkStatus::Pending) {
        download.manager.mark_started(runner_id);
    }
    for runner_id in runners(download, ChunkStatus::Running) {
        download
            .manager
            .update_progress(runner_id, bytes(runner_id))
            .unwrap();
        if let Some(reads) = download.manager.read_activity(runner_id) {
            reads.start_emitting();
        }
    }
}

/// Two of the four runners of a hold of two finish in the tick that
/// reaches the probe time, the bandwidth now a MiB a tick per runner
/// (design §14.33). The tick's speed counts their last bytes, so it does
/// not measure the two left: the probe waits a tick, starts from the
/// two at two MiB a tick, gains with four at four, and the ramp doubles
/// on to the limit. Started at the tick with the finished runners, it
/// would have compared the four with the six or so MiB of that tick and
/// held two for 60 s.
#[tokio::test]
async fn a_probe_waits_out_a_tick_in_which_runners_finished() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, judged, file_writer) = held_at_two(&rt, two_mib_a_tick).await;
    let probe = judged + Duration::from_secs(30);
    let mut now = judged;
    while now + TICK < probe {
        now += TICK;
        deliver_reading(&mut download, |_| MIB);
        download.on_strategy_tick(now);
        assert_eq!(download.manager.get_active_runners_count(), 4, "{now:?}");
    }
    deliver_reading(&mut download, |_| MIB);
    complete_the_smallest(&mut download);
    complete_the_smallest(&mut download);
    download.on_strategy_tick(probe);
    assert_eq!(download.manager.get_active_runners_count(), 2);
    assert_eq!(download.strategy.want(), 2);

    deliver_reading(&mut download, |_| MIB);
    download.on_strategy_tick(probe + TICK);
    assert_eq!(download.manager.get_active_runners_count(), 4);
    assert_eq!(download.strategy.want(), 4);
    // The new runners connect in the next tick; the one after judges.
    for n in 2..=3 {
        deliver_reading(&mut download, |_| MIB);
        download.on_strategy_tick(probe + TICK * n);
    }
    assert_eq!(download.strategy.reprobe_at(), None, "a gain: ramping");
    assert_eq!(download.strategy.want(), 8);
    assert_eq!(download.manager.get_active_runners_count(), 8);
    assert_covered(&download);
    drop(download);
    file_writer.finalize().await.unwrap();
}

/// A runner of a hold of two connects in the tick that reaches the probe
/// time, after a refill replaced one that finished two ticks before, and
/// delivers a little of it (design §14.33). The tick does not measure the two: the probe
/// waits a tick, starts from the two at two MiB a tick, and with the
/// bandwidth unchanged four gain nothing: a hold of two again, its wait
/// doubled to 60 s. Started at the tick with the new runner, it would
/// have compared the four with about one MiB a tick and ramped on.
#[tokio::test]
async fn a_probe_waits_out_a_tick_in_which_a_runner_connected() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, judged, file_writer) = held_at_two(&rt, two_mib_a_tick).await;
    complete_the_smallest(&mut download);
    complete_the_smallest(&mut download);
    let probe = judged + Duration::from_secs(30);
    let mut now = judged;
    while now + TICK * 3 < probe {
        now += TICK;
        deliver_reading(&mut download, two_mib_a_tick);
        download.on_strategy_tick(now);
        assert_eq!(download.manager.get_active_runners_count(), 2, "{now:?}");
    }
    // One finishes; the hold refills its slot by a split, 2 s before the
    // probe time or more, so the probe is not held back by the interval.
    deliver_reading(&mut download, two_mib_a_tick);
    complete_the_smallest(&mut download);
    download.on_strategy_tick(probe - TICK * 2);
    assert_eq!(download.manager.get_active_runners_count(), 2);
    let new = runners(&download, ChunkStatus::Pending);
    assert_eq!(new.len(), 1);
    // Still connecting a tick later.
    let old = runners(&download, ChunkStatus::Running);
    for &runner_id in &old {
        download.manager.update_progress(runner_id, MIB).unwrap();
    }
    download.on_strategy_tick(probe - TICK);
    assert_eq!(download.manager.get_active_runners_count(), 2);

    // It connects and delivers 64 KiB of the tick, the other a MiB.
    deliver_by_runner(&mut download, |id| {
        if new.contains(&id) { 64 << 10 } else { MIB }
    });
    download.on_strategy_tick(probe);
    assert_eq!(download.manager.get_active_runners_count(), 2);
    assert_eq!(download.strategy.want(), 2);

    deliver_reading(&mut download, two_mib_a_tick);
    download.on_strategy_tick(probe + TICK);
    assert_eq!(download.manager.get_active_runners_count(), 4);
    assert_eq!(download.strategy.want(), 4);
    let verdict = probe + TICK * 3;
    for n in 2..=3 {
        deliver_reading(&mut download, two_mib_a_tick);
        download.on_strategy_tick(probe + TICK * n);
    }
    assert_eq!(download.strategy.want(), 2, "no gain: a hold of two");
    assert_eq!(
        download.strategy.reprobe_at(),
        Some(verdict + Duration::from_secs(60))
    );
    assert_eq!(download.manager.get_active_runners_count(), 4);
    assert_covered(&download);
    drop(download);
    file_writer.finalize().await.unwrap();
}

/// A runner that comes and goes between two ticks, on 8 MiB split off
/// the end of a chunk, leaves the same four runners at both ends of the
/// tick that would judge the doubling from two to four, and its bytes in
/// that tick's speed (design §14.33). The tick judges nothing; the next
/// one finds no gain and holds two. Judged at the first, the eight extra
/// MiB would have read as a gain and the ramp would have doubled to
/// eight.
#[tokio::test]
async fn a_runner_that_came_and_went_defers_the_verdict() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, file_writer) = growth_loop(&rt, 8).await;
    let t0 = Instant::now();
    download.on_strategy_tick(t0);
    for n in 1..=4 {
        deliver_reading(&mut download, two_mib_a_tick);
        download.on_strategy_tick(t0 + TICK * n);
    }
    assert_eq!(download.manager.get_active_runners_count(), 4);
    assert_eq!(download.strategy.want(), 8);

    // Runner 0's last 8 MiB, run and finished by another runner.
    let state = download.manager.get_runner_state(0).unwrap();
    let end = state.allocated.end;
    let remaining = end - state.downloaded.end;
    download
        .manager
        .resize_runner(0, remaining - 8 * MIB)
        .unwrap();
    let transient = download
        .manager
        .allocate_runner_with_chunk(end - 8 * MIB..end)
        .unwrap();
    download
        .manager
        .update_progress(transient.runner_id, 8 * MIB)
        .unwrap();
    download
        .manager
        .release_runner_full(transient.runner_id, true)
        .unwrap();
    deliver_reading(&mut download, two_mib_a_tick);
    download.on_strategy_tick(t0 + TICK * 5);
    assert_eq!(download.manager.get_active_runners_count(), 4);
    assert_eq!(download.strategy.want(), 8, "not judged yet");
    assert_eq!(download.strategy.reprobe_at(), None);

    deliver_reading(&mut download, two_mib_a_tick);
    let judged = t0 + TICK * 6;
    download.on_strategy_tick(judged);
    assert_eq!(download.manager.get_active_runners_count(), 4);
    assert_eq!(download.strategy.want(), 2, "no gain: a hold of two");
    assert_eq!(
        download.strategy.reprobe_at(),
        Some(judged + Duration::from_secs(30))
    );
    assert_covered(&download);
    drop(download);
    file_writer.finalize().await.unwrap();
}

/// A hold whose runners change all the time from its probe time on:
/// every other tick one of its two fails, and a runner is created for its
/// range and connects by the next tick (design §14.33). No tick
/// measures the two, so the probe is not made; a growth deadline after
/// the first due tick it is inconclusive, and the next is due the same
/// 30 s later. Nothing is added above the two meanwhile, and the strategy
/// decides once a tick. Once the churn stops, the next due time probes.
#[tokio::test]
async fn churn_defers_a_probe_for_a_bounded_time() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, judged, file_writer) = held_at_two(&rt, two_mib_a_tick).await;
    complete_the_smallest(&mut download);
    complete_the_smallest(&mut download);
    let probe = judged + Duration::from_secs(30);
    let mut now = judged;
    while now + TICK < probe {
        now += TICK;
        deliver_reading(&mut download, two_mib_a_tick);
        download.on_strategy_tick(now);
    }
    let deadline = growth_deadline(crate::runner::DEFAULT_CONNECT_TIMEOUT);
    let mut inconclusive_at = None;
    let mut now = probe;
    for n in 0.. {
        deliver_reading(&mut download, two_mib_a_tick);
        if n % 2 == 0 {
            let victim = runners(&download, ChunkStatus::Running)[0];
            download.manager.release_runner_full(victim, false).unwrap();
        }
        download.on_strategy_tick(now);
        assert!(download.manager.get_active_runners_count() <= 2, "{now:?}");
        assert_eq!(download.strategy.want(), 2);
        if download.strategy.reprobe_at() != Some(probe) {
            inconclusive_at = Some(now);
            break;
        }
        assert!(now < probe + deadline + TICK * 2, "no end by {now:?}");
        now += TICK;
    }
    // Inconclusive within a tick or two of the deadline, the wait
    // unchanged.
    let inconclusive_at = inconclusive_at.unwrap();
    assert!(inconclusive_at >= probe + deadline, "{inconclusive_at:?}");
    assert_eq!(
        download.strategy.reprobe_at(),
        Some(inconclusive_at + BASE_REPROBE_WAIT)
    );

    // The churn stops: the probe comes at the next due time.
    let next = inconclusive_at + BASE_REPROBE_WAIT;
    loop {
        now += TICK;
        deliver_reading(&mut download, two_mib_a_tick);
        download.on_strategy_tick(now);
        if now >= next {
            break;
        }
        assert_eq!(download.manager.get_active_runners_count(), 2, "{now:?}");
    }
    assert_eq!(download.manager.get_active_runners_count(), 4);
    assert_eq!(download.strategy.want(), 4);
    assert_covered(&download);
    drop(download);
    file_writer.finalize().await.unwrap();
}

/// A search for a tick to probe from ends at its deadline even when the
/// limit controller acts at every tick after it began, so the doubling
/// strategy never steps (design §14.33): before each tick a runner fails,
/// and the controller creates a runner for its range. The probe is
/// inconclusive at the first tick at or past the deadline, the wait
/// unchanged; nothing is added above the two.
#[tokio::test]
async fn controller_actions_do_not_hold_off_the_end_of_a_search() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, judged, file_writer) = held_at_two(&rt, two_mib_a_tick).await;
    complete_the_smallest(&mut download);
    complete_the_smallest(&mut download);
    let probe = judged + Duration::from_secs(30);
    let fail_one = |download: &mut DownloadLoop| {
        let victim = runners(download, ChunkStatus::Running)[0];
        download.manager.release_runner_full(victim, false).unwrap();
    };
    let mut now = judged;
    while now + TICK < probe {
        now += TICK;
        deliver_reading(&mut download, two_mib_a_tick);
        if now + TICK >= probe {
            // The controller replaces it, at the tick before the probe's.
            fail_one(&mut download);
        }
        download.on_strategy_tick(now);
        assert_eq!(download.manager.get_active_runners_count(), 2);
    }
    // The replacement connects in the probe's tick: no tick measured the
    // two, and the search begins.
    deliver_reading(&mut download, two_mib_a_tick);
    download.on_strategy_tick(probe);
    assert_eq!(download.manager.get_active_runners_count(), 2);
    assert_eq!(download.strategy.reprobe_at(), Some(probe));

    let deadline = growth_deadline(crate::runner::DEFAULT_CONNECT_TIMEOUT);
    let creates = |download: &DownloadLoop| {
        download
            .applied
            .iter()
            .filter(|action| action.starts_with("CreateTask"))
            .count()
    };
    let mut now = probe;
    let ended = loop {
        now += TICK;
        assert!(now < probe + deadline + TICK, "no end by {now:?}");
        deliver_reading(&mut download, two_mib_a_tick);
        fail_one(&mut download);
        let before = creates(&download);
        download.on_strategy_tick(now);
        assert_eq!(creates(&download), before + 1, "the controller acts");
        assert_eq!(download.manager.get_active_runners_count(), 2);
        assert_eq!(download.strategy.want(), 2);
        if download.strategy.reprobe_at() != Some(probe) {
            break now;
        }
    };
    assert!(ended >= probe + deadline, "{ended:?}");
    assert_eq!(
        download.strategy.reprobe_at(),
        Some(ended + BASE_REPROBE_WAIT)
    );
    assert_covered(&download);
    drop(download);
    file_writer.finalize().await.unwrap();
}

/// A hold creates runners for unassigned ranges only up to its runners
/// (design §14.33): two of the four of a gainless doubling fail and leave
/// their ranges unassigned, which wait while the two the hold wants run;
/// once one of those fails too, one range is claimed, without a split.
#[tokio::test]
async fn a_hold_leaves_unassigned_ranges_above_its_runners() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, judged, file_writer) = held_at_two(&rt, two_mib_a_tick).await;
    let failed = runners(&download, ChunkStatus::Running);
    for &runner_id in &failed[2..] {
        download
            .manager
            .release_runner_full(runner_id, false)
            .unwrap();
    }
    let mut now = judged;
    for _ in 0..8 {
        now += TICK;
        deliver_reading(&mut download, two_mib_a_tick);
        download.on_strategy_tick(now);
        assert_eq!(download.manager.get_active_runners_count(), 2);
    }
    let unassigned = download.manager.get_eligible_ranges(now);
    assert_eq!(unassigned.len(), 2);

    download
        .manager
        .release_runner_full(failed[0], false)
        .unwrap();
    now += TICK;
    deliver_reading(&mut download, two_mib_a_tick);
    download.on_strategy_tick(now);
    assert_eq!(download.manager.get_active_runners_count(), 2);
    let eligible = download.manager.get_eligible_ranges(now);
    assert_eq!(eligible.len(), 2, "{eligible:?}");
    assert_eq!(
        download
            .applied
            .iter()
            .filter(|action| action.starts_with("Grow"))
            .count(),
        2,
        "the doublings to two and four"
    );
    assert_covered(&download);
    drop(download);
    file_writer.finalize().await.unwrap();
}

/// A hold of eight with the limit lowered to six by a 503 asks the limit
/// controller for steps (design §8.3, §14.33): each step's trial is a
/// split of the hold's refill, and the limit climbs to the hold's eight
/// and no further, though `max_connections` is 16.
#[tokio::test]
async fn a_hold_above_the_limit_climbs_by_steps() {
    const RUNNERS: u64 = 6;
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, file_writer) = growth_loop(&rt, 16).await;
    let chunk = TOTAL / RUNNERS;
    let _slots: Vec<_> = (0..RUNNERS)
        .map(|i| {
            let end = if i + 1 == RUNNERS {
                TOTAL
            } else {
                (i + 1) * chunk
            };
            download
                .manager
                .allocate_runner_with_chunk(i * chunk..end)
                .unwrap()
        })
        .collect();
    for id in 0..RUNNERS as usize {
        download.manager.admission_mut().on_started(id, id);
    }
    download
        .manager
        .admission_mut()
        .on_attempt_ended(UNSEEN, AdmissionClass::Congestion, 6, 6);
    let t0 = Instant::now();
    download.on_strategy_tick(t0);
    deliver_each(&mut download, |_| 64 << 10);
    download.on_strategy_tick(t0 + TICK);
    assert_eq!(download.max_concurrency, 6);
    download.strategy.hold(8, t0 + Duration::from_secs(3600));
    assert_eq!(download.strategy.want(), 8);

    let mut limits = vec![download.max_concurrency];
    for n in 2..202 {
        deliver_each(&mut download, |_| 64 << 10);
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
    assert_eq!(limits, [6, 7, 8]);
    assert_eq!(download.strategy.limit_state().0, 8);
    assert_eq!(download.manager.get_active_runners_count(), 8);
    assert_covered(&download);
    drop(download);
    file_writer.finalize().await.unwrap();
}

/// Under `Doubling` with `max_connections`, grow 1, 2, 4, 8 through the
/// real split, each runner 1 MiB a tick; then one of the eight completes
/// and stops before the tick that would judge the doubling to eight.
/// Returns the loop, the time of that tick, and the writer.
async fn eight_then_one_completes(
    rt: &ScriptedRuntime,
    max_connections: usize,
) -> (DownloadLoop, Instant, FileWriter) {
    let (mut download, file_writer) = growth_loop(rt, max_connections).await;
    let t0 = Instant::now();
    download.on_strategy_tick(t0);
    for n in 1..=5 {
        deliver(&mut download, MIB);
        download.on_strategy_tick(t0 + TICK * n);
    }
    assert_eq!(download.manager.get_active_runners_count(), 8);
    // Too early to judge the doubling to eight.
    deliver(&mut download, MIB);
    download.on_strategy_tick(t0 + TICK * 6);
    assert_eq!(download.manager.get_active_runners_count(), 8);
    // The smallest chunk completes; much is left in the others.
    let running = runners(&download, ChunkStatus::Running);
    let smallest = *running
        .iter()
        .min_by_key(|&&id| download.manager.get_runner_state(id).unwrap().remaining())
        .unwrap();
    download
        .manager
        .update_progress(smallest, u64::MAX)
        .unwrap();
    download
        .manager
        .release_runner_full(smallest, true)
        .unwrap();
    assert_eq!(download.manager.get_active_runners_count(), 7);
    assert!(download.manager.get_available_ranges().is_empty());
    (download, t0 + TICK * 7, file_writer)
}

/// A doubling below the limit that loses a runner before it is judged
/// gets no verdict, and the growth goes on (design §14.18): nothing is
/// left for a new runner to claim, so only a split brings the slot back.
#[tokio::test]
async fn doubling_below_the_limit_grows_on_after_a_runner_completes() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, judging, file_writer) = eight_then_one_completes(&rt, 16).await;
    deliver(&mut download, MIB);
    download.on_strategy_tick(judging);
    assert_eq!(download.manager.get_active_runners_count(), 14);
    assert_covered(&download);
    drop(download);
    file_writer.finalize().await.unwrap();
}

/// A doubling to the limit that loses a runner before it is judged holds
/// the growth: the runner is not replaced, however long the download
/// goes on (design §14.18).
#[tokio::test]
async fn doubling_to_the_limit_does_not_refill_before_it_is_judged() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, judging, file_writer) = eight_then_one_completes(&rt, 8).await;
    for n in 0..10 {
        deliver(&mut download, MIB);
        download.on_strategy_tick(judging + TICK * n);
        assert_eq!(download.manager.get_active_runners_count(), 7, "tick {n}");
    }
    assert_covered(&download);
    drop(download);
    file_writer.finalize().await.unwrap();
}

/// A doubling below the limit is not held by a runner that stalls with a
/// chunk too small for the silent restart, while the others still have
/// plenty to split, so the hedge waits too (design §9.6, §14.19, M1): the
/// tick lets the silent runner off and drops the doubling it cannot
/// measure, and the growth goes on.
#[tokio::test]
async fn a_small_stalled_chunk_does_not_freeze_the_growth() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, file_writer) = growth_loop(&rt, 8).await;
    let t0 = Instant::now();
    // Start the connecting runners; every running one but `stalled`
    // delivers a MiB, noted at `now`.
    let feed = |download: &mut DownloadLoop, stalled: Option<crate::task::RunnerId>, now| {
        for runner_id in runners(download, ChunkStatus::Pending) {
            download.manager.mark_started(runner_id);
        }
        for runner_id in runners(download, ChunkStatus::Running) {
            if Some(runner_id) != stalled {
                download.manager.update_progress(runner_id, MIB).unwrap();
                download.manager.note_delivery(runner_id, now);
                download
                    .manager
                    .read_activity(runner_id)
                    .unwrap()
                    .note_at(now);
            }
        }
    };
    download.on_strategy_tick(t0);
    for n in 1..=3 {
        feed(&mut download, None, t0 + TICK * n);
        download.on_strategy_tick(t0 + TICK * n);
    }
    // 1, 2, then 4 at 4.5 s: a doubling below the limit, to be judged.
    assert_eq!(download.manager.get_active_runners_count(), 4);

    // Runner 0 gets to a MiB short of its end and stalls there.
    let left = download.manager.get_runner_state(0).unwrap().remaining();
    download.manager.update_progress(0, left - MIB).unwrap();
    feed(&mut download, Some(0), t0 + TICK * 4);
    download.on_strategy_tick(t0 + TICK * 4);
    assert_eq!(download.manager.get_active_runners_count(), 4, "too early");
    feed(&mut download, Some(0), t0 + TICK * 5);
    download.on_strategy_tick(t0 + TICK * 5);
    assert!(download.rates.tick().silent);
    assert_eq!(download.manager.get_active_runners_count(), 8);
    assert!(!download.manager.is_stopping(0), "left to the hedge");
    assert_covered(&download);
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
    let (mut download, file_writer) = growth_loop(&rt, 4).await;
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
        let (mut download, file_writer) = growth_loop(&rt, 8).await;
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

/// The default options grow by doubling (design §14.20): a tick with every
/// runner delivering, past the grow interval, asks to double the one
/// runner.
#[tokio::test]
async fn doubling_is_the_default() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, file_writer) = loop_with_options(
        ThreadedRuntimeImpl::new_other_rt(rt.clone()),
        DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(1).with_range_support(true)),
        TOTAL,
        &TaskOptions::default(),
    )
    .await;
    let _slot = download
        .manager
        .allocate_runner_with_chunk(0..TOTAL)
        .unwrap();
    let tick = super::super::super::strategy::TickSample {
        speed: Some(MIB as f64),
        all_delivering: true,
        silent: false,
        stable: true,
        rates: Default::default(),
    };
    let now = Instant::now();
    let signals = download.signals(now);
    let actions = download
        .strategy
        .execute(
            download.max_concurrency,
            &mut download.manager,
            now,
            signals,
            &tick,
        )
        .map(|(name, actions)| (name, format!("{actions:?}")));
    assert_eq!(
        actions,
        Some(("doubling", format!("{:?}", [StrategyAction::Grow(1)])))
    );
    drop(download);
    file_writer.finalize().await.unwrap();
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
        let (mut download, file_writer) = growth_loop(&rt, 8).await;
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

/// Growth respects the limit controller's cap (design §8.3, §14.14): a
/// recovery step allows one runner above the limit, but once a refusal
/// left the step unclear, a late tick, which does not decide it, lets no
/// growth take the trial's slot.
#[tokio::test]
async fn doubling_opens_no_trial_for_a_marked_step() {
    const RUNNERS: u64 = 4;
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, file_writer) = growth_loop(&rt, 8).await;
    let chunk = TOTAL / RUNNERS;
    let _slots: Vec<_> = (0..RUNNERS)
        .map(|i| {
            download
                .manager
                .allocate_runner_with_chunk(i * chunk..(i + 1) * chunk)
                .unwrap()
        })
        .collect();
    for id in 0..RUNNERS as usize {
        download.manager.admission_mut().on_started(id, id);
    }
    download
        .manager
        .admission_mut()
        .on_attempt_ended(UNSEEN, AdmissionClass::Congestion, 4, 4);
    let t0 = Instant::now();
    download.on_strategy_tick(t0);
    deliver(&mut download, MIB);
    download.on_strategy_tick(t0 + TICK);
    assert_eq!(download.max_concurrency, 4);
    // Ticks up to the step, 30 s after the lowering.
    let mut now = t0 + TICK;
    while download.strategy.limit_state().1.is_none() {
        now += TICK;
        deliver(&mut download, MIB);
        download.on_strategy_tick(now);
        assert!(now < t0 + Duration::from_secs(40), "no step began");
    }
    assert_eq!(download.strategy.cap(), 5);
    // An uncorroborated refusal marks it unclear.
    download
        .manager
        .admission_mut()
        .on_attempt_ended(UNSEEN, AdmissionClass::Refused, 0, 4);
    let tick = super::super::super::strategy::TickSample {
        speed: Some(4.0 * MIB as f64),
        all_delivering: true,
        silent: false,
        stable: true,
        rates: Default::default(),
    };
    let late = now + 3 * TICK;
    deliver(&mut download, MIB);
    let signals = download.signals(late);
    let acted = download.strategy.execute(
        download.max_concurrency,
        &mut download.manager,
        late,
        signals,
        &tick,
    );
    assert!(acted.is_none(), "{acted:?}");
    assert!(
        download.strategy.limit_state().1.is_some(),
        "the late tick decides nothing"
    );
    drop(download);
    file_writer.finalize().await.unwrap();
}
