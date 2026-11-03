//! HTTP helpers for bolt-load adapters.
//!
//! The task relies on an HTTP adapter to classify statuses, to prove that a
//! range response carries the requested bytes, and to keep content codings
//! out of the file. The functions here do that on plain header values, so an
//! adapter can use them with any HTTP client. [`header`] parses the headers
//! the adapters read.

mod coding;
pub mod header;
mod range;
mod status;

pub use coding::{check_content_coding, is_identity_coding};
pub use range::{RangeHeaders, limit_range_body, validate_range_response};
pub use status::{MAX_RETRY_AFTER, classify_status};
