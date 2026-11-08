//! Backoff of failed ranges, `Retry-After` and the failure budget of a
//! range in the loop (design §8.1, §8.2).
//!
//! Every step gets its time passed in, except in two tests that wait for
//! real time: `loop_retries_after_the_backoff_without_a_tick` waits for the
//! loop's own timer, and `retry_after_holds_back_the_requests_to_the_server`
//! for `Retry-After` at a real server.

use std::{num::NonZeroU32, ops::Range, pin::pin, sync::Arc, task::Context};

use async_ringbuf::{
    AsyncHeapRb,
    traits::{Producer, Split},
};
use bolt_load_tests::adapter::{
    fault::{FaultKind, FaultPlan},
    simple::SimpleTestAdapterBuilder,
};
use pretty_assertions::assert_eq;

use super::{
    super::super::{
        chunk_planner::Cooldown,
        runner_manager::RunnerChannels,
        strategy::{MIN_RANGE_BACKOFF, StrategyAction},
    },
    *,
};
use crate::{
    adapter::UnretryableError,
    runner::{
        DATA_FRAME_CHANNEL_CAPACITY, DataFrame, DataFrameSender, LIFECYCLE_CHANNEL_CAPACITY,
        LifecycleEvent, LifecycleSender, LimitReceiver,
    },
};

const TOTAL: u64 = 1 << 30;
const HALF: u64 = TOTAL / 2;
const SECOND: Duration = Duration::from_secs(1);
const NANO: Duration = Duration::from_nanos(1);

/// A loop on `rt`, whose runners are parked and never connect.
async fn scripted_loop(rt: &ScriptedRuntime, max_connections: usize) -> (DownloadLoop, FileWriter) {
    connecting_loop(
        ThreadedRuntimeImpl::new_other_rt(rt.clone()),
        DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(1).with_range_support(true)),
        TOTAL,
        max_connections,
    )
    .await
}

/// The ends of a runner the test drives.
struct TestRunner {
    lifecycle: LifecycleSender,
    _data: DataFrameSender,
    _limits: LimitReceiver,
}

/// A runner on `range` whose lifecycle the test drives.
fn spawn_test_runner(download: &mut DownloadLoop, range: Range<u64>) -> TestRunner {
    let (lifecycle, lifecycle_rx) =
        AsyncHeapRb::<LifecycleEvent>::new(LIFECYCLE_CHANNEL_CAPACITY).split();
    let (data, data_rx) = AsyncHeapRb::<DataFrame>::new(DATA_FRAME_CHANNEL_CAPACITY).split();
    let mut limits = None;
    download
        .manager
        .spawn_runner_with_chunk(range, |_, runner_limits| {
            limits = Some(runner_limits);
            Ok(RunnerChannels {
                lifecycle_rx,
                data_rx,
                stop: Default::default(),

                reads: Default::default(),
            })
        })
        .unwrap();
    TestRunner {
        lifecycle,
        _data: data,
        _limits: limits.unwrap(),
    }
}

fn congestion(retry_after: Option<Duration>) -> TaskError {
    TaskError::StreamError {
        source: AdapterError::Unretryable {
            source: UnretryableError::Congestion {
                status: 503,
                retry_after,
            },
        },
    }
}

/// The only cooldown.
fn single_cooldown(download: &DownloadLoop) -> (Range<u64>, Cooldown) {
    let cooldowns = download.manager.cooldowns();
    let [cooldown] = cooldowns.as_slice() else {
        panic!("expected one cooldown, got {cooldowns:?}");
    };
    cooldown.clone()
}

/// A failed range is claimed again neither by a tick nor by the wake-up
/// before its backoff ends, and by both after; the claim counts on from the
/// failures.
#[tokio::test]
async fn failed_range_waits_for_its_backoff() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, file_writer) = scripted_loop(&rt, 4).await;
    let t0 = Instant::now();
    // The first tick creates a runner, which is lost while connecting.
    download.on_strategy_tick(t0);
    assert_eq!(download.manager.allocated_runner_count(), 1);
    drop(rt.take_parked());
    let tick = download.manager.tick().await;
    assert!(download.on_runner(tick, t0).unwrap().is_continue());

    let (range, cooldown) = single_cooldown(&download);
    assert_eq!((range, cooldown.attempts), (0..TOTAL, 1));
    let end = cooldown.not_before;
    assert!(
        t0 + MIN_RANGE_BACKOFF <= end && end <= t0 + 2 * MIN_RANGE_BACKOFF,
        "backs off for {:?}",
        end - t0
    );
    // The loop wakes when it ends.
    assert_eq!(download.next_retry(t0), Some(end));
    assert_eq!(download.deadline.at(), end);

    download.on_strategy_tick(end - NANO);
    download.on_deadline(end - NANO).unwrap();
    assert_eq!(download.manager.allocated_runner_count(), 0);
    assert_eq!(download.deadline.at(), end, "re-armed for the same end");

    // The wake-up claims it, counting the failure.
    download.on_deadline(end).unwrap();
    assert_eq!(download.manager.allocated_runner_count(), 1);
    let state = download.manager.get_runner_state(0).unwrap();
    assert_eq!((state.allocated.clone(), state.attempts), (0..TOTAL, 1));
    // Nothing else waits.
    assert_eq!(download.next_retry(end), None);
    assert_eq!(download.deadline.at(), download.policy.deadline());

    // The second failure backs off for twice as long; a tick claims it.
    drop(rt.take_parked());
    let tick = download.manager.tick().await;
    assert!(download.on_runner(tick, end).unwrap().is_continue());
    let (_, cooldown) = single_cooldown(&download);
    assert_eq!(cooldown.attempts, 2);
    let second_end = cooldown.not_before;
    assert!(
        end + 2 * MIN_RANGE_BACKOFF <= second_end && second_end <= end + 4 * MIN_RANGE_BACKOFF,
        "backs off for {:?}",
        second_end - end
    );
    download.on_strategy_tick(second_end - NANO);
    assert_eq!(download.manager.allocated_runner_count(), 0);
    download.on_strategy_tick(second_end);
    assert_eq!(download.manager.get_runner_state(0).unwrap().attempts, 2);

    drop(download);
    file_writer.finalize().await.unwrap();
}

/// A loop with runner 0 running on the first half, and the runner on the
/// second half rejected at the returned time with a 503 and `retry_after`.
async fn congested_loop(
    rt: &ScriptedRuntime,
    retry_after: Option<Duration>,
) -> (DownloadLoop, FileWriter, TestRunner, Instant) {
    let (mut download, file_writer) = scripted_loop(rt, 4).await;
    let mut running = spawn_test_runner(&mut download, 0..HALF);
    let mut rejected = spawn_test_runner(&mut download, HALF..TOTAL);
    // The first tick has no range to create a runner for.
    download.on_strategy_tick(Instant::now());
    running.lifecycle.try_push(LifecycleEvent::Started).unwrap();
    download.manager.tick().await;
    rejected
        .lifecycle
        .try_push(LifecycleEvent::failed(congestion(retry_after)))
        .unwrap();
    let tick = download.manager.tick().await;
    let t0 = Instant::now();
    // Runner 0 is in flight, so the task goes on.
    assert!(download.on_runner(tick, t0).unwrap().is_continue());
    (download, file_writer, running, t0)
}

/// With `Retry-After: 2`, no new connection opens within 2 s: no runner is
/// created and none is split, whatever asks. One may once they passed.
#[tokio::test]
async fn retry_after_holds_back_every_new_connection() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, file_writer, _running, t0) = congested_loop(&rt, Some(2 * SECOND)).await;
    let until = t0 + 2 * SECOND;
    assert_eq!(download.backoff.congestion_until(), Some(until));
    assert_eq!(
        download.manager.cooldowns(),
        [(
            HALF..TOTAL,
            Cooldown {
                end: TOTAL,
                attempts: 1,
                // A congestion is not charged to the range.
                charged: 0,
                not_before: until
            }
        )]
    );
    assert_eq!(download.next_retry(t0), Some(until));
    assert_eq!(download.deadline.at(), until);

    for now in [t0, t0 + SECOND, until - NANO] {
        download.apply(StrategyAction::CreateTask(HALF..TOTAL), now);
        let incomplete = download
            .manager
            .get_runner_state(0)
            .unwrap()
            .incomplete_range();
        assert!(!download.split(0, incomplete, now));
        // At 2 MiB/s growth would split.
        download.throughput.per_runner_speed = 2.0 * 1024.0 * 1024.0;
        download.apply(StrategyAction::Grow(2), now);
        download.on_strategy_tick(now);
        download.on_deadline(now).unwrap();
        assert_eq!(download.manager.allocated_runner_count(), 1, "at {now:?}");
        assert_eq!(
            download.manager.get_runner_state(0).unwrap().allocated,
            0..HALF
        );
        assert!(rt.take_parked().is_empty());
        assert_eq!(download.deadline.at(), until);
    }

    let incomplete = download
        .manager
        .get_runner_state(0)
        .unwrap()
        .incomplete_range();
    assert!(download.split(0, incomplete, until));
    assert!(download.manager.get_runner_state(0).unwrap().allocated.end < HALF);
    download.apply(StrategyAction::CreateTask(HALF..TOTAL), until);
    assert_eq!(download.manager.allocated_runner_count(), 3);
    assert_eq!(rt.take_parked().len(), 2);

    drop(download);
    file_writer.finalize().await.unwrap();
}

/// Once the congestion ends, the wake-up creates a runner for the rejected
/// range without waiting for a tick.
#[tokio::test]
async fn wake_up_creates_runners_once_the_congestion_ends() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, file_writer, _running, t0) = congested_loop(&rt, Some(2 * SECOND)).await;
    let until = t0 + 2 * SECOND;
    // The next tick degrades to the one running runner, which completes.
    download.on_strategy_tick(t0 + SECOND);
    assert_eq!(download.max_concurrency, 1);
    download.manager.update_progress(0, HALF).unwrap();
    download.manager.release_runner_full(0, true).unwrap();

    download.on_deadline(until - NANO).unwrap();
    assert_eq!(download.manager.allocated_runner_count(), 0);
    assert_eq!(download.deadline.at(), until);

    download.on_deadline(until).unwrap();
    let state = download.manager.get_runner_state(0).unwrap();
    assert_eq!((state.allocated.clone(), state.attempts), (HALF..TOTAL, 1));
    assert_eq!(rt.take_parked().len(), 1);
    assert_eq!(download.deadline.at(), download.policy.deadline());

    drop(download);
    file_writer.finalize().await.unwrap();
}

/// The known limit of the wake-up (design §14.12): a 503 without
/// `Retry-After` backs off for less than a tick, so its backoff ends before
/// the next step degrades the concurrency. The wake-up then creates
/// nothing, although a slot is free, and sleeps until the no-progress
/// deadline instead of waking again; the next tick only degrades, and the
/// tick after it creates the runner.
#[tokio::test]
async fn wake_up_before_the_degradation_leaves_the_retry_to_a_later_tick() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, file_writer, _running, t0) = congested_loop(&rt, None).await;
    let (_, cooldown) = single_cooldown(&download);
    let end = cooldown.not_before;
    assert!(
        t0 + MIN_RANGE_BACKOFF <= end && end <= t0 + 2 * MIN_RANGE_BACKOFF,
        "backs off for {:?}",
        end - t0
    );
    assert_eq!(download.backoff.congestion_until(), Some(end));
    assert_eq!(download.deadline.at(), end);
    // The running runner completes: a slot is free.
    download.manager.update_progress(0, HALF).unwrap();
    download.manager.release_runner_full(0, true).unwrap();

    // The degradation is pending, so the wake-up creates nothing, and
    // nothing else waits.
    download.on_deadline(end).unwrap();
    assert_eq!(download.manager.allocated_runner_count(), 0);
    assert!(rt.take_parked().is_empty());
    assert_eq!(download.next_retry(end), None);
    assert_eq!(download.deadline.at(), download.policy.deadline());
    assert!(download.deadline.at() > end, "the loop would wake again");

    // The next tick only degrades.
    let tick = t0 + DEFAULT_STRATEGY_TICK_INTERVAL;
    download.on_strategy_tick(tick);
    assert_eq!(download.max_concurrency, 1);
    assert_eq!(download.manager.allocated_runner_count(), 0);
    assert!(rt.take_parked().is_empty());

    // The tick after it creates the runner.
    download.on_strategy_tick(tick + DEFAULT_STRATEGY_TICK_INTERVAL);
    let state = download.manager.get_runner_state(0).unwrap();
    assert_eq!((state.allocated.clone(), state.attempts), (HALF..TOTAL, 1));
    assert_eq!(rt.take_parked().len(), 1);

    drop(download);
    file_writer.finalize().await.unwrap();
}

/// A backoff that ends while every slot is taken re-arms the loop for the
/// no-progress deadline instead of spinning; a later tick retries it.
#[tokio::test]
async fn wake_up_without_a_free_slot_does_not_spin() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let (mut download, file_writer) = scripted_loop(&rt, 2).await;
    let mut running = spawn_test_runner(&mut download, 0..HALF);
    let mut failing = spawn_test_runner(&mut download, HALF..TOTAL);
    download.on_strategy_tick(Instant::now());
    running.lifecycle.try_push(LifecycleEvent::Started).unwrap();
    download.manager.tick().await;
    failing
        .lifecycle
        .try_push(LifecycleEvent::failed(TaskError::Timeout))
        .unwrap();
    let tick = download.manager.tick().await;
    let t0 = Instant::now();
    assert!(download.on_runner(tick, t0).unwrap().is_continue());
    let end = single_cooldown(&download).1.not_before;
    assert_eq!(download.deadline.at(), end);

    // The freed slot is taken before the backoff ends.
    let taken = download.manager.try_reserve_slot().unwrap();
    download.on_deadline(end).unwrap();
    assert_eq!(download.manager.allocated_runner_count(), 2);
    assert!(rt.take_parked().is_empty());
    assert_eq!(download.next_retry(end), None);
    assert_eq!(download.deadline.at(), download.policy.deadline());

    download.manager.release(taken);
    download.on_strategy_tick(end + SECOND);
    assert_eq!(
        download.manager.get_runner_state(1).unwrap().allocated,
        HALF..TOTAL
    );

    drop(download);
    file_writer.finalize().await.unwrap();
}

/// Run by the loop itself: a lost runner's range is retried once its
/// backoff ends, with no strategy tick.
#[test]
fn loop_retries_after_the_backoff_without_a_tick() {
    on_each_executor(|| async {
        let rt = ScriptedRuntime::new(TimerKind::Scripted);
        let (mut download, file_writer) = scripted_loop(&rt, 4).await;
        download.on_strategy_tick(Instant::now());
        drop(rt.take_parked());
        let lost_at = Instant::now();

        let wakes = Arc::new(WakeCount::default());
        let waker = futures::task::waker(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        {
            let mut run = pin!(download.run());
            loop {
                let seen = wakes.get();
                assert!(run.as_mut().poll(&mut cx).is_pending());
                if !rt.parked.lock().unwrap().is_empty() {
                    break;
                }
                wakes.wait_past(seen);
            }
        }
        let elapsed = lost_at.elapsed();
        assert!(elapsed >= MIN_RANGE_BACKOFF, "retried after {elapsed:?}");
        assert!(rt.script.log().is_empty(), "no strategy tick");
        assert_eq!(download.manager.get_runner_state(0).unwrap().attempts, 1);

        drop(rt.take_parked());
        drop(download);
        file_writer.finalize().await.unwrap();
    });
}

/// A loop on `rt` with `options`, whose parked runners fail to connect with
/// a retryable error once the test runs them.
async fn failing_loop(rt: &ScriptedRuntime, options: &TaskOptions) -> (DownloadLoop, FileWriter) {
    let adapter = SimpleTestAdapterBuilder::new()
        .content_size(1)
        .support_range(true)
        .range_connect_plan(FaultPlan::AlwaysFail(FaultKind::Retryable))
        .build()
        .unwrap();
    loop_with_options(
        ThreadedRuntimeImpl::new_other_rt(rt.clone()),
        DynBoltLoadAdapter::new_arc(adapter),
        TOTAL,
        options,
    )
    .await
}

/// The only runner the loop spawned since the last call.
fn single_parked(rt: &ScriptedRuntime) -> futures::future::FutureObj<'static, ()> {
    let mut parked = rt.take_parked();
    assert_eq!(parked.len(), 1, "expected one new runner");
    parked.pop().unwrap()
}

/// `runner` delivers `len` bytes, which the loop accepts at `now` and
/// writes; the write completes.
async fn deliver_and_write(
    download: &mut DownloadLoop,
    runner: &mut TestRunner,
    len: usize,
    now: Instant,
) {
    runner
        ._data
        .try_push(DataFrame {
            data: bytes::Bytes::from(vec![0; len]),
        })
        .unwrap();
    let tick = download.manager.tick().await;
    assert!(tick.downloaded.is_some());
    assert!(download.on_runner(tick, now).unwrap().is_continue());
    let completions = download.writer.tick().await.unwrap();
    download
        .record_completions(completions, false, now)
        .unwrap();
}

fn reset() -> TaskError {
    TaskError::StreamError {
        source: AdapterError::Retryable {
            source: crate::adapter::RetryableError::Io {
                source: Arc::new(std::io::Error::from(std::io::ErrorKind::ConnectionReset)),
            },
        },
    }
}

/// Run the parked retry of the failing range at `now`, once its backoff
/// ended, and return its failure, which the loop then handles.
async fn retry_and_fail(
    download: &mut DownloadLoop,
    rt: &ScriptedRuntime,
    now: &mut Instant,
) -> super::super::super::runner_manager::AttemptFailure {
    let (_, cooldown) = single_cooldown(download);
    download.on_deadline(cooldown.not_before - NANO).unwrap();
    assert_eq!(download.manager.allocated_runner_count(), 1);
    *now = cooldown.not_before;
    download.on_deadline(*now).unwrap();
    assert_eq!(download.manager.allocated_runner_count(), 2);
    single_parked(rt).await;
    let tick = download.manager.tick().await;
    let failure = tick.failure.clone().unwrap();
    assert!(download.on_runner(tick, *now).unwrap().is_continue());
    failure
}

/// G6 through the loop: both connections were proven, so the server
/// admits two, and the range of one that keeps failing is refused inside
/// the proven window. It is retried after each backoff while the other
/// runner writes between its failures, so the consecutive failures never
/// reach their limit of 2. The range's third charged failure ends the task
/// with `RangeExhausted`, which carries that failure, not an earlier one.
#[tokio::test]
async fn a_failing_range_ends_the_task_while_another_runner_writes() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let options = TaskOptions::default()
        .max_connections(NonZeroUsize::new(2).unwrap())
        .max_consecutive_failures(2)
        .max_attempts_per_range(NonZeroU32::new(3).unwrap());
    let (mut download, file_writer) = failing_loop(&rt, &options).await;
    let mut healthy = spawn_test_runner(&mut download, 0..HALF);
    let mut doomed = spawn_test_runner(&mut download, HALF..TOTAL);
    for runner in [&mut healthy, &mut doomed] {
        runner.lifecycle.try_push(LifecycleEvent::Started).unwrap();
        download.manager.tick().await;
    }
    let mut now = Instant::now();
    download.on_strategy_tick(now);
    deliver_and_write(&mut download, &mut healthy, 1024, now).await;
    deliver_and_write(&mut download, &mut doomed, 1024, now).await;
    // A tick sees both proven.
    now += DEFAULT_STRATEGY_TICK_INTERVAL;
    download.on_strategy_tick(now);
    assert_eq!(download.manager.admission().proven_window(), 2);

    // Its connection resets after the bytes: a restart, not charged.
    doomed
        .lifecycle
        .try_push(LifecycleEvent::failed(reset()))
        .unwrap();
    let tick = download.manager.tick().await;
    let failure = tick.failure.as_ref().unwrap();
    assert_eq!((failure.received, failure.charged), (1024, 0));
    assert!(download.on_runner(tick, now).unwrap().is_continue());
    let rest = HALF + 1024..TOTAL;

    for charged in 1..=2 {
        // A refused connect beside one proven runner, inside the window.
        let failure = retry_and_fail(&mut download, &rt, &mut now).await;
        assert!(failure.error.is_retryable(), "{failure:?}");
        assert_eq!(
            (
                failure.range.clone(),
                failure.attempts,
                failure.charged,
                failure.exempt
            ),
            (rest.clone(), charged + 1, charged, false)
        );
        // The other runner writes, which resets the consecutive failures.
        deliver_and_write(&mut download, &mut healthy, 1024, now).await;
        assert_eq!(single_cooldown(&download).1.charged, charged);
    }

    // The third attempt is lost instead.
    let (_, cooldown) = single_cooldown(&download);
    now = cooldown.not_before;
    download.on_deadline(now).unwrap();
    drop(single_parked(&rt));
    let tick = download.manager.tick().await;
    match download.on_runner(tick, now) {
        Err(DownloadError::Transfer { source }) => match &*source {
            TaskError::RangeExhausted {
                range,
                attempts,
                source,
            } => {
                assert_eq!((range.clone(), *attempts), (rest, 3));
                assert!(matches!(**source, TaskError::ChannelClosed), "{source:?}");
            }
            other => panic!("expected RangeExhausted, got {other:?}"),
        },
        other => panic!("expected RangeExhausted, got {other:?}"),
    }

    drop(download);
    file_writer.finalize().await.unwrap();
}

/// The old shape of G6's loop test (design §14.13): the range's runner was
/// never proven. Its first refusal comes while nothing is proven and is
/// charged; once the other runner delivered, the server admitted one, and
/// a refusal beside it is above the proven window and not charged. A loss
/// is charged as before. The range is not exhausted.
#[tokio::test]
async fn a_range_never_proven_is_exempt_above_the_window() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let options = TaskOptions::default()
        .max_connections(NonZeroUsize::new(2).unwrap())
        .max_consecutive_failures(2)
        .max_attempts_per_range(NonZeroU32::new(3).unwrap());
    let (mut download, file_writer) = failing_loop(&rt, &options).await;
    let mut healthy = spawn_test_runner(&mut download, 0..HALF);
    healthy.lifecycle.try_push(LifecycleEvent::Started).unwrap();
    download.manager.tick().await;
    // The first step creates a runner for the other half.
    let mut now = Instant::now();
    download.on_strategy_tick(now);

    // Refused while nothing is proven: charged.
    single_parked(&rt).await;
    let tick = download.manager.tick().await;
    let failure = tick.failure.clone().unwrap();
    assert_eq!((failure.charged, failure.exempt), (1, false));
    assert!(download.on_runner(tick, now).unwrap().is_continue());
    deliver_and_write(&mut download, &mut healthy, 1024, now).await;

    // Refused beside the proven runner, with one other in flight.
    for attempt in 2..=3 {
        let failure = retry_and_fail(&mut download, &rt, &mut now).await;
        assert_eq!(
            (failure.attempts, failure.charged, failure.exempt),
            (attempt, 1, true)
        );
        deliver_and_write(&mut download, &mut healthy, 1024, now).await;
    }

    // A loss is no refusal, and is charged.
    let (_, cooldown) = single_cooldown(&download);
    now = cooldown.not_before;
    download.on_deadline(now).unwrap();
    drop(single_parked(&rt));
    let tick = download.manager.tick().await;
    let failure = tick.failure.clone().unwrap();
    assert_eq!((failure.charged, failure.exempt), (2, false));
    assert!(download.on_runner(tick, now).unwrap().is_continue());

    drop(rt.take_parked());
    drop(download);
    file_writer.finalize().await.unwrap();
}

/// A timeout after the runner delivered bytes is a restart: with one
/// consecutive failure and one attempt per range allowed, the task goes on
/// and retries the rest of the range after a backoff. That retry failing
/// without data is the range's first charged failure, which ends the task.
#[tokio::test]
async fn a_timeout_after_data_restarts_the_range() {
    let rt = ScriptedRuntime::new(TimerKind::Scripted);
    let options = TaskOptions::default()
        .max_connections(NonZeroUsize::new(2).unwrap())
        .max_consecutive_failures(1)
        .max_attempts_per_range(NonZeroU32::new(1).unwrap());
    let (mut download, file_writer) = failing_loop(&rt, &options).await;
    let mut runner = spawn_test_runner(&mut download, 0..TOTAL);
    runner.lifecycle.try_push(LifecycleEvent::Started).unwrap();
    download.manager.tick().await;
    let t0 = Instant::now();
    deliver_and_write(&mut download, &mut runner, 1024, t0).await;

    runner
        .lifecycle
        .try_push(LifecycleEvent::failed(TaskError::Timeout))
        .unwrap();
    let tick = download.manager.tick().await;
    let failure = tick.failure.as_ref().unwrap();
    assert_eq!(
        (failure.received, failure.attempts, failure.charged),
        (1024, 1, 0)
    );
    assert!(download.on_runner(tick, t0).unwrap().is_continue());
    let (range, cooldown) = single_cooldown(&download);
    assert_eq!(
        (range, cooldown.attempts, cooldown.charged),
        (1024..TOTAL, 1, 0)
    );

    // The first step at the end of the backoff retries the rest.
    let end = cooldown.not_before;
    download.on_strategy_tick(end);
    let state = download.manager.get_runner_state(0).unwrap();
    assert_eq!(
        (state.allocated.clone(), state.attempts, state.charged),
        (1024..TOTAL, 1, 0)
    );
    drop(single_parked(&rt));
    let tick = download.manager.tick().await;
    assert!(matches!(
        download.on_runner(tick, end),
        Err(DownloadError::Transfer { source })
            if matches!(&*source, TaskError::RangeExhausted { range, attempts: 1, .. } if *range == (1024..TOTAL))
    ));

    drop(download);
    file_writer.finalize().await.unwrap();
}

/// Aborts the server task when dropped, also when a test panics, so that
/// no server outlives its test or holds its files open.
#[cfg(all(feature = "reqwest", feature = "tokio"))]
struct AbortOnDrop(tokio::task::JoinHandle<()>);

#[cfg(all(feature = "reqwest", feature = "tokio"))]
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Handle the loop's events one by one until `done` holds; the loop must not
/// end before.
#[cfg(all(feature = "reqwest", feature = "tokio"))]
async fn drive_until(
    download: &mut DownloadLoop,
    mut done: impl FnMut(&DownloadLoop) -> bool,
) -> Result<()> {
    while !done(download) {
        let event = std::future::poll_fn(|cx| download.poll_event(cx)).await;
        if download.handle_event(event)?.is_break() {
            return Err(DownloadError::other(
                "the loop ended before the condition held",
            ));
        }
    }
    Ok(())
}

/// The test server's 503 with `Retry-After` (T4), through the loop with
/// its real timers, the reqwest adapter and the HTTP fault server: the
/// server sees no request until `Retry-After` after the one it rejected,
/// the rejected range is retried, and the file is complete.
///
/// The order is set by the test, not raced:
/// - the server holds the first runner's body at a gate, after its headers;
/// - once that runner runs, the test splits it, so the split's request is
///   the one rejected;
/// - the test handles the loop's events itself until the loop recorded the
///   congestion, with the first runner still in flight, and only then
///   releases the gate.
///
/// A task only splits on its own when its growth's speed samples call for
/// it, which a test cannot force.
#[cfg(all(feature = "reqwest", feature = "tokio"))]
#[tokio::test(flavor = "multi_thread")]
async fn retry_after_holds_back_the_requests_to_the_server() {
    use bolt_load_adapter::reqwest::IntoReqwestAdapter;
    use bolt_load_tests::adapter::{
        fault::{CONGESTED_RETRY_AFTER_SECS, Gate, RangeFault, THROTTLED_BYTES_PER_SEC},
        http_server::create_range_fault_server_with_log,
        simple::{calculate_blake3, create_deterministic_content},
    };

    use super::super::super::chunk_planner::ChunkStatus;
    use crate::task::Progress;

    // Once released, the first runner streams its half for a second.
    const SIZE: u64 = 2 * THROTTLED_BYTES_PER_SEC;
    let dir = tempfile::TempDir::new().unwrap();
    let content = bytes::Bytes::from(create_deterministic_content(SIZE as usize));
    let gate = Gate::new();
    let (port, server, log) =
        create_range_fault_server_with_log(content.clone(), Some(gate.clone()))
            .await
            .unwrap();
    let server = AbortOnDrop(server);
    let url = reqwest::Url::parse(&format!(
        "http://127.0.0.1:{port}{}",
        RangeFault::CongestedOnce.path()
    ))
    .unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let adapter =
        DynBoltLoadAdapter::new_arc(client.into_reqwest_adapter((reqwest::Method::GET, url)));
    let path = dir.path().join("out.bin");
    let file_writer = FileWriter::new(&path, SIZE).await.unwrap();
    let spawner = RunnerSpawner::new(
        ThreadedRuntimeImpl::new_tokio_rt(),
        adapter,
        WaitGroup::new(),
        CancellationToken::new(),
        crate::runner::DEFAULT_IDLE_TIMEOUT,
    );
    let (reporter, _status) = ProgressReporter::for_test();
    let mut download = DownloadLoop::new(
        SIZE,
        spawner,
        &file_writer,
        reporter,
        &TaskOptions::default(),
        CancellationToken::new(),
    );

    // The first step creates the first runner.
    download.on_strategy_tick(Instant::now());
    let driven = with_watchdog(async {
        // It connects, and its body waits at the gate.
        drive_until(&mut download, |download| {
            download
                .manager
                .get_runner_state(0)
                .is_some_and(|state| state.status == ChunkStatus::Running)
        })
        .await?;
        let incomplete = download
            .manager
            .get_runner_state(0)
            .unwrap()
            .incomplete_range();
        download.split(0, incomplete, Instant::now());
        drive_until(&mut download, |download| {
            download.backoff.congestion_until().is_some()
        })
        .await?;
        // What was in flight when the loop handled the rejection.
        let in_flight = (
            download.manager.allocated_runner_count(),
            download
                .manager
                .get_runner_state(0)
                .map(|state| (state.status.clone(), state.downloaded.clone())),
        );
        gate.release();
        download.run().await?;
        Ok::<_, DownloadError>(in_flight)
    })
    .await;
    // Clean up before any assertion.
    let (result, in_flight) = match driven {
        Ok(in_flight) => (Ok(()), Some(in_flight)),
        Err(error) => (Err(error), None),
    };
    let mut progress = Progress::default();
    let finished = with_watchdog(download.finish(result, &mut progress)).await;
    let finalized = file_writer.finalize().await;
    drop(server);

    finished.expect("the download completes");
    finalized.unwrap();
    assert_eq!(
        in_flight,
        Some((1, Some((ChunkStatus::Running, 0..0)))),
        "only the first runner, waiting for its body, is in flight when the 503 is handled"
    );
    assert_eq!(progress.downloaded, SIZE);
    assert_eq!(
        calculate_blake3(&std::fs::read(&path).unwrap()),
        calculate_blake3(&content)
    );

    let mut requests = log.requests();
    requests.sort_by_key(|request| request.at);
    let ranges: Vec<_> = requests
        .iter()
        .map(|request| (request.range, request.status.as_u16()))
        .collect();
    let half = SIZE / 2;
    assert!(
        ranges.starts_with(&[(Some((0, SIZE - 1)), 206), (Some((half, SIZE - 1)), 503)]),
        "the split's request must be the rejected one: {requests:?}"
    );
    let rejected_at = requests[1].at;
    let retry_after = Duration::from_secs(CONGESTED_RETRY_AFTER_SECS);
    let later = &requests[2..];
    assert!(
        later
            .iter()
            .any(|request| request.range.is_some_and(|(first, _)| first == half)),
        "the rejected range was not retried: {requests:?}"
    );
    for request in later {
        let gap = request.at - rejected_at;
        assert!(
            gap >= retry_after,
            "{:?} came {gap:?} after the 503",
            request.range
        );
    }
}
