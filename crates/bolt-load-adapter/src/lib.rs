use bolt_load_core::adapter::*;

#[cfg(feature = "reqwest")]
pub mod reqwest;

#[cfg(feature = "ureq2")]
#[allow(clippy::result_large_err)]
pub mod ureq2;
