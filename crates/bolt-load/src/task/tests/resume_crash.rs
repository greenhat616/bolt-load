//! A process that crashes while it downloads (design §11.5.11): its lock
//! holds against other processes until it dies, and what it left resumes
//! here, wherever it was killed.
//!
//! The test runs this test binary again as the child, which downloads with
//! a step held at a gate, says so on its output, and is killed there: no
//! cleanup runs, as in a crash. Nothing waits on time.

use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

use smol_cancellation_token::CancellationToken;

use super::resume_state::{BLOCK, Fault, SIZE, Source, assert_unlocked, dest, finish, resumable};
use crate::task::{
    DownloadError, DownloadMode, Resume, TaskState,
    instance::{
        destination_lock::state_path_for,
        file_steps::{
            Step,
            tests::{StepLog, When},
        },
        state_file::tests::{decode_header, decode_records},
    },
    test_support::{builder, tmp_path},
};

/// Set in the child to the destination it downloads to.
const CHILD: &str = "BOLT_LOAD_RESUME_CHILD";
/// Set in the child to the place it is killed at, an index of [`POINTS`].
const POINT: &str = "BOLT_LOAD_RESUME_POINT";
/// What the child prints once it is held there.
const HELD: &str = "BOLT_LOAD_RESUME_HELD";

/// Where the child is killed.
#[derive(Debug, Clone, Copy)]
enum Kill {
    /// Held at this step, while it downloads and holds the destination.
    At(Step, When),
    /// Once its task ended, its rename having failed: both files kept.
    AfterFailedRename,
}

/// Where the child is killed, and how many blocks its state then records.
/// The first three are the places of design §11.5.11 G9: before the first
/// checkpoint, during one, and after a failed rename. Every download takes
/// at least one checkpoint, the one at its teardown, so each place is
/// reached however fast the download goes.
const POINTS: [(Kill, std::ops::RangeInclusive<usize>); 4] = [
    // Before the first checkpoint writes its records: the header only.
    (Kill::At(Step::WriteState, When::Nth(2)), 0..=0),
    // During the first checkpoint: its records are written, their sync
    // not begun; the operating system keeps what was written.
    (Kill::At(Step::SyncState, When::Nth(2)), 1..=4),
    // After a failed rename: every block recorded, the file complete.
    (Kill::AfterFailedRename, 4..=4),
    // Before the rename: every block recorded, the file complete.
    (Kill::At(Step::Rename, When::Nth(1)), 4..=4),
];

/// Run only in the child the parent test starts: download to the
/// destination it is given, hold the step it is given, say so, and wait
/// there to be killed.
#[test]
fn resume_child() {
    let Some(dest) = std::env::var_os(CHILD) else {
        return;
    };
    let point: usize = std::env::var(POINT).unwrap().parse().unwrap();
    let (kill, _) = POINTS[point];
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let log = StepLog::default();
        match kill {
            Kill::At(step, when) => {
                // Held until the process is killed: the release is never
                // sent, and the sender lives as long as the process.
                let (reached, release) = log.hold(step, when);
                let task = resumable(
                    Source::new(Fault::None),
                    Path::new(&dest),
                    &log,
                    &CancellationToken::new(),
                );
                tokio::task::spawn_blocking(move || reached.recv().unwrap())
                    .await
                    .unwrap();
                println!("{HELD}");
                std::io::stdout().flush().unwrap();
                let _ = task.await;
                drop(release);
            }
            Kill::AfterFailedRename => {
                log.fail(Step::Rename, When::Nth(1));
                let status = finish(resumable(
                    Source::new(Fault::None),
                    Path::new(&dest),
                    &log,
                    &CancellationToken::new(),
                ))
                .await;
                assert_eq!(status.kept_for_resume, Some(tmp_path(Path::new(&dest))));
                println!("{HELD}");
                std::io::stdout().flush().unwrap();
                // Killed here, the process idle.
                loop {
                    std::thread::park();
                }
            }
        }
    });
}

/// Kills the child when dropped, so a failing test leaves no process.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start the child for `point`, downloading to `dest`, and wait until it
/// is held there.
async fn held_child(dest: &Path, point: usize) -> KillOnDrop {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "task::tests::resume_crash::resume_child",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, dest)
        .env(POINT, point.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let child = KillOnDrop(child);
    let held = tokio::task::spawn_blocking(move || {
        BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .any(|line| line.contains(HELD))
    });
    let held = tokio::time::timeout(Duration::from_secs(60), held)
        .await
        .expect("the child was not held in time")
        .unwrap();
    assert!(held, "the child exited before it was held");
    child
}

/// A child held at each point while it downloads holds the destination
/// against a task here, with resume on or off; killed there, or once its
/// task ended after a failed rename, it leaves a pair that resumes here
/// from every block it recorded, and the file completes.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_crashed_process_leaves_a_pair_that_resumes() {
    for (point, (kill, recorded)) in POINTS.iter().enumerate() {
        let step = kill;
        let (_dir, dest) = dest();
        let mut child = held_child(&dest, point).await;
        // A child whose task ended holds nothing.
        let holds = matches!(kill, Kill::At(..));
        for resume in [Resume::On, Resume::Off].into_iter().filter(|_| holds) {
            let status = finish(
                builder(
                    DownloadMode::Concurrent,
                    Source::new(Fault::None),
                    &dest,
                    &CancellationToken::new(),
                )
                .meta(super::resume_state::meta())
                .resume(resume)
                .spawn()
                .unwrap(),
            )
            .await;
            assert!(
                matches!(&status.error, Some(DownloadError::InUse { path }) if *path == dest),
                "{step:?}, {resume:?}: {:?}",
                status.error
            );
        }
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        // What the crash left: a valid header and the records synced.
        let state = std::fs::read(state_path_for(&dest)).unwrap();
        let header = decode_header(&state).expect("a valid header");
        let records = decode_records(&state, header.len).len();
        assert!(recorded.contains(&records), "{step:?}: {records} records");
        assert!(tmp_path(&dest).exists(), "{step:?}");
        let source = Source::new(Fault::None);
        let (content, requested) = (source.content.clone(), source.requested.clone());
        let status = finish(resumable(
            source,
            &dest,
            &StepLog::default(),
            &CancellationToken::new(),
        ))
        .await;
        assert_eq!(
            status.state,
            TaskState::Finished,
            "{step:?}: {:?}",
            status.error
        );
        assert_eq!(std::fs::read(&dest).unwrap(), content, "{step:?}");
        let report = status
            .info
            .as_ref()
            .and_then(|info| info.resumed.clone())
            .expect("resumed");
        assert_eq!(report.dropped, [], "{step:?}");
        let kept = report.kept_bytes;
        assert_eq!(
            kept,
            (records as u64 * BLOCK).min(SIZE),
            "{step:?}: every recorded block kept"
        );
        let fetched: u64 = requested
            .lock()
            .unwrap()
            .iter()
            .map(|range| range.end - range.start)
            .sum();
        assert_eq!(fetched, SIZE - kept, "{step:?}: only the rest requested");
        assert!(!tmp_path(&dest).exists() && !state_path_for(&dest).exists());
        assert_unlocked(&dest);
    }
}
