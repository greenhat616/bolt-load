//! Resuming against a real server, on every client (design §11.5.11, G9):
//! a task cancelled at a byte position that a seed picks resumes with a new
//! adapter, downloads only what it did not keep, and ends with the whole
//! file. What a task kept can also be discarded.

#![cfg(all(feature = "reqwest", feature = "ureq2", feature = "tokio"))]

mod common;

use std::{path::Path, time::Duration};

use bolt_load::{
    runtime::ThreadedRuntimeImpl,
    task::{DownloadError, Resume, Task, TaskState, TaskStatus, discard_partial},
};
use bolt_load_tests::adapter::{
    fault::{Gate, RangeFault},
    http_server::create_range_fault_server_held_at,
    simple::{calculate_blake3, create_deterministic_content},
};
use bytes::Bytes;
use common::Client;
use smol_cancellation_token::CancellationToken;
use url::Url;

/// The blocks a state file records.
const BLOCK: u64 = 1 << 20;
/// Three blocks and a short one.
const SIZE: usize = 3 * BLOCK as usize + 12_345;
/// What a runner may hold back of the bytes it received, short of a
/// frame, with room to spare.
const HELD_BACK: u64 = 64 << 10;
/// The seeds; every client runs each one.
const SEEDS: u64 = 21;

/// A byte position within the file, from `seed`, never the first: the
/// range probe asks for that one.
fn position(seed: u64) -> u64 {
    // SplitMix64, one step.
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    1 + z % (SIZE as u64 - 1)
}

fn tmp(dest: &Path) -> std::path::PathBuf {
    let mut tmp = dest.to_owned().into_os_string();
    tmp.push(bolt_load::DOWNLOADING_TMP_EXTENSION);
    tmp.into()
}

fn state(dest: &Path) -> std::path::PathBuf {
    let mut state = dest.to_owned().into_os_string();
    state.push(".boltload");
    state.into()
}

/// A task on `client` for `url` that may resume, saving to `dest`.
fn task(client: Client, url: &Url, dest: &Path, token: &CancellationToken) -> Task {
    Task::builder(client.adapter(url.clone()))
        .runtime(ThreadedRuntimeImpl::new_tokio_rt())
        .save_to(dest)
        .resume(Resume::On)
        .cancel_token(token.clone())
        .spawn()
        .unwrap()
}

/// Wait for `task` and its last status.
async fn end(
    task: Task,
) -> (
    Result<bolt_load::task::TaskOutput, DownloadError>,
    TaskStatus,
) {
    let status = task.status();
    let result = tokio::time::timeout(Duration::from_secs(60), task)
        .await
        .expect("the task did not end");
    let last = status.borrow().as_ref().clone();
    (result, last)
}

/// Whether `written`, the ranges a task wrote, holds a whole block of
/// `BLOCK` bytes, one the checkpoint at its teardown records: not the
/// short last block, so that what is kept is at least `BLOCK`.
fn holds_a_block(written: &[std::ops::Range<u64>]) -> bool {
    written.iter().any(|range| {
        let first = range.start.div_ceil(BLOCK) * BLOCK;
        first + BLOCK <= range.end
    })
}

/// Cancel a task once the range that holds byte `position` reached it, on a
/// server that holds it there; the task keeps what it wrote. When
/// `position` lies past the first block by more than a runner holds back,
/// every byte before it is written in the end, the first block with them,
/// so the cancellation waits until the task reports a whole block written:
/// the task keeps at least one block.
async fn cancelled_at(
    client: Client,
    content: &Bytes,
    position: u64,
    dest: &Path,
) -> (Url, tokio::task::JoinHandle<()>, bool) {
    let gate = Gate::new();
    let (port, server, _log) =
        create_range_fault_server_held_at(content.clone(), position, gate.clone())
            .await
            .unwrap();
    let url = Url::parse(&format!(
        "http://127.0.0.1:{port}{}",
        RangeFault::Stable.path()
    ))
    .unwrap();
    let token = CancellationToken::new();
    let first = task(client, &url, dest, &token);
    let mut status = first.status();
    tokio::time::timeout(Duration::from_secs(60), gate.entered())
        .await
        .expect("the position was not reached");
    let keeps_a_block = position >= BLOCK + HELD_BACK;
    if keeps_a_block {
        tokio::time::timeout(
            Duration::from_secs(60),
            status.wait_for(|status| holds_a_block(&status.progress.downloaded_chunks)),
        )
        .await
        .expect("no whole block was written")
        .unwrap();
    }
    token.cancel();
    let (result, status) = end(first).await;
    gate.release();
    assert!(
        matches!(result, Err(DownloadError::Cancelled)),
        "{client:?} at {position}: {result:?}"
    );
    assert_eq!(status.state, TaskState::Cancelled);
    assert_eq!(
        status.kept_for_resume,
        Some(tmp(dest)),
        "{client:?} at {position}"
    );
    assert!(state(dest).exists());
    (url, server, keeps_a_block)
}

/// G9: for every seed, a task cancelled at the seed's position resumes on
/// a new adapter and ends with the whole file; nothing is left beside it.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_download_resumes_on_every_client() {
    let content = Bytes::from(create_deterministic_content(SIZE));
    let hash = calculate_blake3(&content);
    let mut kept_any = [false; Client::ALL.len()];
    for (seed, (index, client)) in
        (0..SEEDS).flat_map(|seed| Client::ALL.into_iter().enumerate().map(move |c| (seed, c)))
    {
        let position = position(seed);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("file.bin");
        let (url, server, keeps_a_block) = cancelled_at(client, &content, position, &dest).await;
        let (result, status) = end(task(client, &url, &dest, &CancellationToken::new())).await;
        server.abort();
        let context = format!("seed {seed}, {client:?} at {position}");
        result.unwrap_or_else(|e| panic!("{context}: {e}"));
        assert_eq!(
            calculate_blake3(&std::fs::read(&dest).unwrap()),
            hash,
            "{context}"
        );
        let info = status.info.as_ref().unwrap();
        let report = info.resumed.as_ref().expect("resumed");
        assert!(report.kept_bytes < SIZE as u64, "{context}: {report:?}");
        if keeps_a_block {
            assert!(report.kept_bytes >= BLOCK, "{context}: {report:?}");
            kept_any[index] = true;
        }
        assert!(info.resumable, "{context}");
        assert!(!tmp(&dest).exists() && !state(&dest).exists(), "{context}");
    }
    assert_eq!(
        kept_any,
        [true; Client::ALL.len()],
        "on every client, some seed lies past the first block and kept one"
    );
}

/// What a cancelled task kept is discarded with `discard_partial`, and the
/// next task starts anew.
#[tokio::test(flavor = "multi_thread")]
async fn what_was_kept_can_be_discarded() {
    let content = Bytes::from(create_deterministic_content(SIZE));
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("file.bin");
    let (url, server, _) = cancelled_at(Client::Reqwest, &content, position(7), &dest).await;
    discard_partial(&dest).await.unwrap();
    assert!(!tmp(&dest).exists() && !state(&dest).exists());
    let (result, status) = end(task(
        Client::Reqwest,
        &url,
        &dest,
        &CancellationToken::new(),
    ))
    .await;
    server.abort();
    result.unwrap();
    assert!(status.info.as_ref().unwrap().resumed.is_none());
    assert_eq!(std::fs::read(&dest).unwrap(), content);
}
