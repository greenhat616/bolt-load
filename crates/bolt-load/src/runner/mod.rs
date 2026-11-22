#![doc = include_str!("../../docs/runner.md")]
use async_ringbuf::{AsyncHeapCons, AsyncHeapProd};
use bytes::Bytes;
use futures::future::BoxFuture;
use smol_cancellation_token::CancellationToken;

use crate::{
    adapter::{AdapterError, AnyBytesStream},
    task::RunnerId,
};

mod builder;
mod connector;
mod control;
mod error;
mod run_loop;
pub use builder::*;
pub use connector::*;
use control::ControlSource;
pub use control::{ControlSignalReceiver, ControlSignalSender};
pub(crate) use control::{LimitReceiver, LimitSender, limit_mailbox};
pub use error::*;

// TODO: make it configurable or detect the local disk performance?
/// The most bytes a runner buffers before it delivers them as a frame.
pub(crate) const BUFFER_SIZE: usize = 32 * 1024; // 32KB

/// The default period without non-empty upstream data after which a runner
/// fails with a retryable [`TaskError::Timeout`]
pub const DEFAULT_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// The default period a runner may take to connect before it fails with a
/// retryable [`TaskError::Timeout`]
pub const DEFAULT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const DATA_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub const LIFECYCLE_CHANNEL_CAPACITY: usize = 2;
pub const DATA_FRAME_CHANNEL_CAPACITY: usize = 32;

#[derive(Debug)]
pub struct DataFrame {
    pub data: Bytes,
}

pub type DataFrameSender = AsyncHeapProd<DataFrame>;
pub type DataFrameReceiver = AsyncHeapCons<DataFrame>;
pub type LifecycleSender = AsyncHeapProd<LifecycleEvent>;
pub type LifecycleReceiver = AsyncHeapCons<LifecycleEvent>;

#[derive(Debug)]
pub enum LifecycleEvent {
    /// The runner is connected; its data frames follow
    Started,
    /// Stopped with message
    Stopped(StoppedReason),
}

impl LifecycleEvent {
    #[inline]
    pub fn failed(e: TaskError) -> Self {
        Self::Stopped(StoppedReason::Failed(e))
    }

    #[inline]
    pub fn finished() -> Self {
        Self::Stopped(StoppedReason::Finished)
    }

    #[inline]
    pub fn started() -> Self {
        Self::Started
    }

    #[inline]
    pub const fn is_finished(&self) -> bool {
        matches!(self, Self::Stopped(StoppedReason::Finished))
    }

    #[inline]
    pub fn is_cancelled(&self) -> bool {
        match self {
            Self::Stopped(StoppedReason::Failed(e)) => e.is_cancelled(),
            _ => false,
        }
    }
}

/// The reason why the task is stopped
#[derive(Debug)]
pub enum StoppedReason {
    /// The task is finished
    Finished,
    /// The task is failed
    Failed(TaskError),
}

#[derive(Debug)]
enum TaskRunError {
    Cancelled,
    Failed(TaskError),
}

impl From<TaskError> for TaskRunError {
    fn from(value: TaskError) -> Self {
        Self::Failed(value)
    }
}

/// Where a runner reads its bytes from.
enum RunnerInput {
    Connected(AnyBytesStream),
    /// Connected by the runner itself before it starts.
    Connecting(BoxFuture<'static, Result<AnyBytesStream, AdapterError>>),
    /// The connection was abandoned before it yielded a stream.
    Abandoned,
}

/// runner for each chunk, or single file, responsible for downloading each chunk
#[derive(derive_more::Debug)]
pub struct TaskRunner {
    /// The id of the runner
    #[cfg_attr(
        not(feature = "tracing"),
        expect(dead_code, reason = "only read by tracing spans")
    )]
    id: RunnerId,
    /// The total size of the this chunk or file
    /// possible None if the total size is unknown
    total: Option<u64>,
    /// the downloaded size
    downloaded: u64,
    /// the stream of the task, or how to connect it
    #[debug(skip)]
    input: RunnerInput,
    /// the control signals from the manager
    #[debug(skip)]
    control: ControlSource,
    /// the sender of the lifecycle events
    #[debug(skip)]
    lifecycle_tx: LifecycleSender,
    /// the sender of the data frames
    #[debug(skip)]
    data_tx: Option<DataFrameSender>,
    /// How long to wait for data frames to be consumed after the stream stops.
    data_drain_timeout: std::time::Duration,
    /// How long the upstream may yield no non-empty data before failing with `Timeout`.
    idle_timeout: std::time::Duration,
    /// How long connecting may take before failing with `Timeout`.
    connect_timeout: std::time::Duration,
    /// When the runner was built, to tell scheduling from connecting delays.
    built_at: std::time::Instant,
    /// the cancel token
    cancel_token: CancellationToken,
    /// Stops this runner alone, see [`TaskRunnerBuilder::stop_token`].
    stop_token: Option<CancellationToken>,
    /// Stamped as it reads its body, see
    /// [`TaskRunnerBuilder::read_activity`].
    read_activity: Option<ReadActivity>,
}

/// When a runner was last active reading its body, shared with the loop
/// that spawned it (design §14.19).
///
/// The runner hands its bytes over a frame at a time, once a buffer is
/// full, so a runner reading slowly may deliver nothing for long. It is
/// active where it resets its idle timer: at every non-empty item the
/// stream yields, and when it goes back to reading an item after pushing a
/// full buffer of it. While it pushes, waiting for the loop to take its
/// frames, it is active now: waiting for the consumer is not idleness. A
/// push that ends the stream, its idle time or its error is no new read.
/// The loop so tells a runner still reading from one whose stream gives
/// nothing, by the runner's own clock.
#[derive(Debug, Clone, Default)]
pub(crate) struct ReadActivity(std::sync::Arc<std::sync::Mutex<Activity>>);

#[derive(Debug, Default)]
struct Activity {
    last: Option<std::time::Instant>,
    emitting: bool,
    /// Every state a reader could see, in order.
    #[cfg(test)]
    seen: Vec<(Option<std::time::Instant>, bool)>,
}

impl ReadActivity {
    /// Change the activity under one lock: a reader sees it before `change`
    /// or after it, never halfway.
    fn change(&self, change: impl FnOnce(&mut Activity)) {
        let mut activity = self.0.lock().expect("read activity mutex poisoned");
        change(&mut activity);
        #[cfg(test)]
        {
            let state = (activity.last, activity.emitting);
            activity.seen.push(state);
        }
    }

    /// Every state a reader could have seen so far, `(last, emitting)`.
    #[cfg(test)]
    pub(crate) fn seen(&self) -> Vec<(Option<std::time::Instant>, bool)> {
        self.0
            .lock()
            .expect("read activity mutex poisoned")
            .seen
            .clone()
    }

    /// When the runner was last active, as of `now`: `now` itself while it
    /// pushes, `None` if it never was.
    pub(crate) fn last_active(&self, now: std::time::Instant) -> Option<std::time::Instant> {
        let activity = self.0.lock().expect("read activity mutex poisoned");
        if activity.emitting {
            Some(now)
        } else {
            activity.last
        }
    }

    /// The runner was active at `at`. An earlier time than one noted
    /// already changes nothing.
    pub(crate) fn note_at(&self, at: std::time::Instant) {
        self.change(|activity| activity.last = activity.last.max(Some(at)));
    }

    /// The runner started pushing a frame.
    pub(crate) fn start_emitting(&self) {
        self.change(|activity| activity.emitting = true);
    }

    /// The runner's push ended, and it reads no more.
    pub(crate) fn stop_emitting(&self) {
        self.change(|activity| activity.emitting = false);
    }

    /// The runner's push ended at `at`, and it reads on: at once, so that
    /// no reader sees it neither pushing nor read since it started pushing.
    pub(crate) fn resume_reading(&self, at: std::time::Instant) {
        self.change(|activity| {
            activity.last = activity.last.max(Some(at));
            activity.emitting = false;
        });
    }
}

impl TaskRunner {
    /// Get the builder of the task runner
    pub fn builder() -> TaskRunnerBuilder {
        TaskRunnerBuilder::default()
    }
}

#[cfg(test)]
mod tests;
