//! The destination's lock (design §11.5.4, #152): one task at a time per
//! destination, the lock file gone after every outcome, a stale state
//! invalidated, and the lock released after cancellation and panics.

use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use bolt_load_tests::adapter::{fault::Gate, simple::SimpleTestAdapter};
use fs4::TryLockError;
use futures::StreamExt;
use smol_cancellation_token::CancellationToken;
use tempfile::TempDir;

use crate::{
    adapter::{AdapterError, BoltLoadAdapter, BoltLoadAdapterMeta, FullResponse},
    task::{
        DownloadError, DownloadMode, IoOp, Task, TaskOptions,
        instance::destination_lock::{LockHooks, state_path_for},
        test_support::{builder, spawn, tmp_path, watchdog},
    },
};

const SIZE: usize = 64 * 1024;

fn dest() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("file.bin");
    (dir, dest)
}

/// Neither the lock file nor the temporary file is left.
fn assert_clean(dest: &Path) {
    assert!(!state_path_for(dest).exists(), "the lock file is left");
    assert!(!tmp_path(dest).exists(), "the temporary file is left");
}

/// The content of a [`SimpleTestAdapter`], streamed in two halves with
/// `gate` between them: a task on it holds its destination until the test
/// releases the gate.
#[derive(Clone)]
struct GatedAdapter {
    inner: SimpleTestAdapter,
    gate: Gate,
}

impl GatedAdapter {
    fn new(size: usize) -> Self {
        Self {
            inner: SimpleTestAdapter::new(size),
            gate: Gate::new(),
        }
    }
}

impl BoltLoadAdapter for GatedAdapter {
    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError> {
        self.inner.retrieve_meta().await
    }

    async fn full_stream(&self) -> Result<FullResponse, AdapterError> {
        let content = self.inner.content().clone();
        let (first, second) = (
            content.slice(..content.len() / 2),
            content.slice(content.len() / 2..),
        );
        let gate = self.gate.clone();
        let body = futures::stream::once(async move { Ok(first) }).chain(futures::stream::once(
            async move {
                gate.pass().await;
                Ok(second)
            },
        ));
        Ok(FullResponse::new(Box::pin(body)))
    }
}

/// A singleton task on `gated` that holds its destination at the gate for
/// as long as the test needs: its stream idle timeout outlasts any
/// watchdog, so it does not end, and let go of the lock, on its own.
fn spawn_held(gated: GatedAdapter, dest: &Path, token: &CancellationToken) -> Task {
    builder(DownloadMode::Singleton, gated, dest, token)
        .options(TaskOptions::default().stream_idle_timeout(Duration::from_secs(3600)))
        .spawn()
        .expect("task should spawn")
}

/// A task on `dest` that finishes.
async fn download(dest: &Path) {
    let adapter = SimpleTestAdapter::new(SIZE);
    let content = adapter.content().clone();
    let token = CancellationToken::new();
    let output = watchdog(spawn(DownloadMode::Singleton, adapter, dest, &token))
        .await
        .unwrap();
    assert_eq!(output.path, dest);
    assert_eq!(std::fs::read(dest).unwrap(), content);
    assert_clean(dest);
}

/// #152: a second task on the destination of a running one fails with
/// `InUse`, without touching its temporary file; the first finishes
/// intact, and the destination is free again afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_second_task_on_the_same_destination_is_in_use() {
    let (_dir, dest) = dest();
    let gated = GatedAdapter::new(SIZE);
    let (content, gate) = (gated.inner.content().clone(), gated.gate.clone());
    let token = CancellationToken::new();
    let first = spawn_held(gated, &dest, &token);
    // Held at the gate, with its lock, until released below.
    watchdog(gate.entered()).await;

    for mode in [DownloadMode::Singleton, DownloadMode::Concurrent] {
        let second = spawn(mode, SimpleTestAdapter::new(SIZE / 2), &dest, &token);
        let result = watchdog(second).await;
        assert!(
            matches!(&result, Err(DownloadError::InUse { path }) if *path == dest),
            "{mode:?}: {result:?}"
        );
    }

    gate.release();
    let output = watchdog(first).await.unwrap();
    assert_eq!(output.path, dest);
    assert_eq!(std::fs::read(&dest).unwrap(), content);
    assert_clean(&dest);
    download(&dest).await;
}

/// Panics in its stream, inline in the singleton attempt.
#[derive(Clone)]
struct PanickingAdapter(SimpleTestAdapter);

impl BoltLoadAdapter for PanickingAdapter {
    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError> {
        self.0.retrieve_meta().await
    }

    async fn full_stream(&self) -> Result<FullResponse, AdapterError> {
        panic!("the adapter panicked");
    }
}

/// The lock file is gone after every outcome: success, failure,
/// cancellation and a panic; the next task on the destination runs.
#[tokio::test(flavor = "multi_thread")]
async fn test_the_lock_is_released_after_every_outcome() {
    let (_dir, dest) = dest();
    download(&dest).await;

    let token = CancellationToken::new();
    let failing = SimpleTestAdapter::new(SIZE).with_failure(true);
    let result = watchdog(spawn(DownloadMode::Singleton, failing, &dest, &token)).await;
    assert!(result.is_err(), "{result:?}");
    assert_clean(&dest);
    download(&dest).await;

    let gated = GatedAdapter::new(SIZE);
    let gate = gated.gate.clone();
    let token = CancellationToken::new();
    let task = spawn_held(gated, &dest, &token);
    watchdog(gate.entered()).await;
    token.cancel();
    assert!(matches!(
        watchdog(task).await,
        Err(DownloadError::Cancelled)
    ));
    assert_clean(&dest);
    download(&dest).await;

    let token = CancellationToken::new();
    let panicking = PanickingAdapter(SimpleTestAdapter::new(SIZE));
    let result = watchdog(spawn(DownloadMode::Singleton, panicking, &dest, &token)).await;
    assert!(
        matches!(result, Err(DownloadError::Panicked { .. })),
        "{result:?}"
    );
    assert_clean(&dest);
    download(&dest).await;
}

/// A state left by an earlier task, and its temporary file, do not get in
/// the way: the task invalidates the state, downloads from scratch, and
/// leaves neither behind.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_stale_state_is_invalidated() {
    let (_dir, dest) = dest();
    std::fs::write(state_path_for(&dest), b"a state from an earlier task").unwrap();
    std::fs::write(tmp_path(&dest), vec![0xAA; SIZE * 2]).unwrap();
    // Held at the gate, to look at the files while it downloads.
    let gated = GatedAdapter::new(SIZE);
    let (content, gate) = (gated.inner.content().clone(), gated.gate.clone());
    let token = CancellationToken::new();
    let task = spawn_held(gated, &dest, &token);
    watchdog(gate.entered()).await;
    // Emptied under the lock before the download started.
    assert_eq!(std::fs::metadata(state_path_for(&dest)).unwrap().len(), 0);
    gate.release();
    watchdog(task).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), content);
    assert_clean(&dest);
    std::fs::remove_file(&dest).unwrap();

    // A crash leaves an empty, unlocked lock file.
    std::fs::File::create(state_path_for(&dest)).unwrap();
    download(&dest).await;
}

/// Cancelled while the lock is being taken: the task still releases it,
/// removes the temporary file an earlier task left (it owns it once it
/// holds the lock), and the destination is free again.
#[tokio::test(flavor = "multi_thread")]
async fn test_cancelled_while_locking() {
    let (_dir, dest) = dest();
    std::fs::write(tmp_path(&dest), b"an earlier task's partial file").unwrap();
    let token = CancellationToken::new();
    let task = builder(
        DownloadMode::Singleton,
        SimpleTestAdapter::new(SIZE),
        &dest,
        &token,
    )
    .lock_hooks(LockHooks::default().before_lock({
        let token = token.clone();
        move |_| token.cancel()
    }))
    .spawn()
    .unwrap();
    let status = task.status();
    assert!(matches!(
        watchdog(task).await,
        Err(DownloadError::Cancelled)
    ));
    assert_eq!(status.borrow().leftover_temp_file, None);
    assert_clean(&dest);
    download(&dest).await;
}

/// A file system that cannot lock: the task goes on unlocked. Any other
/// error, such as a missing permission, fails it with `Io { op: Lock }`,
/// not `InUse`, before anything is written. Either way the lock file stays,
/// empty: without the lock, the task does not remove it.
#[tokio::test(flavor = "multi_thread")]
async fn test_lock_failures() {
    let (_dir, dest) = dest();
    let unsupported = || {
        #[cfg(unix)]
        let code = libc::ENOLCK;
        #[cfg(windows)]
        let code = 50; // ERROR_NOT_SUPPORTED
        io::Error::from_raw_os_error(code)
    };
    let adapter = SimpleTestAdapter::new(SIZE);
    let content = adapter.content().clone();
    let token = CancellationToken::new();
    let output = watchdog(
        builder(DownloadMode::Singleton, adapter, &dest, &token)
            .lock_hooks(
                LockHooks::default().try_lock(move |_| Err(TryLockError::Error(unsupported()))),
            )
            .spawn()
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(output.path, dest);
    assert_eq!(std::fs::read(&dest).unwrap(), content);
    assert!(!tmp_path(&dest).exists());
    assert_eq!(std::fs::metadata(state_path_for(&dest)).unwrap().len(), 0);
    std::fs::remove_file(&dest).unwrap();

    let result = watchdog(
        builder(
            DownloadMode::Singleton,
            SimpleTestAdapter::new(SIZE),
            &dest,
            &token,
        )
        .lock_hooks(LockHooks::default().try_lock(|_| {
            Err(TryLockError::Error(io::Error::from(
                io::ErrorKind::PermissionDenied,
            )))
        }))
        .spawn()
        .unwrap(),
    )
    .await;
    assert!(
        matches!(&result, Err(DownloadError::Io { op: IoOp::Lock, path, .. }) if *path == state_path_for(&dest)),
        "{result:?}"
    );
    assert!(!tmp_path(&dest).exists());
    assert_eq!(std::fs::metadata(state_path_for(&dest)).unwrap().len(), 0);
    assert!(!dest.exists());
}
