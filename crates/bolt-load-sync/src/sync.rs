//! The synchronization primitives of this crate, replaced by loom's under
//! `cfg(loom)` so the model checker can explore their interleavings.

#[cfg(not(loom))]
pub(crate) use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicU64, Ordering},
};

#[cfg(loom)]
pub(crate) use loom::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicU64, Ordering},
};
