//! The file steps of a resumable task's start, completion and discard
//! (design §11.5.5), which tests log in order and fail one at a time.
//!
//! A task with `Resume::Off` takes none of these steps: its files are
//! handled as before C33.

use std::{fmt, io, sync::Arc};

use super::state_file::{LockedFile, StateIo};

/// One file step of design §11.5.5, in the order a task can take them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// Empty the state file through the locked handle, before a fallback
    /// and first in a discard.
    Invalidate,
    /// Sync the state file after it was emptied.
    SyncInvalidation,
    /// Create or truncate the partial file and size it to the total.
    CreatePartial,
    /// Sync the partial file.
    SyncPartial,
    /// Write to the state file: its header, or the records of a
    /// checkpoint.
    WriteState,
    /// Sync the state file after a write or a cut.
    SyncState,
    /// Sync the directory after the new start created both files.
    SyncDirAfterStart,
    /// Rename the partial file to the destination.
    Rename,
    /// Sync the destination's directory after the rename.
    SyncDirAfterRename,
    /// Cut the state file: to nothing after the rename, or to its header
    /// before a resume writes back the records it keeps.
    TruncateState,
    /// Remove the partial file in a discard.
    RemovePartial,
    /// Sync the directory after the partial file was removed.
    SyncDirAfterPartial,
    /// Remove the state file, while the lock is still held.
    RemoveState,
    /// Sync the directory after the state file was removed.
    SyncDirAfterState,
    /// Read the state file a resume continues from.
    ReadState,
    /// Read a block of the partial file to check it before a resume.
    ReadPartial,
    /// Size the partial file to the total, after a resume wrote back the
    /// records it keeps.
    SizePartial,
}

type Hook = dyn Fn(Step) -> io::Result<()> + Send + Sync;

/// What to call before each step; `Err` fails the step without taking it.
/// Production calls nothing.
#[derive(Clone, Default)]
pub(crate) struct FileHooks(Option<Arc<Hook>>);

impl fmt::Debug for FileHooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FileHooks")
            .field(&self.0.as_ref().map(|_| ".."))
            .finish()
    }
}

impl FileHooks {
    /// Call `f` before each step.
    #[cfg(test)]
    pub(crate) fn new(f: impl Fn(Step) -> io::Result<()> + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(f)))
    }

    /// Run the hook of `step`, if any.
    pub(crate) fn before(&self, step: Step) -> io::Result<()> {
        match &self.0 {
            Some(hook) => hook(step),
            None => Ok(()),
        }
    }
}

/// The state file's locked handle, each write and sync announced to the
/// hooks first.
pub(crate) struct HookedFile {
    pub(crate) file: LockedFile,
    pub(crate) hooks: FileHooks,
}

impl StateIo for HookedFile {
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.hooks.before(Step::WriteState)?;
        self.file.write_at(offset, bytes)
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.hooks.before(Step::TruncateState)?;
        self.file.set_len(len)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.hooks.before(Step::SyncState)?;
        self.file.sync()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Mutex, mpsc};

    use super::*;

    /// Which time a step fails or is held.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum When {
        /// The n-th time the step comes, counted from 1.
        Nth(usize),
        /// The first time it comes after `Step` came.
        After(Step),
    }

    type GateState = (Step, When, mpsc::Sender<()>, mpsc::Receiver<()>);

    /// Logs every step in order, fails the steps named, and holds one at a
    /// gate.
    #[derive(Clone, Default)]
    pub(crate) struct StepLog {
        log: Arc<Mutex<Vec<Step>>>,
        fail: Arc<Mutex<Vec<(Step, When)>>>,
        gate: Arc<Mutex<Option<GateState>>>,
        panic: Arc<Mutex<Option<(Step, When)>>>,
    }

    impl StepLog {
        pub(crate) fn hooks(&self) -> FileHooks {
            let this = self.clone();
            FileHooks::new(move |step| this.before(step))
        }

        /// Fail `step` `when` it comes.
        pub(crate) fn fail(&self, step: Step, when: When) -> &Self {
            self.fail.lock().unwrap().push((step, when));
            self
        }

        /// Panic, once, at `step` `when` it comes, after logging it.
        pub(crate) fn panic(&self, step: Step, when: When) -> &Self {
            *self.panic.lock().unwrap() = Some((step, when));
            self
        }

        /// The steps taken or tried, in order.
        pub(crate) fn steps(&self) -> Vec<Step> {
            self.log.lock().unwrap().clone()
        }

        /// Hold `step` `when` it comes, until the returned sender sends or
        /// drops; the receiver reports that it was reached.
        pub(crate) fn gate(
            &self,
            step: Step,
            when: When,
        ) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
            let (entered_tx, entered) = mpsc::channel();
            let (release, release_rx) = mpsc::channel();
            *self.gate.lock().unwrap() = Some((step, when, entered_tx, release_rx));
            (entered, release)
        }

        /// Whether `step`, as the log stands before it is added, comes
        /// `when`.
        fn matches(log: &[Step], step: Step, when: When) -> bool {
            match when {
                When::Nth(n) => log.iter().filter(|&&s| s == step).count() + 1 == n,
                When::After(anchor) => match log.iter().rposition(|&s| s == anchor) {
                    Some(at) => !log[at + 1..].contains(&step),
                    None => false,
                },
            }
        }

        fn before(&self, step: Step) -> io::Result<()> {
            let held = {
                let log = self.log.lock().unwrap();
                let mut gate = self.gate.lock().unwrap();
                match gate.as_ref() {
                    Some((gated, when, ..))
                        if *gated == step && Self::matches(&log, step, *when) =>
                    {
                        gate.take()
                    }
                    _ => None,
                }
            };
            if let Some((.., entered, release)) = held {
                let _ = entered.send(());
                let _ = release.recv_timeout(std::time::Duration::from_secs(30));
            }
            let mut log = self.log.lock().unwrap();
            let fails = self
                .fail
                .lock()
                .unwrap()
                .iter()
                .any(|&(failed, when)| failed == step && Self::matches(&log, step, when));
            let panics = {
                let mut panic = self.panic.lock().unwrap();
                match *panic {
                    Some((at, when)) if at == step && Self::matches(&log, step, when) => {
                        panic.take().is_some()
                    }
                    _ => false,
                }
            };
            log.push(step);
            drop(log);
            assert!(!panics, "injected panic at {step:?}");
            if fails {
                Err(io::Error::other(format!("injected failure of {step:?}")))
            } else {
                Ok(())
            }
        }
    }
}
