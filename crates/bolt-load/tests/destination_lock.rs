//! The destination's lock holds across processes (design §11.5.4, #152):
//! while another process downloads to a destination, a task here fails
//! with `InUse`; once that process is killed, the operating system
//! releases its lock, and a task here takes over the destination.

#![cfg(feature = "tokio")]

use std::{
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

use bolt_load::{
    adapter::{AdapterError, BoltLoadAdapter, BoltLoadAdapterMeta, FullResponse, RetryableError},
    runtime::ThreadedRuntimeImpl,
    task::{DownloadError, DownloadMode, Task, TaskOptions, TaskState},
};
use bolt_load_tests::adapter::simple::{SimpleTestAdapter, SimpleTestAdapterBuilder};

/// Set in the child process to the destination it downloads to.
const CHILD: &str = "BOLT_LOAD_LOCK_CHILD";
/// What the child prints once it holds the destination.
const LOCKED: &str = "BOLT_LOAD_LOCKED";

fn task(adapter: impl BoltLoadAdapter + 'static, dest: &Path) -> Task {
    Task::builder(adapter)
        .runtime(ThreadedRuntimeImpl::new_tokio_rt())
        .save_to(dest)
        .prefer_mode(DownloadMode::Singleton)
        // The child holds the destination until the parent lets go, so its
        // stream must not time out first.
        .options(TaskOptions::default().stream_idle_timeout(Duration::from_secs(3600)))
        .spawn()
        .unwrap()
}

/// Run only in the child process the parent test starts: download to the
/// destination it is given, say so once the download started, and hold it
/// there until the parent closes the child's input.
#[test]
fn child_holds_the_destination() {
    let Some(dest) = std::env::var_os(CHILD) else {
        return;
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // The body waits for the parent, and the task holds the lock
        // meanwhile.
        let adapter = SimpleTestAdapterBuilder::new()
            .content_size(1 << 20)
            .build()
            .unwrap();
        let task = task(StdinGated(adapter), Path::new(&dest));
        let mut status = task.status();
        status
            .wait_for(|status| status.state == TaskState::Downloading)
            .await
            .unwrap();
        println!("{LOCKED}");
        let _ = task.await;
    });
}

/// A body that waits for the parent: it ends with an error once the child's
/// input is closed.
struct StdinGated(SimpleTestAdapter);

impl BoltLoadAdapter for StdinGated {
    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, AdapterError> {
        self.0.retrieve_meta().await
    }

    async fn full_stream(&self) -> Result<FullResponse, AdapterError> {
        let body = futures::stream::once(async {
            blocking::unblock(|| {
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
            })
            .await;
            Err(AdapterError::from(RetryableError::Io {
                source: std::sync::Arc::new(std::io::Error::other("the parent let go")),
            }))
        });
        Ok(FullResponse::new(Box::pin(body)))
    }
}

/// Kills the child when dropped, so a failing test leaves no process.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn another_process_holds_the_destination() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("file.bin");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "child_holds_the_destination",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, &dest)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut child = KillOnDrop(child);
    let started = tokio::task::spawn_blocking(move || {
        BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .any(|line| line.contains(LOCKED))
    });
    let started = tokio::time::timeout(Duration::from_secs(60), started)
        .await
        .expect("the child did not start downloading")
        .unwrap();
    assert!(started, "the child exited before downloading");

    let result = tokio::time::timeout(
        Duration::from_secs(60),
        task(SimpleTestAdapter::new(1024), &dest),
    )
    .await
    .expect("the contender did not finish");
    assert!(
        matches!(&result, Err(DownloadError::InUse { path }) if *path == dest),
        "{result:?}"
    );

    child.0.kill().unwrap();
    child.0.wait().unwrap();
    // The killed process left its temporary file and its lock file; the
    // lock went with the process.
    let adapter = SimpleTestAdapter::new(64 * 1024);
    let content = adapter.content().clone();
    let output = tokio::time::timeout(Duration::from_secs(60), task(adapter, &dest))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.path, dest);
    assert_eq!(std::fs::read(&dest).unwrap(), content);
    let mut lock = dest.clone().into_os_string();
    lock.push(".boltload");
    assert!(!Path::new(&lock).exists());
}
