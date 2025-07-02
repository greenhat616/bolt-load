#[cfg(feature = "tracing")]
pub use tracing::{debug, error, info, trace, warn};

#[cfg(not(feature = "tracing"))]
macro_rules! info {
    ($($arg:tt)*) => {};
}

#[cfg(not(feature = "tracing"))]
macro_rules! warn {
    ($($arg:tt)*) => {};
}

#[cfg(not(feature = "tracing"))]
macro_rules! error {
    ($($arg:tt)*) => {};
}

#[cfg(not(feature = "tracing"))]
macro_rules! debug {
    ($($arg:tt)*) => {};
}

#[cfg(not(feature = "tracing"))]
macro_rules! trace {
    ($($arg:tt)*) => {};
}
