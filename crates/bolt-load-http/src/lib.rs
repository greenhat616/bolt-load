//! HTTP helpers for bolt-load adapters.
//!
//! The task relies on an HTTP adapter to classify statuses, to prove that a
//! range response carries the requested bytes, and to report the content
//! codings of a full response. The functions here work on plain header
//! values, so an adapter can use them with any HTTP client, and the task
//! uses [`codings_to_decode`] on what the adapter reports. [`header`] parses
//! the headers the adapters read.

mod coding;
pub mod header;
mod range;
mod status;

pub use coding::{ContentCoding, codings_to_decode};
pub use range::{RangeHeaders, limit_range_body, validate_range_response};
pub use status::{MAX_RETRY_AFTER, classify_status};
