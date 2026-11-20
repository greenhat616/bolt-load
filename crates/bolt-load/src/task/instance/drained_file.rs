//! The temporary file of the singleton mode, written on `blocking` threads
//! that the attempt's panic handler can wait for (design §11.5.4).
//!
//! `async_fs::File` hands writes to a background worker that keeps the file
//! after the attempt is gone, and nothing tells when it closed it. Here
//! every operation runs as its own blocking task holding the file and a
//! clone of the attempt's drain group, and drops the file before the
//! clone. An operation still running when the attempt unwinds keeps the
//! drain open, so the destination's lock is released only after the file
//! is closed.

use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::Path,
    sync::Arc,
};

use async_waitgroup::WaitGroup;
use bytes::Bytes;

/// A value and a clone of a drain group, dropped in that order on every
/// path: when the holder returns, unwinds, or is dropped without running.
///
/// Struct fields drop in declaration order, while the separate captures of
/// a closure drop in an unspecified order (Rust reference, "Destructors").
/// So a closure that must close a file before it tells the drain moves one
/// `Drained` in whole: it binds it with `let held = held;` first, which
/// keeps the closure from capturing the fields one by one.
pub(crate) struct Drained<T> {
    pub(crate) value: T,
    _drain: Option<WaitGroup>,
}

impl<T> Drained<T> {
    pub(crate) fn new(value: T, drain: Option<WaitGroup>) -> Self {
        Self {
            value,
            _drain: drain,
        }
    }
}

/// What to call in each write before it touches the file; tests hold a
/// write there.
#[cfg(test)]
pub(crate) type BeforeWrite = Arc<dyn Fn() + Send + Sync>;

pub(crate) struct DrainedFile {
    file: Arc<File>,
    drain: WaitGroup,
    #[cfg(test)]
    before_write: Option<BeforeWrite>,
}

impl DrainedFile {
    /// Create or truncate the file at `path`.
    pub(crate) async fn create(path: &Path, drain: WaitGroup) -> io::Result<Self> {
        let path = path.to_path_buf();
        let guard = drain.clone();
        // The guard travels with the file: if this future is dropped while
        // the file is opened, the output is dropped on the blocking thread,
        // the file first.
        let (file, _guard) = blocking::unblock(move || {
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(path);
            (file, guard)
        })
        .await;
        let file = file?;
        Ok(Self {
            file: Arc::new(file),
            drain,
            #[cfg(test)]
            before_write: None,
        })
    }

    /// Set the file's length.
    pub(crate) async fn set_len(&self, len: u64) -> io::Result<()> {
        self.run(move |file| file.set_len(len)).await
    }

    /// Call `f` in each write before it touches the file.
    #[cfg(test)]
    pub(crate) fn before_write(&mut self, f: BeforeWrite) {
        self.before_write = Some(f);
    }

    /// Run `f` on the file on a blocking thread, holding the file and a
    /// drain clone until it returns.
    async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&File) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let held = Drained::new(self.file.clone(), Some(self.drain.clone()));
        // The file, if this was its last handle, is closed before the drain
        // is told: after `f` returns or panics, and when the blocking task
        // is dropped before it ran.
        blocking::unblock(move || {
            let held = held;
            f(&held.value)
        })
        .await
    }

    /// Append `data` at the file's position.
    pub(crate) async fn write_all(&self, data: Bytes) -> io::Result<()> {
        #[cfg(test)]
        let before_write = self.before_write.clone();
        self.run(move |mut file| {
            #[cfg(test)]
            if let Some(f) = before_write {
                f();
            }
            file.write_all(&data)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, time::Duration};

    use super::*;

    /// A write in flight when the attempt unwinds keeps the drain open
    /// until it reached the file and let go of it: dropping its future, as
    /// the unwind does, does not stop it.
    #[test]
    fn test_a_dropped_write_holds_the_drain_until_it_ends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.bin.downloading");
        let drain = WaitGroup::new();
        let mut file =
            futures::executor::block_on(DrainedFile::create(&path, drain.clone())).unwrap();
        futures::executor::block_on(file.set_len(4)).unwrap();
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        file.before_write(Arc::new(move || {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        }));

        {
            let mut write = std::pin::pin!(file.write_all(Bytes::from_static(b"data")));
            let waker = futures::task::noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            assert!(write.as_mut().poll(&mut cx).is_pending());
            entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            // The unwind drops the attempt: the write future and the file.
        }
        drop(file);

        // Polled here, not on a thread that may not have run yet: pending
        // while the write is held, ready once it let go of the file.
        // Boxed to move it to the thread that finishes it below.
        let mut wait = Box::pin(drain.wait());
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(
            wait.as_mut().poll(&mut cx).is_pending(),
            "the write is still running"
        );
        release_tx.send(()).unwrap();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            futures::executor::block_on(wait);
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the drain did not complete");
        assert_eq!(std::fs::read(&path).unwrap(), b"data");
    }

    /// The value of a [`Drained`] is dropped before its drain clone, both
    /// when the closure holding it panics and when the closure is dropped
    /// without running: the drain never completes while the value lives.
    #[test]
    fn test_a_drained_value_drops_before_its_drain() {
        use std::{
            pin::Pin,
            sync::atomic::{AtomicU8, Ordering},
        };

        const ALIVE: u8 = 0;
        const DRAIN_OPEN: u8 = 1;
        const DRAIN_DONE: u8 = 2;

        /// Records, when dropped, whether the drain was still open. It holds
        /// the drain's only waiter, so only the `Drained`'s clone keeps it
        /// open. Recorded, not asserted: a panic in a drop during an unwind
        /// would abort.
        struct Value {
            wait: Pin<Box<async_waitgroup::Wait>>,
            seen: Arc<AtomicU8>,
        }
        impl Drop for Value {
            fn drop(&mut self) {
                let waker = futures::task::noop_waker();
                let mut cx = std::task::Context::from_waker(&waker);
                let seen = if self.wait.as_mut().poll(&mut cx).is_pending() {
                    DRAIN_OPEN
                } else {
                    DRAIN_DONE
                };
                self.seen.store(seen, Ordering::SeqCst);
            }
        }

        /// A `Drained` value and its drain's only other reference.
        fn held() -> (Drained<Value>, Arc<AtomicU8>) {
            let drain = WaitGroup::new();
            let clone = drain.clone();
            let seen = Arc::new(AtomicU8::new(ALIVE));
            let value = Value {
                // Gives up the test's reference.
                wait: Box::pin(drain.wait()),
                seen: seen.clone(),
            };
            (Drained::new(value, Some(clone)), seen)
        }

        // A closure that panics while holding it.
        let (held_a, seen) = held();
        let run = move || {
            let held = held_a;
            let _ = &held.value;
            panic!("the worker panics");
        };
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).is_err());
        assert_eq!(seen.load(Ordering::SeqCst), DRAIN_OPEN, "after a panic");

        // A closure dropped without running, as a queued blocking task is.
        let (held_b, seen) = held();
        let run = move || {
            let held = held_b;
            let _ = &held.value;
        };
        drop(run);
        assert_eq!(seen.load(Ordering::SeqCst), DRAIN_OPEN, "dropped unrun");
    }
}
