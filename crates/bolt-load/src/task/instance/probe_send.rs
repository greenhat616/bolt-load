//! Probe whether the futures of a download attempt are `Send`.
//!
//! They are built and run on a local runtime because rustc cannot yet prove
//! them `Send` (rust-lang/rust#100013). This module only compiles with
//! `--cfg bolt_load_probe_send`; the CI `hra-probe` job compiles it with and
//! without `-Zhigher-ranked-assumptions` to see whether that workaround is
//! still needed.

use bolt_load_tests::adapter::simple::SimpleTestAdapter;
use smol_cancellation_token::CancellationToken;

use super::drive::{Attempt, run};
use crate::{
    adapter::DynBoltLoadAdapter,
    runtime::ThreadedRuntimeImpl,
    task::{TaskOptions, destination::Destination, status::StatusPublisher},
};

fn assert_send<F: Future<Output = ()> + Send + 'static>(_: &F) {}

#[test]
fn attempts_are_send() {
    let attempt = Attempt {
        adapter: DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(1024)),
        destination: Destination::File("attempt.bin".into()),
        prefer_mode: None,
        options: TaskOptions::default(),
        meta: None,
        cancel_token: CancellationToken::new(),
    };
    let (publisher, _status) = StatusPublisher::new();
    let attempt = run(attempt, ThreadedRuntimeImpl::new_tokio_rt(), publisher);
    assert_send(&attempt);
}
