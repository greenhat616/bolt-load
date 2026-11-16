// Whole tasks run on a local runtime: the suites built on the tokio one need
// its feature.
#[cfg(any(feature = "tokio", feature = "smol"))]
mod comprehensive;
#[cfg(feature = "tokio")]
mod destination_lock;
#[cfg(feature = "tokio")]
mod meta_retry;
#[cfg(feature = "tokio")]
mod resume_crash;
#[cfg(feature = "tokio")]
mod resume_from;
#[cfg(feature = "tokio")]
mod resume_state;
#[cfg(feature = "tokio")]
mod termination;
