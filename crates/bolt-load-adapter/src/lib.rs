use bolt_load_core::adapter::*;

#[cfg(feature = "reqwest")]
pub mod reqwest;

#[cfg(feature = "ureq2")]
#[expect(
    clippy::result_large_err,
    reason = "the module re-exports ureq's large error type"
)]
pub mod ureq2;
