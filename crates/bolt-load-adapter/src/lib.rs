use bolt_load_core::adapter::*;

#[cfg(feature = "reqwest")]
pub mod reqwest;

#[cfg(feature = "ureq2")]
pub mod ureq2;
