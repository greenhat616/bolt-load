pub mod adapter;
mod builder;
pub mod client;
pub mod runner;
pub mod runtime;
pub mod task;
mod utils;

pub use builder::*;
pub use client::*;

/// The extension of the downloading tmp file
pub const DOWNLOADING_TMP_EXTENSION: &str = ".downloading";

/// The default capacity of the event channel
///
/// It is used to send the event to the manager
const DEFAULT_EVENT_CHANNEL_CAPACITY: usize = 512; // 48 * 512 = 24KB for each runner
