use std::{
    num::{NonZeroU32, NonZeroUsize},
    path::PathBuf,
    sync::Arc,
};

use bolt_load_core::adapter::AdapterError;
use smol_cancellation_token::CancellationToken;

use super::{
    BuildError, DownloadMode, Task, TaskOptions,
    destination::Destination,
    instance::{
        destination_lock::LockHooks,
        drive::{self, Attempt},
        file_steps::FileHooks,
    },
    status::StatusPublisher,
};
use crate::{
    adapter::{AnyAdapter, BoltLoadAdapterMeta},
    runtime::{LocalRuntimeBuilderImpl, ThreadedRuntimeImpl},
};

/// Configures a download; [`spawn`](Self::spawn) starts it.
///
/// Configuration is synchronous and performs no network I/O: the metadata
/// request, the mode selection and the destination resolution run inside the
/// task while it is `Initializing`. A builder is `Clone`, so a queue can keep
/// validated builders and spawn them later, or spawn again to retry.
#[derive(Clone)]
#[must_use = "a builder does nothing until `spawn` is called"]
pub struct TaskBuilder {
    adapter: Arc<AnyAdapter>,
    runtime: Option<ThreadedRuntimeImpl>,
    local_runtime_builder: Option<LocalRuntimeBuilderImpl>,
    destination: Option<Destination>,
    prefer_mode: Option<DownloadMode>,
    options: TaskOptions,
    cancel_token: Option<CancellationToken>,
    meta: Option<BoltLoadAdapterMeta>,
    /// How the destination's lock is taken; replaced in tests.
    lock_hooks: LockHooks,
    /// Whether the task keeps a state file for a later resume.
    resume: Resume,
    /// An identity of the source, stored in the state file and compared on
    /// resume.
    resume_source: Option<String>,
    /// What to call before each file step of a resumable task; tests fail
    /// them.
    file_hooks: FileHooks,
    /// Where a test records the progress each transfer starts with.
    #[cfg(test)]
    starts: Option<super::status::Starts>,
}

/// Whether a task keeps what it downloaded when it does not finish, and
/// continues from what an earlier task kept (design §11.5.2).
///
/// With [`Resume::On`], a download that can be resumed keeps two files
/// next to the destination when it does not finish: the partial file,
/// `<dest>.downloading`, and a state file, `<dest>.boltload`, which records
/// the blocks written and synced so far. The next task with `Resume::On`
/// for the same destination checks both, every kept block against its
/// checksum, and downloads only the rest. Whether a download can be
/// resumed is in [`TaskInfo::resumable`](super::TaskInfo::resumable); what a
/// task continued from, in [`TaskInfo::resumed`](super::TaskInfo::resumed).
///
/// A task never starts over on its own: when what was kept cannot be
/// continued, it fails with
/// [`DownloadError::ResumeRejected`](super::DownloadError::ResumeRejected)
/// and changes neither file. Starting over is the caller's decision, after
/// [`discard_partial`](super::discard_partial).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Resume {
    /// Start from an empty file; delete it when the task does not finish.
    #[default]
    Off,
    /// Keep the partial file and a state file when the task does not
    /// finish; continue from them when they are there, or fail with
    /// [`DownloadError::ResumeRejected`](super::DownloadError::ResumeRejected)
    /// when they cannot be continued.
    On,
}

impl TaskBuilder {
    pub(super) fn new(adapter: Arc<AnyAdapter>) -> Self {
        Self {
            adapter,
            runtime: None,
            local_runtime_builder: None,
            destination: None,
            prefer_mode: None,
            options: TaskOptions::default(),
            cancel_token: None,
            meta: None,
            lock_hooks: LockHooks::default(),
            resume: Resume::Off,
            resume_source: None,
            file_hooks: FileHooks::default(),
            #[cfg(test)]
            starts: None,
        }
    }

    /// The runtime the runners are spawned on and whose timer is used.
    /// Required: there is no ambient default.
    pub fn runtime(mut self, runtime: ThreadedRuntimeImpl) -> Self {
        self.runtime = Some(runtime);
        self
    }

    /// Builds the local runtime the task's attempt runs on, for a custom
    /// [`runtime`](Self::runtime) that cannot provide one itself.
    pub fn local_runtime_builder(mut self, builder: LocalRuntimeBuilderImpl) -> Self {
        self.local_runtime_builder = Some(builder);
        self
    }

    /// Save to exactly `path`. Replaces an earlier [`save_into`](Self::save_into).
    pub fn save_to(mut self, path: impl Into<PathBuf>) -> Self {
        self.destination = Some(Destination::File(path.into()));
        self
    }

    /// Save into `dir`, named by the filename the adapter suggests, reduced to
    /// one safe path component. The task fails with
    /// [`DownloadError::MissingFilename`](super::DownloadError::MissingFilename)
    /// when there is none, and with
    /// [`DownloadError::InvalidFilename`](super::DownloadError::InvalidFilename)
    /// when it is unsafe. Replaces an earlier [`save_to`](Self::save_to).
    pub fn save_into(mut self, dir: impl Into<PathBuf>) -> Self {
        self.destination = Some(Destination::Dir(dir.into()));
        self
    }

    /// Prefer a mode. `Singleton` is honoured, except by a task with
    /// [`Resume::On`] that can be resumed: it downloads over ranges with
    /// one connection, so that it keeps a state file (design §11.5.8).
    /// `Concurrent` (the default) is used when the size is known and the
    /// adapter supports ranges.
    pub fn prefer_mode(mut self, mode: DownloadMode) -> Self {
        self.prefer_mode = Some(mode);
        self
    }

    /// Set the connection limit and the termination options, see
    /// [`TaskOptions`].
    pub fn options(mut self, options: TaskOptions) -> Self {
        self.options = options;
        self
    }

    /// Set [`TaskOptions::max_connections`]. A later
    /// [`options`](Self::options) call replaces it.
    pub fn max_connections(mut self, limit: NonZeroUsize) -> Self {
        self.options.max_connections = limit;
        self
    }

    /// Set [`TaskOptions::max_attempts_per_range`]. A later
    /// [`options`](Self::options) call replaces it.
    pub fn max_attempts_per_range(mut self, limit: NonZeroU32) -> Self {
        self.options.max_attempts_per_range = limit;
        self
    }

    /// Cancel the task with `parent`. The task only cancels child tokens of
    /// it, so it never cancels `parent` itself.
    pub fn cancel_token(mut self, parent: CancellationToken) -> Self {
        self.cancel_token = Some(parent);
        self
    }

    /// Use known metadata instead of retrieving it.
    pub fn meta(mut self, meta: BoltLoadAdapterMeta) -> Self {
        self.meta = Some(meta);
        self
    }

    /// The metadata set by [`meta`](Self::meta) or retrieved before.
    pub fn cached_meta(&self) -> Option<&BoltLoadAdapterMeta> {
        self.meta.as_ref()
    }

    /// Retrieve the metadata now, for example to ask where to save the file,
    /// and cache it: the spawned task does not request it again.
    ///
    /// Retryable errors are retried as [`TaskOptions::meta_retry`] allows,
    /// sleeping on the runtime's timer when one is set. The cancel token is
    /// not observed; drop the future to cancel it.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip(self)))]
    pub async fn retrieve_meta(&mut self) -> Result<&BoltLoadAdapterMeta, AdapterError> {
        if self.meta.is_none() {
            let meta = super::meta::retrieve_meta(
                &self.adapter,
                &self.options.meta_retry,
                self.runtime.as_ref(),
            )
            .await?;
            self.meta = Some(meta);
        }
        Ok(self.meta.as_ref().expect("the metadata was just set"))
    }

    /// Check the configuration as [`spawn`](Self::spawn) does, without
    /// network I/O: the runtime and the destination are set, the options are
    /// valid, and the destination's directory exists.
    pub fn validate(&self) -> Result<(), BuildError> {
        if self.runtime.is_none() {
            return Err(BuildError::MissingRuntime);
        }
        self.options.validate()?;
        if self
            .resume_source
            .as_ref()
            .is_some_and(|source| source.len() > super::instance::state_file::MAX_SOURCE_LEN)
        {
            return Err(BuildError::InvalidOptions {
                reason: "the resume source is longer than 4096 bytes",
            });
        }
        self.destination
            .as_ref()
            .ok_or(BuildError::MissingDestination)?
            .precheck()
    }

    /// Take the destination's lock with `hooks`.
    #[cfg(test)]
    pub(crate) fn lock_hooks(mut self, hooks: LockHooks) -> Self {
        self.lock_hooks = hooks;
        self
    }

    /// Keep what the download wrote when it does not finish, and continue
    /// from what an earlier task kept, or not; see [`Resume`].
    ///
    /// A resume needs metadata as the adapter retrieves it: the task
    /// retrieves it again when it finds a state file, even when
    /// [`meta`](Self::meta) gave some. Adapters cache what they retrieved,
    /// so on the same adapter the state file is compared with that, and a
    /// change since is caught only by the `If-Range` of the range requests
    /// the resume makes: one that kept every block makes none. To check
    /// against the resource as it is now, resume with a new adapter.
    pub fn resume(mut self, resume: Resume) -> Self {
        self.resume = resume;
        self
    }

    /// An identity of the source, such as its URL, stored in the state
    /// file and compared on resume: another one rejects the resume with
    /// [`RejectReason::SourceMismatch`](super::RejectReason::SourceMismatch).
    /// At most 4096 bytes; a longer one fails [`validate`](Self::validate).
    /// Optional; a task without it resumes only from a state file without
    /// one.
    pub fn resume_source(mut self, source: impl Into<String>) -> Self {
        self.resume_source = Some(source.into());
        self
    }

    /// Call `hooks` before each file step of a resumable task.
    #[cfg(test)]
    pub(crate) fn file_hooks(mut self, hooks: FileHooks) -> Self {
        self.file_hooks = hooks;
        self
    }

    /// Record in `starts` the progress of every `Downloading` start, as
    /// it is published, before the transfer runs.
    #[cfg(test)]
    pub(crate) fn record_starts(mut self, starts: super::status::Starts) -> Self {
        self.starts = Some(starts);
        self
    }

    /// Validate and start the download on a `blocking` pool thread.
    ///
    /// Each running task occupies one such thread until it ends.
    pub fn spawn(self) -> Result<Task, BuildError> {
        self.validate()?;
        let Self {
            adapter,
            runtime,
            local_runtime_builder,
            destination,
            prefer_mode,
            options,
            cancel_token,
            meta,
            lock_hooks,
            resume,
            resume_source,
            file_hooks,
            #[cfg(test)]
            starts,
        } = self;
        let (Some(runtime), Some(destination)) = (runtime, destination) else {
            unreachable!("validated above");
        };
        let cancel_token = match cancel_token {
            Some(parent) => parent.child_token(),
            None => CancellationToken::new(),
        };
        let (publisher, status) = StatusPublisher::new();
        #[cfg(test)]
        if let Some(starts) = starts {
            publisher.record_starts(starts);
        }
        let attempt = Attempt {
            adapter,
            destination,
            prefer_mode,
            options,
            meta,
            cancel_token: cancel_token.clone(),
            lock_hooks,
            resume,
            resume_source,
            file_hooks,
        };
        let join = drive::spawn(attempt, runtime, local_runtime_builder, publisher);
        Ok(Task::new(join, status, cancel_token))
    }
}

impl std::fmt::Debug for TaskBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskBuilder")
            .field("runtime", &self.runtime.is_some())
            .field("destination", &self.destination)
            .field("prefer_mode", &self.prefer_mode)
            .field("options", &self.options)
            .field("meta", &self.meta)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use bolt_load_tests::adapter::simple::SimpleTestAdapter;

    use super::*;
    use crate::task::{DEFAULT_MAX_ATTEMPTS_PER_RANGE, DEFAULT_MAX_CONNECTIONS};

    #[test]
    fn test_max_connections_sets_the_option() {
        let limit = NonZeroUsize::new(3).unwrap();
        let builder = Task::builder(SimpleTestAdapter::new(1024)).max_connections(limit);
        assert_eq!(builder.options.max_connections, limit);
        // `options` replaces the whole set, the limit included.
        let builder = builder.options(TaskOptions::default());
        assert_eq!(builder.options.max_connections, DEFAULT_MAX_CONNECTIONS);
    }

    #[test]
    fn test_max_attempts_per_range_sets_the_option() {
        let limit = NonZeroU32::new(2).unwrap();
        let builder = Task::builder(SimpleTestAdapter::new(1024)).max_attempts_per_range(limit);
        assert_eq!(builder.options.max_attempts_per_range, limit);
        assert_eq!(
            TaskOptions::default()
                .max_attempts_per_range(limit)
                .max_attempts_per_range,
            limit
        );
        // `options` replaces the whole set, the limit included.
        let builder = builder.options(TaskOptions::default());
        assert_eq!(
            builder.options.max_attempts_per_range,
            DEFAULT_MAX_ATTEMPTS_PER_RANGE
        );
    }
}
