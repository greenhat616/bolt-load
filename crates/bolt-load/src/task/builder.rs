use std::{path::PathBuf, sync::Arc};

use bolt_load_core::adapter::AdapterError;
use smol_cancellation_token::CancellationToken;

use super::{
    BuildError, DownloadMode, Task, TaskOptions,
    destination::Destination,
    instance::drive::{self, Attempt},
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

    /// Prefer a mode. `Singleton` is always honoured; `Concurrent` (the
    /// default) is used when the size is known and the adapter supports
    /// ranges.
    pub fn prefer_mode(mut self, mode: DownloadMode) -> Self {
        self.prefer_mode = Some(mode);
        self
    }

    /// Set the termination options, see [`TaskOptions`].
    pub fn options(mut self, options: TaskOptions) -> Self {
        self.options = options;
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
        self.destination
            .as_ref()
            .ok_or(BuildError::MissingDestination)?
            .precheck()
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
        } = self;
        let (Some(runtime), Some(destination)) = (runtime, destination) else {
            unreachable!("validated above");
        };
        let cancel_token = match cancel_token {
            Some(parent) => parent.child_token(),
            None => CancellationToken::new(),
        };
        let (publisher, status) = StatusPublisher::new();
        let attempt = Attempt {
            adapter,
            destination,
            prefer_mode,
            options,
            meta,
            cancel_token: cancel_token.clone(),
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
