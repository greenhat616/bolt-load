//! The two transfer modes of a download attempt and the lifecycle that drives
//! them, see [`drive`].

use super::DownloadError;

/// Concurrent download task implementation.
pub mod concurrent_task;
pub(crate) mod drive;
mod id;
mod output_file;
mod sampler;
mod singleton_task;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod status_contract_tests;

#[cfg(test)]
mod integration_test;
#[cfg(all(test, bolt_load_probe_send))]
mod probe_send;

use concurrent_task::ConcurrentTaskInner;
use id::Generator;
use singleton_task::SingletonTaskInner;

type Result<T, E = DownloadError> = std::result::Result<T, E>;
