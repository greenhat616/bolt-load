//! Fixtures shared by the concurrent task tests.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures::{
    future::FutureObj,
    task::{Spawn, SpawnError},
};

use crate::runtime::{
    DowncastLocalRuntime, DynTimer, ThreadedRuntime, Timer, TimerBuilder, TimerImpl,
};

#[derive(Clone, Debug)]
pub(super) struct FailOnSpawnRuntime {
    fail_on: usize,
    pub(super) calls: Arc<AtomicUsize>,
}

impl FailOnSpawnRuntime {
    pub(super) fn new(fail_on: usize) -> Self {
        Self {
            fail_on,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Spawn for FailOnSpawnRuntime {
    fn spawn_obj(&self, future: FutureObj<'static, ()>) -> Result<(), SpawnError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == self.fail_on {
            return Err(SpawnError::shutdown());
        }

        tokio::spawn(future);
        Ok(())
    }
}

impl DowncastLocalRuntime for FailOnSpawnRuntime {}

impl TimerBuilder for FailOnSpawnRuntime {
    fn create_delayed_timer(&self, _duration: Duration) -> TimerImpl {
        TimerImpl::Custom(DynTimer::new_box(NoopTimer))
    }
}

impl ThreadedRuntime for FailOnSpawnRuntime {}

struct NoopTimer;

impl Timer for NoopTimer {
    async fn tick(&mut self) {}
}
