//! The two transfer modes of a download attempt and the lifecycle that drives
//! them, see [`drive`].

use super::DownloadError;

/// Concurrent download task implementation.
pub mod concurrent_task;
pub(crate) mod destination_lock;
pub(crate) mod drained_file;
pub(crate) mod drive;
pub(crate) mod file_steps;
mod full_body;
mod id;
mod output_file;
mod ownership;
mod sampler;
mod singleton_task;
pub(crate) mod state_file;

#[cfg(all(test, bolt_load_probe_send))]
mod probe_send;
#[cfg(test)]
mod tests;

use concurrent_task::ConcurrentTaskInner;
use id::Generator;
use singleton_task::SingletonTaskInner;

type Result<T, E = DownloadError> = std::result::Result<T, E>;
