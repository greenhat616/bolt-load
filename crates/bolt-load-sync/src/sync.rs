//! The synchronization primitives of this crate, replaced by loom's under
//! `cfg(loom)` so the model checker can explore their interleavings.

/// Let another thread run; loom needs it to leave a spin.
#[cfg(not(loom))]
pub(crate) use std::hint::spin_loop as yield_now;
#[cfg(not(loom))]
pub(crate) use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

#[cfg(loom)]
pub(crate) use loom::{
    cell::UnsafeCell,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread::yield_now,
};

/// `std`'s `UnsafeCell` behind loom's closure-based interface.
#[cfg(not(loom))]
#[derive(Debug)]
pub(crate) struct UnsafeCell<T>(std::cell::UnsafeCell<T>);

#[cfg(not(loom))]
impl<T> UnsafeCell<T> {
    pub(crate) const fn new(value: T) -> Self {
        Self(std::cell::UnsafeCell::new(value))
    }

    pub(crate) fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
        f(self.0.get())
    }
}
