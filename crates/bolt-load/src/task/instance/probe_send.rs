//! Probe whether the futures of a download attempt are `Send`.
//!
//! They are built and run on a local runtime because rustc cannot yet prove
//! them `Send` (rust-lang/rust#100013). This module only compiles with
//! `--cfg bolt_load_probe_send`; the CI `hra-probe` job compiles it with and
//! without `-Zhigher-ranked-assumptions` to see whether that workaround is
//! still needed.

use std::sync::Arc;

use bolt_load_tests::adapter::simple::SimpleTestAdapter;
use smol_cancellation_token::CancellationToken;

use super::{concurrent_task::concurrent_attempt, singleton_task::singleton_attempt};
use crate::{
    adapter::{AnyAdapter, DynBoltLoadAdapter},
    runtime::ThreadedRuntimeImpl,
    task::TaskOptions,
};

fn assert_send<F: Future<Output = ()> + Send + 'static>(_: &F) {}

fn adapter() -> Arc<AnyAdapter> {
    DynBoltLoadAdapter::new_arc(SimpleTestAdapter::new(1024))
}

#[test]
fn attempts_are_send() {
    let rt = ThreadedRuntimeImpl::new_tokio_rt();
    let (event_tx, _event_rx) = async_channel::unbounded();
    let concurrent = concurrent_attempt(
        adapter(),
        "concurrent.bin".into(),
        event_tx.clone(),
        CancellationToken::new(),
        rt.clone(),
        TaskOptions::default(),
    );
    assert_send(&concurrent);
    let singleton = singleton_attempt(
        adapter(),
        "singleton.bin".into(),
        event_tx,
        CancellationToken::new(),
        rt,
        TaskOptions::default(),
    );
    assert_send(&singleton);
}
