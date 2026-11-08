//! A connection that stops sending without closing, with plenty left, is
//! recovered long before the runner's idle timeout (design
//! §14.19), through the silent restart, or the growth it no longer holds
//! back and the endgame hedge.

#![cfg(feature = "tokio")]

use std::{num::NonZeroUsize, time::Duration};

use bolt_load::{runtime::ThreadedRuntimeImpl, task::Task};
use bolt_load_tests::adapter::simple::SimpleTestAdapterBuilder;

const MIB: u64 = 1 << 20;

#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_connection_recovers_before_the_idle_timeout() {
    const SIZE: u64 = 8 * MIB;
    const STALL: u64 = 6 * MIB;
    let adapter = SimpleTestAdapterBuilder::new()
        .content_size(SIZE as usize)
        .support_range(true)
        .chunk_size(16 * 1024)
        .speed_burst(Duration::from_millis(62))
        .max_per_stream_speed(2 * MIB)
        .stall_once_at(STALL)
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let task = Task::builder(adapter.clone())
        .runtime(ThreadedRuntimeImpl::new_tokio_rt())
        .save_to(dir.path().join("download.bin"))
        .max_connections(NonZeroUsize::new(2).unwrap())
        .spawn()
        .unwrap();
    // A deadlock watchdog only; the idle timeout alone would take 30 s.
    let output = tokio::time::timeout(Duration::from_secs(120), task)
        .await
        .expect("the download ends")
        .expect("the download completes");
    assert!(std::fs::read(&output.path).unwrap() == adapter.content()[..]);

    let stalled = adapter.stalled_at().expect("a connection stalled");
    let retried = adapter
        .streams()
        .iter()
        .find(|stream| stream.at > stalled && stream.range.0 <= STALL && STALL < stream.range.1)
        .expect("the stalled offset is requested again")
        .at;
    // A smoke test: the loop tests carry the timing. The bound only has to
    // stay clear of the 30 s idle timeout.
    let wait = retried - stalled;
    assert!(wait < Duration::from_secs(25), "waited {wait:?}");
}
