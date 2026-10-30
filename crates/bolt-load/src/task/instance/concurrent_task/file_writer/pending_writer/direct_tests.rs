//! A manually driven command queue exercises the real FileWriter submit path.
use std::{
    collections::VecDeque,
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    pin::Pin,
    sync::Mutex,
    task::{Context, Poll},
    time::Duration,
};

use futures::{
    future::FutureObj,
    task::{Spawn, SpawnError},
};

use super::{
    super::{
        CloseOnExit, Command, FileRangeWriterImpl, FileRangeWriterKind, null_writer::NullWriter,
    },
    *,
};
use crate::runtime::{DowncastLocalRuntime, ThreadedRuntime, TimerBuilder, TimerImpl};

#[derive(Clone, Default)]
struct ParkRuntime {
    tasks: Arc<Mutex<VecDeque<FutureObj<'static, ()>>>>,
    reject: bool,
}
impl Spawn for ParkRuntime {
    fn spawn_obj(&self, future: FutureObj<'static, ()>) -> Result<(), SpawnError> {
        if self.reject {
            return Err(SpawnError::shutdown());
        }
        self.tasks.lock().unwrap().push_back(future);
        Ok(())
    }
}
impl DowncastLocalRuntime for ParkRuntime {}
impl TimerBuilder for ParkRuntime {
    fn create_delayed_timer(&self, duration: Duration) -> TimerImpl {
        TimerImpl::Custom(crate::runtime::DynTimer::new_box(TestTimer {
            timer: async_io::Timer::at(std::time::Instant::now()),
            duration,
        }))
    }
}
impl ThreadedRuntime for ParkRuntime {}
struct TestTimer {
    timer: async_io::Timer,
    duration: Duration,
}
impl crate::runtime::Timer for TestTimer {
    async fn tick(&mut self) {
        (&mut self.timer).await;
        self.timer.set_after(self.duration);
    }
}

impl ParkRuntime {
    fn take(&self) -> FutureObj<'static, ()> {
        self.tasks
            .lock()
            .unwrap()
            .pop_front()
            .expect("one fallback")
    }
    fn count(&self) -> usize {
        self.tasks.lock().unwrap().len()
    }
}

fn fixture(
    queue: usize,
    capacity: usize,
    rt: ParkRuntime,
) -> (PendingWriter<FileWriter>, FileWriter, Receiver<Command>) {
    let (tx, rx) = async_channel::bounded(queue);
    let writer = FileWriter {
        kind: FileRangeWriterKind::Null,
        inner: Arc::new(FileRangeWriterImpl::Null(NullWriter { tx })),
    };
    let pending = PendingWriter::new_direct(
        Arc::new(writer.clone()),
        ThreadedRuntimeImpl::new_other_rt(rt),
        capacity,
    );
    (pending, writer, rx)
}

fn write(pending: &mut PendingWriter<FileWriter>, offset: u64) -> WriteStatus {
    pending
        .write_range(offset..offset + 1, Bytes::from_static(b"x"))
        .unwrap()
}

fn execute(rx: &Receiver<Command>, result: io::Result<()>) -> Range<u64> {
    let Command::Write(chunk, reply) = rx.try_recv().expect("queued write") else {
        panic!("expected Write");
    };
    let range = chunk.range;
    reply.complete(result, Path::new("<controlled>"));
    range
}

fn poll_task(task: &mut FutureObj<'static, ()>) -> Poll<()> {
    let mut cx = Context::from_waker(futures::task::noop_waker_ref());
    Pin::new(task).poll(&mut cx)
}

fn assert_one(pending: &mut PendingWriter<FileWriter>, ok: bool) {
    let batch = pending.try_tick();
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].result.is_ok(), ok);
    assert_eq!(pending.in_flight_count(), 0);
    assert!(pending.try_tick().is_empty());
}

#[test]
fn test_direct_completion_exactly_once_on_success() {
    let (mut pending, _writer, rx) = fixture(1, 1, ParkRuntime::default());
    write(&mut pending, 0);
    execute(&rx, Ok(()));
    assert_one(&mut pending, true);
}

#[test]
fn test_direct_completion_exactly_once_on_error() {
    let (mut pending, _writer, rx) = fixture(1, 1, ParkRuntime::default());
    write(&mut pending, 0);
    execute(&rx, Err(io::Error::other("disk full")));
    assert_one(&mut pending, false);
}

#[test]
fn test_direct_completion_exactly_once_on_worker_panic() {
    let (mut pending, _writer, rx) = fixture(2, 2, ParkRuntime::default());
    write(&mut pending, 0);
    write(&mut pending, 1);
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            let _close_on_exit = CloseOnExit(&rx);
            let _executing = rx.try_recv().unwrap();
            panic!("worker failed with another write queued");
        }))
        .is_err()
    );
    let batch = pending.try_tick();
    assert_eq!(batch.len(), 2);
    assert!(batch.iter().all(|c| c.result.is_err()));
    assert!(pending.is_idle());
    assert!(pending.try_tick().is_empty());
}

#[test]
fn test_direct_completion_exactly_once_on_queue_closed() {
    let (mut pending, _writer, rx) = fixture(1, 1, ParkRuntime::default());
    // Simulate worker shutdown with a queued request and a live sender.
    write(&mut pending, 0);
    drop(CloseOnExit(&rx));
    assert_one(&mut pending, false);
    // A new submission to the closed queue also completes exactly once.
    write(&mut pending, 1);
    assert_one(&mut pending, false);
}

#[test]
fn test_queue_full_falls_back_to_spawn() {
    let rt = ParkRuntime::default();
    let (mut pending, _writer, rx) = fixture(1, 2, rt.clone());
    write(&mut pending, 0);
    write(&mut pending, 1);
    assert_eq!(rt.count(), 1);
    assert_eq!(pending.in_flight_count(), 2);
    execute(&rx, Ok(()));
    assert_eq!(pending.try_tick().len(), 1);
    let mut fallback = rt.take();
    assert!(poll_task(&mut fallback).is_ready());
    assert_eq!(pending.in_flight_count(), 1, "fallback must not increment");
    execute(&rx, Ok(()));
    assert_one(&mut pending, true);
}

#[test]
fn test_pending_slot_preserved_under_direct_path() {
    let rt = ParkRuntime::default();
    let (mut pending, _writer, rx) = fixture(1, 1, rt.clone());
    assert_eq!(write(&mut pending, 0), WriteStatus::Dispatched);
    assert_eq!(write(&mut pending, 1), WriteStatus::Pending);
    assert!(pending.write_range(2..3, Bytes::from_static(b"x")).is_err());
    execute(&rx, Ok(()));
    assert_eq!(pending.try_tick().len(), 1);
    assert!(!pending.has_pending_write());
    assert_eq!(pending.in_flight_count(), 1);
    assert_eq!(rt.count(), 0);
    execute(&rx, Ok(()));
    assert_one(&mut pending, true);
}

#[test]
fn test_late_error_stops_pending_flush() {
    let (mut pending, _writer, rx) = fixture(2, 2, ParkRuntime::default());
    write(&mut pending, 0);
    write(&mut pending, 1);
    assert_eq!(write(&mut pending, 2), WriteStatus::Pending);
    execute(&rx, Ok(()));
    execute(&rx, Err(io::Error::other("late error")));
    let batch = pending.try_tick();
    assert_eq!(batch.len(), 2);
    assert!(batch[0].result.is_ok());
    assert!(batch[1].result.is_err());
    assert!(pending.has_pending_write());
    assert_eq!(pending.in_flight_count(), 0);
    assert!(futures::executor::block_on(pending.finish()).is_empty());
    assert!(
        rx.try_recv().is_err(),
        "buffered request must not be dispatched"
    );
}

#[test]
fn test_fallback_spawn_rejected_completes_once() {
    let rt = ParkRuntime {
        reject: true,
        ..Default::default()
    };
    let (mut pending, _writer, rx) = fixture(1, 2, rt);
    write(&mut pending, 0);
    write(&mut pending, 1);
    let rejected = pending.try_tick();
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0].range, 1..2);
    assert!(rejected[0].result.is_err());
    execute(&rx, Ok(()));
    assert_one(&mut pending, true);
}

#[test]
fn test_accepted_unpolled_task_drop_completes_once() {
    // Both public generic dispatch and the direct queue-full fallback own
    // their guards before the runtime polls them.
    for direct in [false, true] {
        let rt = ParkRuntime::default();
        let (mut pending, _writer, rx) = fixture(1, 2, rt.clone());
        if direct {
            write(&mut pending, 0);
        } else {
            pending.dispatch = PendingWriter::<FileWriter>::dispatch_spawned;
        }
        write(&mut pending, 1);
        drop(rt.take());
        let dropped = pending.try_tick();
        assert_eq!(dropped.len(), 1);
        assert!(dropped[0].result.is_err());
        if direct {
            execute(&rx, Ok(()));
            assert_one(&mut pending, true);
        } else {
            assert!(pending.is_idle());
            assert!(pending.try_tick().is_empty());
        }
    }
}

#[test]
fn test_fast_path_can_overtake_fallback() {
    let rt = ParkRuntime::default();
    let (mut pending, _writer, rx) = fixture(1, 2, rt.clone());
    write(&mut pending, 0);
    write(&mut pending, 1);
    assert_eq!(execute(&rx, Ok(())), 0..1);
    assert_eq!(pending.try_tick().len(), 1);
    write(&mut pending, 2);
    let mut fallback = rt.take();
    assert!(poll_task(&mut fallback).is_pending());
    assert_eq!(execute(&rx, Ok(())), 2..3);
    assert_eq!(pending.try_tick().len(), 1);
    assert!(poll_task(&mut fallback).is_ready());
    assert_eq!(execute(&rx, Ok(())), 1..2);
    assert_one(&mut pending, true);
}

#[test]
fn test_dropping_blocked_fallback_releases_writer_before_join() {
    let rt = ParkRuntime::default();
    let (mut pending, writer, rx) = fixture(1, 2, rt.clone());
    write(&mut pending, 0);
    write(&mut pending, 1);
    let mut fallback = rt.take();
    assert!(poll_task(&mut fallback).is_pending());
    drop(fallback);
    execute(&rx, Ok(()));
    let batch = futures::executor::block_on(pending.finish());
    assert_eq!(batch.len(), 2);
    assert_eq!(batch.iter().filter(|c| c.result.is_err()).count(), 1);
    assert_eq!(Arc::strong_count(&writer.inner), 1);
}

#[tokio::test]
async fn test_finish_joins_blocked_fallback_before_finalize() {
    let rt = ParkRuntime::default();
    let (mut pending, writer, rx) = fixture(1, 2, rt.clone());
    write(&mut pending, 0);
    write(&mut pending, 1);
    let mut fallback = rt.take();
    assert!(poll_task(&mut fallback).is_pending());
    // Download cancellation uses this same finish path: outstanding writes
    // are drained instead of abandoning a blocked submit.
    let mut finish = std::pin::pin!(pending.finish());
    assert!(futures::poll!(finish.as_mut()).is_pending());
    execute(&rx, Ok(()));
    assert!(futures::poll!(finish.as_mut()).is_pending());
    assert!(poll_task(&mut fallback).is_ready());
    assert!(futures::poll!(finish.as_mut()).is_pending());
    execute(&rx, Ok(()));
    let batch = tokio::time::timeout(Duration::from_secs(5), finish)
        .await
        .unwrap();
    assert_eq!(batch.len(), 2);
    assert!(batch.iter().all(|c| c.result.is_ok()));
    assert_eq!(Arc::strong_count(&writer.inner), 1);
    let (result, ()) = futures::join!(writer.finalize(), async {
        let Command::Finalize(ack) = rx.recv().await.unwrap() else {
            panic!("writes must precede finalization");
        };
        ack.send(Ok(())).unwrap();
    });
    result.unwrap();
}

#[tokio::test]
async fn test_direct_backends_preserve_bytes() {
    use bolt_load_tests::adapter::simple::calculate_blake3;
    let mut kinds = vec![
        FileRangeWriterKind::Pool,
        FileRangeWriterKind::Null,
        FileRangeWriterKind::Slow(Default::default()),
    ];
    #[cfg(feature = "mmap")]
    kinds.push(FileRangeWriterKind::Mmap);
    #[cfg(feature = "compio")]
    kinds.push(FileRangeWriterKind::Compio);
    let bytes = Bytes::from((0..65536).map(|n| (n % 251) as u8).collect::<Vec<_>>());
    for kind in kinds {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("direct.bin");
        let writer = FileWriter::new_with_kind(&path, bytes.len() as u64, kind)
            .await
            .unwrap();
        let mut pending = PendingWriter::new_direct(
            Arc::new(writer.clone()),
            ThreadedRuntimeImpl::new_tokio_rt(),
            4,
        );
        let mut batch = Vec::new();
        for index in [3, 1, 0, 2] {
            let start = index * 16384;
            pending
                .write_range(
                    start as u64..(start + 16384) as u64,
                    bytes.slice(start..start + 16384),
                )
                .unwrap();
            if matches!(kind, FileRangeWriterKind::Slow(_)) {
                // Slow clones a shared file cursor; exercise its fallback
                // sequentially while real direct backends remain concurrent.
                batch.extend(
                    tokio::time::timeout(Duration::from_secs(5), pending.flush())
                        .await
                        .unwrap(),
                );
            }
        }
        batch.extend(
            tokio::time::timeout(Duration::from_secs(10), pending.finish())
                .await
                .unwrap(),
        );
        assert_eq!(batch.len(), 4);
        assert!(batch.iter().all(|c| c.result.is_ok()), "{batch:?}");
        writer.finalize().await.unwrap();
        if kind != FileRangeWriterKind::Null {
            assert_eq!(
                calculate_blake3(&std::fs::read(&path).unwrap()),
                calculate_blake3(&bytes)
            );
        }
    }
}

#[test]
fn test_unpolled_backend_task_drop_completes_once() {
    let (mut pending, _writer, rx) = fixture(1, 1, ParkRuntime::default());
    write(&mut pending, 0);
    let command = rx.try_recv().unwrap();
    // The same ownership boundary as a detached compio write task.
    let task = async move {
        let Command::Write(_, reply) = command else {
            unreachable!()
        };
        reply.complete(Ok(()), Path::new("<controlled>"));
    };
    drop(task);
    assert_one(&mut pending, false);
}

#[test]
fn test_completion_drop_does_not_panic_on_full_channel() {
    let (tx, rx) = async_channel::bounded(1);
    tx.try_send(WriteCompletion {
        range: 0..1,
        result: Ok(()),
    })
    .unwrap();
    let done = CompletionGuard::new(
        tx,
        Chunk {
            range: 1..2,
            data: Bytes::from_static(b"x"),
        },
    );
    assert!(catch_unwind(AssertUnwindSafe(|| drop(done))).is_ok());
    assert_eq!(rx.try_recv().unwrap().range, 0..1);
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn test_cancelled_download_joins_blocked_fallback_before_finalize() {
    use bolt_load_tests::adapter::simple::SimpleTestAdapter;
    use smol_cancellation_token::CancellationToken;

    use super::super::super::{RunnerSpawner, download_loop::DownloadLoop};
    use crate::{
        adapter::DynBoltLoadAdapter,
        task::{Progress, TaskOptions},
    };

    let rt = ParkRuntime::default();
    let (unused_pending, writer, rx) = fixture(1, 2, rt.clone());
    drop(unused_pending);
    // Occupy the backend queue before DownloadLoop dispatches its first frame.
    let (ack, _ack_rx) = oneshot::channel();
    writer
        .submission()
        .unwrap()
        .0
        .try_send(Command::Write(
            Chunk {
                range: 0..0,
                data: Bytes::new(),
            },
            super::super::WriteReply::Awaited(ack),
        ))
        .unwrap();
    let cancel = CancellationToken::new();
    let spawner = RunnerSpawner::new(
        ThreadedRuntimeImpl::new_other_rt(rt.clone()),
        DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(1).with_range_support(true)),
        WaitGroup::new(),
        cancel.child_token(),
        crate::runner::DEFAULT_IDLE_TIMEOUT,
    );
    let (reporter, _status) = crate::task::status::ProgressReporter::for_test();
    let mut download = DownloadLoop::new(
        1,
        spawner,
        &writer,
        reporter,
        &TaskOptions::default(),
        cancel.clone(),
    );
    let (result, runner, mut fallback) = {
        let mut run = std::pin::pin!(download.run());
        assert!(futures::poll!(run.as_mut()).is_pending());
        let mut runner = rt.take();
        // Drive the actual runner until it waits for its data to be consumed.
        // The test adapter can yield during connection: poll until data is
        // delivered and the resulting fallback has been captured.
        tokio::time::timeout(Duration::from_secs(5), async {
            while rt.count() == 0 {
                let _ = poll_task(&mut runner);
                assert!(futures::poll!(run.as_mut()).is_pending());
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut fallback = rt.take();
        assert!(poll_task(&mut fallback).is_pending());
        cancel.cancel();
        let Poll::Ready(result) = futures::poll!(run.as_mut()) else {
            panic!("cancellation must win while submit is blocked");
        };
        (result, runner, fallback)
    };
    assert!(result.is_err());
    let mut progress = Progress {
        total: Some(1),
        downloaded: 0,
        downloaded_chunks: Vec::new(),
    };
    let mut finish = std::pin::pin!(download.finish(result, &mut progress));
    assert!(futures::poll!(finish.as_mut()).is_pending());
    drop(runner); // release the worker's WaitGroup membership
    assert!(futures::poll!(finish.as_mut()).is_pending());
    assert_eq!(execute(&rx, Ok(())), 0..0);
    assert!(poll_task(&mut fallback).is_ready());
    assert_eq!(execute(&rx, Ok(())), 0..1);
    let result = tokio::time::timeout(Duration::from_secs(5), finish)
        .await
        .unwrap();
    assert!(result.is_err(), "cleanup preserves cancellation");
    assert_eq!(Arc::strong_count(&writer.inner), 1);
    let (result, ()) = futures::join!(writer.finalize(), async {
        let Command::Finalize(ack) = rx.recv().await.unwrap() else {
            panic!("only finalize remains");
        };
        ack.send(Ok(())).unwrap();
    });
    result.unwrap();
}
