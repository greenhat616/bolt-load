//! A timer implemented outside the crate plugs into a custom runtime.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use bolt_load::runtime::{
    DowncastLocalRuntime, DynTimer, ThreadedRuntime, ThreadedRuntimeImpl, Timer, TimerBuilder,
    TimerImpl,
};
use futures::{
    future::FutureObj,
    task::{Spawn, SpawnError},
};

struct CountingTimer(Arc<AtomicUsize>);

impl Timer for CountingTimer {
    async fn tick(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Runs every spawned future on its own thread.
struct ThreadRuntime {
    ticks: Arc<AtomicUsize>,
}

impl Spawn for ThreadRuntime {
    fn spawn_obj(&self, future: FutureObj<'static, ()>) -> Result<(), SpawnError> {
        std::thread::spawn(move || futures::executor::block_on(future));
        Ok(())
    }
}

impl DowncastLocalRuntime for ThreadRuntime {}

impl TimerBuilder for ThreadRuntime {
    fn create_delayed_timer(&self, _duration: Duration) -> TimerImpl {
        TimerImpl::Custom(DynTimer::new_box(CountingTimer(self.ticks.clone())))
    }
}

impl ThreadedRuntime for ThreadRuntime {}

#[test]
fn custom_timer_ticks_on_a_custom_runtime() {
    let ticks = Arc::new(AtomicUsize::new(0));
    let rt = ThreadedRuntimeImpl::new_other_rt(ThreadRuntime {
        ticks: ticks.clone(),
    });

    let mut timer = rt.create_delayed_timer(Duration::from_secs(1));
    let (tx, rx) = std::sync::mpsc::channel();
    rt.spawn_obj(FutureObj::new(Box::new(async move {
        timer.tick().await;
        tx.send(()).unwrap();
    })))
    .unwrap();

    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(ticks.load(Ordering::SeqCst), 1);
}
