pub mod http;
pub mod logger;
pub mod reader;

/// A guard that ensures the task is shutdown
pub struct ShutdownGuard(Option<oneshot::Sender<()>>);

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

pub trait ShutdownGuardExt {
    fn shutdown_guard(self) -> ShutdownGuard;
}

impl ShutdownGuardExt for oneshot::Sender<()> {
    fn shutdown_guard(self) -> ShutdownGuard {
        ShutdownGuard(Some(self))
    }
}
