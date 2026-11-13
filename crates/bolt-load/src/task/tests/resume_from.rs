//! Resuming from a state file (design §11.5.10, §11.5.5 "续传", §11.5.9
//! B2 and C): what is kept, what is dropped and why, what is rejected, and
//! what each failure, cancellation and panic leaves.

#![expect(
    clippy::single_range_in_vec_init,
    reason = "lists of the ranges kept or requested, one of them"
)]

use std::path::Path;

use bolt_load_core::adapter::{UnretryableError, Validator};
use smol_cancellation_token::CancellationToken;

use super::resume_state::{
    BLOCK, COMPLETION, Fault, NEW_START, SIZE, Source, assert_checkpoints, assert_kept_state,
    assert_unlocked, dest, entered, finish, meta, resumable,
};
use crate::{
    adapter::BoltLoadAdapterMeta,
    task::{
        DownloadError, DownloadMode, DropReason, DroppedRecord, IoOp, RejectReason, TaskState,
        TaskStatus,
        instance::{
            destination_lock::state_path_for,
            file_steps::{
                Step,
                tests::{StepLog, When},
            },
            state_file::{
                RECORD_LEN, encode_record,
                tests::{decode_header, decode_records},
            },
        },
        test_support::tmp_path,
    },
};

/// The steps of a resume that keeps `kept` blocks: read the state, read
/// each kept block back, write the kept records back, size the partial
/// file.
fn resume_steps(kept: usize) -> Vec<Step> {
    let mut steps = vec![Step::ReadState];
    steps.extend(std::iter::repeat_n(Step::ReadPartial, kept));
    steps.extend([Step::TruncateState, Step::SyncState]);
    if kept > 0 {
        steps.extend([Step::WriteState, Step::SyncState]);
    }
    steps.push(Step::SizePartial);
    steps
}

/// Leave the partial file and the state file of a download that failed
/// within its third block: blocks 0 and 1 recorded.
async fn kept_pair(dest: &Path) {
    let status = finish(resumable(
        Source::new(Fault::Error(
            UnretryableError::NotFound { status: 404 }.into(),
        )),
        dest,
        &StepLog::default(),
        &CancellationToken::new(),
    ))
    .await;
    assert_eq!(status.kept_for_resume, Some(tmp_path(dest)));
    assert_eq!(assert_kept_state(dest), 2);
}

/// Leave the partial file and the state file of a download whose rename
/// failed: every block recorded, the partial file complete.
async fn complete_pair(dest: &Path) {
    let log = StepLog::default();
    log.fail(Step::Rename, When::Nth(1));
    let status = finish(resumable(
        Source::new(Fault::None),
        dest,
        &log,
        &CancellationToken::new(),
    ))
    .await;
    assert_eq!(status.kept_for_resume, Some(tmp_path(dest)));
    assert_eq!(assert_kept_state(dest), 4);
}

/// The bytes of both files.
fn files(dest: &Path) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    (
        std::fs::read(tmp_path(dest)).ok(),
        std::fs::read(state_path_for(dest)).ok(),
    )
}

/// The report the task published, when it resumed.
fn resumed(status: &TaskStatus) -> Option<crate::task::ResumeReport> {
    status.info.as_ref().and_then(|info| info.resumed.clone())
}

/// A download that failed resumes: blocks 0 and 1 are kept and not
/// requested again, the records are written back before the partial file
/// is sized, and the download completes; the report says what was kept.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_kept_pair_resumes() {
    let (_dir, dest) = dest();
    kept_pair(&dest).await;
    let log = StepLog::default();
    let source = Source::new(Fault::None);
    let (content, requested) = (source.content.clone(), source.requested.clone());
    let task = resumable(source, &dest, &log, &CancellationToken::new());
    let status = finish(task).await;
    assert_eq!(status.state, TaskState::Finished, "{:?}", status.error);
    assert_eq!(std::fs::read(&dest).unwrap(), content);
    assert!(!tmp_path(&dest).exists() && !state_path_for(&dest).exists());
    let steps = log.steps();
    let resume = resume_steps(2);
    assert_eq!(steps[..resume.len()], resume, "{steps:?}");
    assert_eq!(steps[steps.len() - 6..], COMPLETION, "{steps:?}");
    assert_checkpoints(&steps[resume.len()..steps.len() - 6]);
    let requested = requested.lock().unwrap().clone();
    assert!(!requested.is_empty());
    assert!(
        requested.iter().all(|range| range.start >= 2 * BLOCK),
        "{requested:?}"
    );
    let report = resumed(&status).expect("resumed");
    assert_eq!(report.state_file, state_path_for(&dest));
    assert_eq!(report.kept, [0..2 * BLOCK]);
    assert_eq!(report.kept_bytes, 2 * BLOCK);
    assert_eq!(report.dropped, []);
    let info = status.info.as_ref().unwrap();
    assert!(info.resumable);
    assert_eq!(info.mode, DownloadMode::Concurrent);
    assert_eq!(status.progress.downloaded, SIZE);
    assert_eq!(status.progress.downloaded_chunks, [0..SIZE]);
    assert_unlocked(&dest);
}

/// A pair whose every block was recorded resumes without a single
/// request, and completes.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_complete_pair_downloads_nothing() {
    let (_dir, dest) = dest();
    complete_pair(&dest).await;
    let log = StepLog::default();
    let source = Source::new(Fault::None);
    let (content, requested) = (source.content.clone(), source.requested.clone());
    let status = finish(resumable(source, &dest, &log, &CancellationToken::new())).await;
    assert_eq!(status.state, TaskState::Finished, "{:?}", status.error);
    assert_eq!(std::fs::read(&dest).unwrap(), content);
    assert_eq!(*requested.lock().unwrap(), []);
    let mut expected = resume_steps(4);
    expected.extend(COMPLETION);
    assert_eq!(log.steps(), expected);
    let report = resumed(&status).unwrap();
    assert_eq!((report.kept, report.kept_bytes), (vec![0..SIZE], SIZE));
}

/// A task that cannot resume fails with the reason and changes neither
/// file: the partial file is not reported as kept, the lock is free, and
/// nothing is requested.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_pair_that_cannot_resume_is_left_as_it_is() {
    let coded = meta().with_content_encoding(Some("gzip".to_owned()));
    let other_tag = meta().with_validator(Some(Validator::StrongEtag("\"v2\"".to_owned())));
    let other_kind = meta().with_validator(Some(Validator::LastModified(
        "Sun, 08 Nov 2026 12:00:10 GMT".to_owned(),
    )));
    // Each case: how the pair is spoiled, the source, and the reason.
    type Case = (&'static str, fn(&Path), Source, RejectReason);
    let cases: [Case; 9] = [
        (
            "missing partial",
            |dest| std::fs::remove_file(tmp_path(dest)).unwrap(),
            Source::new(Fault::None),
            RejectReason::MissingPartial,
        ),
        (
            "bad header",
            |dest| flip(&state_path_for(dest), 3),
            Source::new(Fault::None),
            RejectReason::BadHeader,
        ),
        (
            "oversized",
            |dest| append(&state_path_for(dest), &[0; 400]),
            Source::new(Fault::None),
            RejectReason::Oversized,
        ),
        (
            "another size",
            |_| {},
            Source {
                meta: BoltLoadAdapterMeta::new(SIZE + 1).with_validator(meta().validator),
                ..Source::new(Fault::None)
            },
            RejectReason::TotalMismatch,
        ),
        (
            "another entity tag",
            |_| {},
            Source {
                meta: other_tag,
                ..Source::new(Fault::None)
            },
            RejectReason::ValidatorMismatch,
        ),
        (
            "another kind",
            |_| {},
            Source {
                meta: other_kind,
                ..Source::new(Fault::None)
            },
            RejectReason::ValidatorMismatch,
        ),
        (
            "no validator",
            |_| {},
            Source {
                meta: BoltLoadAdapterMeta::new(SIZE),
                ..Source::new(Fault::None)
            },
            RejectReason::NoValidator,
        ),
        (
            "content-coded",
            |_| {},
            Source {
                meta: coded,
                ..Source::new(Fault::None)
            },
            RejectReason::ContentCoded,
        ),
        (
            "no ranges",
            |_| {},
            Source::without_ranges(Fault::None),
            RejectReason::RangeUnsupported,
        ),
    ];
    for (case, spoil, source, reason) in cases {
        let (_dir, dest) = dest();
        kept_pair(&dest).await;
        spoil(&dest);
        let before = files(&dest);
        let log = StepLog::default();
        let requested = source.requested.clone();
        let status = finish(resumable(source, &dest, &log, &CancellationToken::new())).await;
        assert_eq!(status.state, TaskState::Failed, "{case}");
        let Some(DownloadError::ResumeRejected {
            reason: got,
            state_file,
        }) = &status.error
        else {
            panic!("{case}: {:?}", status.error);
        };
        assert_eq!(
            (*got, state_file),
            (reason, &state_path_for(&dest)),
            "{case}"
        );
        assert_eq!(files(&dest), before, "{case}: both files as they were");
        assert!(!dest.exists(), "{case}");
        assert_eq!(
            (&status.kept_for_resume, &status.leftover_temp_file),
            (&None, &None),
            "{case}"
        );
        assert!(status.info.is_none(), "{case}: rejected while initializing");
        assert_eq!(*requested.lock().unwrap(), [], "{case}");
        assert!(
            log.steps()
                .iter()
                .all(|step| matches!(step, Step::ReadState)),
            "{case}: {:?}",
            log.steps()
        );
        assert_unlocked(&dest);
    }
}

/// The comparison takes the adapter's metadata, not the caller's: a
/// caller's validator that differs from the adapter's matching one does
/// not reject; a caller's that matches the stored one does not save an
/// adapter's that changed.
#[tokio::test(flavor = "multi_thread")]
async fn test_the_adapter_metadata_decides() {
    let v2 = meta().with_validator(Some(Validator::StrongEtag("\"v2\"".to_owned())));
    for (caller, adapter, resumes) in [(v2.clone(), meta(), true), (meta(), v2, false)] {
        let (_dir, dest) = dest();
        kept_pair(&dest).await;
        let source = Source {
            meta: adapter,
            ..Source::new(Fault::None)
        };
        let task = crate::task::test_support::builder(
            DownloadMode::Concurrent,
            source,
            &dest,
            &CancellationToken::new(),
        )
        .meta(caller)
        .options(super::resume_state::options())
        .resume(crate::task::builder::Resume::On)
        .spawn()
        .unwrap();
        let status = finish(task).await;
        if resumes {
            assert_eq!(status.state, TaskState::Finished, "{:?}", status.error);
            assert!(resumed(&status).is_some());
        } else {
            assert!(
                matches!(
                    status.error,
                    Some(DownloadError::ResumeRejected {
                        reason: RejectReason::ValidatorMismatch,
                        ..
                    })
                ),
                "{:?}",
                status.error
            );
        }
    }
}

fn flip(path: &Path, at: usize) {
    let mut bytes = std::fs::read(path).unwrap();
    bytes[at] ^= 0x40;
    std::fs::write(path, bytes).unwrap();
}

fn append(path: &Path, more: &[u8]) {
    let mut bytes = std::fs::read(path).unwrap();
    bytes.extend_from_slice(more);
    std::fs::write(path, bytes).unwrap();
}

/// Where record `n` of the state file of `dest` starts.
fn record_at(dest: &Path, n: usize) -> u64 {
    let state = std::fs::read(state_path_for(dest)).unwrap();
    (decode_header(&state).unwrap().len + n * RECORD_LEN) as u64
}

/// The record of block `index` of the download with `crc`.
fn record(index: u64, crc: u32) -> [u8; RECORD_LEN] {
    encode_record(
        &crate::task::instance::concurrent_task::checkpoint::CheckpointBlock {
            index,
            len: BLOCK as u32,
            crc,
        },
    )
}

/// A pair spoiled in one of the ways a record or a block can be bad
/// resumes from the rest: what is dropped is reported with its reason and
/// downloaded again, the state file holds only good records after the
/// write back, and the file is complete.
#[tokio::test(flavor = "multi_thread")]
async fn test_dropped_records_are_reported_and_downloaded_again() {
    type Spoil = fn(
        &Path,
    ) -> (
        Vec<(usize, DropReason, Option<std::ops::Range<u64>>)>,
        usize,
    );
    let cases: [(&str, Spoil); 6] = [
        ("a flipped byte in block 1", |dest| {
            flip(&tmp_path(dest), (BLOCK + 7) as usize);
            (
                vec![(1, DropReason::CrcMismatch, Some(BLOCK..2 * BLOCK))],
                1,
            )
        }),
        ("a partial file cut within block 1", |dest| {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(tmp_path(dest))
                .unwrap();
            file.set_len(BLOCK + BLOCK / 2).unwrap();
            (
                vec![(1, DropReason::BeyondFileLength, Some(BLOCK..2 * BLOCK))],
                1,
            )
        }),
        ("a torn tail", |dest| {
            append(&state_path_for(dest), &record(2, 9)[..11]);
            (vec![(2, DropReason::TornTail, None)], 2)
        }),
        ("a bad record", |dest| {
            let at = record_at(dest, 1) as usize + 13;
            flip(&state_path_for(dest), at);
            (vec![(1, DropReason::BadRecord, None)], 1)
        }),
        ("a conflicting record", |dest| {
            append(&state_path_for(dest), &record(0, 12345));
            (
                vec![
                    (0, DropReason::Conflict, Some(0..BLOCK)),
                    (2, DropReason::Conflict, Some(0..BLOCK)),
                ],
                1,
            )
        }),
        ("a block out of bounds", |dest| {
            append(&state_path_for(dest), &record(9, 1));
            (vec![(2, DropReason::OutOfBounds, None)], 2)
        }),
    ];
    for (case, spoil) in cases {
        // Stopped right after the write back, the state holds only the
        // kept records; the lock forbids reading it before the task ends.
        let (_dir, dest) = dest();
        kept_pair(&dest).await;
        let (_, kept) = spoil(&dest);
        let log = StepLog::default();
        log.fail(Step::SizePartial, When::Nth(1));
        let status = finish(resumable(
            Source::new(Fault::None),
            &dest,
            &log,
            &CancellationToken::new(),
        ))
        .await;
        assert_eq!(status.kept_for_resume, Some(tmp_path(&dest)), "{case}");
        let state = std::fs::read(state_path_for(&dest)).unwrap();
        let header = decode_header(&state).unwrap();
        assert_eq!(
            decode_records(&state, header.len).len(),
            kept,
            "{case}: only the kept records written back"
        );
        assert_eq!(state.len(), header.len + kept * RECORD_LEN, "{case}");
        // Resumed to the end, the dropped blocks are downloaded again.
        let (_dir, dest) = super::resume_state::dest();
        kept_pair(&dest).await;
        let (dropped, kept) = spoil(&dest);
        let dropped: Vec<_> = dropped
            .into_iter()
            .map(|(n, reason, range)| DroppedRecord {
                position: record_at(&dest, n),
                reason,
                range,
            })
            .collect();
        let source = Source::new(Fault::None);
        let content = source.content.clone();
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
            "{case}: {:?}",
            status.error
        );
        assert_eq!(std::fs::read(&dest).unwrap(), content, "{case}");
        let report = resumed(&status).unwrap();
        assert_eq!(report.dropped, dropped, "{case}");
        assert_eq!(report.kept_bytes, kept as u64 * BLOCK, "{case}");
    }
}

/// The steps of a resume that fail: reading either file leaves both as
/// they were (B2); writing back or sizing keeps both for a resume (C),
/// and the next resume completes.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_failed_resume_step_follows_its_phase() {
    for (failed, when, op, checking) in [
        (Step::ReadState, When::Nth(1), IoOp::Read, true),
        (Step::ReadPartial, When::Nth(2), IoOp::Read, true),
        (Step::TruncateState, When::Nth(1), IoOp::Write, false),
        (Step::SyncState, When::Nth(1), IoOp::Write, false),
        (Step::WriteState, When::Nth(1), IoOp::Write, false),
        (Step::SizePartial, When::Nth(1), IoOp::Allocate, false),
    ] {
        let (_dir, dest) = dest();
        kept_pair(&dest).await;
        let before = files(&dest);
        let log = StepLog::default();
        log.fail(failed, when);
        let status = finish(resumable(
            Source::new(Fault::None),
            &dest,
            &log,
            &CancellationToken::new(),
        ))
        .await;
        let Some(DownloadError::Io { op: got, .. }) = &status.error else {
            panic!("{failed:?}: {:?}", status.error);
        };
        assert_eq!(*got, op, "{failed:?}");
        assert_eq!(status.leftover_temp_file, None, "{failed:?}");
        assert_eq!(
            files(&dest).0,
            before.0,
            "{failed:?}: the partial file as it was"
        );
        if checking {
            assert_eq!(files(&dest), before, "{failed:?}");
            assert_eq!(status.kept_for_resume, None, "{failed:?}");
        } else {
            assert_eq!(status.kept_for_resume, Some(tmp_path(&dest)), "{failed:?}");
            // A header and a prefix of the kept records, all valid.
            assert!(assert_kept_state(&dest) <= 2, "{failed:?}");
        }
        assert_unlocked(&dest);
        let source = Source::new(Fault::None);
        let content = source.content.clone();
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
            "{failed:?}: {:?}",
            status.error
        );
        assert_eq!(std::fs::read(&dest).unwrap(), content, "{failed:?}");
    }
}

/// A cancellation while a resume is checked leaves both files as they
/// were (B2); once the records are written back, it keeps both (C). It is
/// seen between steps, before each block and after the write back, never
/// racing one.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_cancelled_resume_follows_its_phase() {
    for (gated, when, checking) in [
        (Step::ReadState, When::Nth(1), true),
        (Step::ReadPartial, When::Nth(1), true),
        // The last block: the cancellation is seen after the verification.
        (Step::ReadPartial, When::Nth(2), true),
        (Step::WriteState, When::Nth(1), false),
    ] {
        let (_dir, dest) = dest();
        kept_pair(&dest).await;
        let before = files(&dest);
        let log = StepLog::default();
        let (reached, release) = log.gate(gated, when);
        let token = CancellationToken::new();
        let task = resumable(Source::new(Fault::None), &dest, &log, &token);
        entered(reached).await;
        token.cancel();
        release.send(()).unwrap();
        let status = finish(task).await;
        assert_eq!(status.state, TaskState::Cancelled, "{gated:?}");
        // A read is a step; the write back, its cut, write and syncs, is
        // one: the cancellation is seen after it.
        let steps = log.steps();
        let last = if checking { gated } else { Step::SyncState };
        assert_eq!(steps.last(), Some(&last), "{gated:?}: {steps:?}");
        assert!(!steps.contains(&Step::SizePartial), "{gated:?}: {steps:?}");
        if checking {
            assert_eq!(files(&dest), before, "{gated:?}");
            assert_eq!(status.kept_for_resume, None, "{gated:?}");
        } else {
            assert_eq!(files(&dest).0, before.0, "{gated:?}");
            assert_eq!(status.kept_for_resume, Some(tmp_path(&dest)), "{gated:?}");
            assert_eq!(assert_kept_state(&dest), 2, "{gated:?}");
        }
        assert_eq!(status.leftover_temp_file, None);
        assert_unlocked(&dest);
    }
}

/// A panic while a resume is checked leaves both files as they were and
/// reports neither (B2); one after the records began to be written back
/// keeps both for a resume (C).
#[tokio::test(flavor = "multi_thread")]
async fn test_a_panic_in_a_resume_follows_its_phase() {
    for (step, checking) in [
        (Step::ReadState, true),
        (Step::ReadPartial, true),
        (Step::WriteState, false),
        (Step::SizePartial, false),
    ] {
        let (_dir, dest) = dest();
        kept_pair(&dest).await;
        let before = files(&dest);
        let log = StepLog::default();
        log.panic(step, When::Nth(1));
        let status = finish(resumable(
            Source::new(Fault::None),
            &dest,
            &log,
            &CancellationToken::new(),
        ))
        .await;
        assert!(
            matches!(status.error, Some(DownloadError::Panicked { .. })),
            "{step:?}: {:?}",
            status.error
        );
        if checking {
            assert_eq!(files(&dest), before, "{step:?}");
            assert_eq!(status.kept_for_resume, None, "{step:?}");
        } else {
            assert!(files(&dest).0.is_some(), "{step:?}");
            assert_eq!(status.kept_for_resume, Some(tmp_path(&dest)), "{step:?}");
            assert_kept_state(&dest);
        }
        assert_eq!(status.leftover_temp_file, None, "{step:?}");
        assert_unlocked(&dest);
    }
}

/// A resumed download that fails again, one block further, keeps what it
/// had and records what it added; the next resume keeps that too and does
/// not request it again.
#[tokio::test(flavor = "multi_thread")]
async fn test_resuming_again_keeps_what_was_added() {
    let (_dir, dest) = dest();
    kept_pair(&dest).await;
    // Block 2 completes this time; the fault comes within block 3.
    let status = finish(resumable(
        Source {
            fault_at: 3 * BLOCK + 100,
            ..Source::new(Fault::Error(
                UnretryableError::NotFound { status: 404 }.into(),
            ))
        },
        &dest,
        &StepLog::default(),
        &CancellationToken::new(),
    ))
    .await;
    assert_eq!(status.state, TaskState::Failed, "{:?}", status.error);
    assert_eq!(status.kept_for_resume, Some(tmp_path(&dest)));
    assert_eq!(resumed(&status).unwrap().kept_bytes, 2 * BLOCK);
    assert_eq!(assert_kept_state(&dest), 3);
    let state = std::fs::read(state_path_for(&dest)).unwrap();
    let records = decode_records(&state, decode_header(&state).unwrap().len);
    let mut indices: Vec<_> = records.iter().map(|record| record.index).collect();
    indices.sort_unstable();
    assert_eq!(indices, [0, 1, 2]);
    let source = Source::new(Fault::None);
    let (content, requested) = (source.content.clone(), source.requested.clone());
    let status = finish(resumable(
        source,
        &dest,
        &StepLog::default(),
        &CancellationToken::new(),
    ))
    .await;
    assert_eq!(status.state, TaskState::Finished, "{:?}", status.error);
    let report = resumed(&status).unwrap();
    assert_eq!(report.kept, [0..3 * BLOCK]);
    assert_eq!(*requested.lock().unwrap(), [3 * BLOCK..SIZE]);
    assert_eq!(std::fs::read(&dest).unwrap(), content);
}

/// A resumed download whose ranges turn out unusable falls back to one
/// connection: the state is invalidated first, what was kept goes with
/// it, and the info says so.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_resumed_download_that_falls_back_starts_over() {
    let (_dir, dest) = dest();
    kept_pair(&dest).await;
    let source = Source::new(Fault::Error(UnretryableError::RangeIgnored.into()));
    let content = source.content.clone();
    let status = finish(resumable(
        source,
        &dest,
        &StepLog::default(),
        &CancellationToken::new(),
    ))
    .await;
    assert_eq!(status.state, TaskState::Finished, "{:?}", status.error);
    assert_eq!(std::fs::read(&dest).unwrap(), content);
    let info = status.info.as_ref().unwrap();
    assert_eq!(info.mode, DownloadMode::Singleton);
    assert!(info.resumed.is_none() && !info.resumable);
    assert!(!state_path_for(&dest).exists());
}

/// Whether a task keeps a state file shows in its info: a resumable new
/// start does, a task with resume off does not.
#[tokio::test(flavor = "multi_thread")]
async fn test_the_info_says_whether_the_task_is_resumable() {
    for resume in [
        crate::task::builder::Resume::On,
        crate::task::builder::Resume::Off,
    ] {
        let (_dir, dest) = dest();
        let task = crate::task::test_support::builder(
            DownloadMode::Concurrent,
            Source::new(Fault::None),
            &dest,
            &CancellationToken::new(),
        )
        .meta(meta())
        .resume(resume)
        .spawn()
        .unwrap();
        let status = finish(task).await;
        assert_eq!(status.state, TaskState::Finished);
        let info = status.info.as_ref().unwrap();
        assert_eq!(info.resumable, resume == crate::task::builder::Resume::On);
        assert!(info.resumed.is_none());
    }
    let _ = NEW_START;
}

/// A task that prefers one connection resumes over ranges with one, as a
/// new start would (design §11.5.8).
#[tokio::test(flavor = "multi_thread")]
async fn test_a_single_connection_task_resumes_over_ranges() {
    let (_dir, dest) = dest();
    kept_pair(&dest).await;
    let source = Source::new(Fault::None);
    let (content, requested, peak) = (
        source.content.clone(),
        source.requested.clone(),
        source.peak.clone(),
    );
    let task = crate::task::test_support::builder(
        DownloadMode::Singleton,
        source,
        &dest,
        &CancellationToken::new(),
    )
    .meta(meta())
    .resume(crate::task::builder::Resume::On)
    .spawn()
    .unwrap();
    let status = finish(task).await;
    assert_eq!(status.state, TaskState::Finished, "{:?}", status.error);
    assert_eq!(std::fs::read(&dest).unwrap(), content);
    assert_eq!(*requested.lock().unwrap(), [2 * BLOCK..SIZE]);
    assert_eq!(peak.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(resumed(&status).unwrap().kept_bytes, 2 * BLOCK);
}

/// What a task finds next to the destination decides how it starts: an
/// empty state file, as a crash before the header leaves it, is no state;
/// with resume off, a valid state is invalidated, not resumed.
#[tokio::test(flavor = "multi_thread")]
async fn test_no_state_or_resume_off_starts_anew() {
    // An empty state file and some partial file.
    let (_dir, dest) = dest();
    std::fs::write(state_path_for(&dest), b"").unwrap();
    std::fs::write(tmp_path(&dest), b"left over").unwrap();
    let log = StepLog::default();
    let source = Source::new(Fault::None);
    let content = source.content.clone();
    let status = finish(resumable(source, &dest, &log, &CancellationToken::new())).await;
    assert_eq!(status.state, TaskState::Finished, "{:?}", status.error);
    assert_eq!(std::fs::read(&dest).unwrap(), content);
    assert_eq!(log.steps()[..5], NEW_START);
    assert!(resumed(&status).is_none());
    // A valid pair, and resume off.
    let (_dir, dest) = super::resume_state::dest();
    kept_pair(&dest).await;
    let source = Source::new(Fault::None);
    let (content, requested) = (source.content.clone(), source.requested.clone());
    let task = crate::task::test_support::builder(
        DownloadMode::Concurrent,
        source,
        &dest,
        &CancellationToken::new(),
    )
    .meta(meta())
    .spawn()
    .unwrap();
    let status = finish(task).await;
    assert_eq!(status.state, TaskState::Finished, "{:?}", status.error);
    assert_eq!(std::fs::read(&dest).unwrap(), content);
    assert!(
        requested
            .lock()
            .unwrap()
            .iter()
            .any(|range| range.start == 0)
    );
    assert!(!state_path_for(&dest).exists() && !tmp_path(&dest).exists());
}

/// The next task with resume on after a finished one, as design §11.5.5
/// "紧接着的下一次 On" lays out: a state file removed, or only emptied,
/// is no state; one neither emptied nor removed names a partial file that
/// was renamed: `MissingPartial`.
#[tokio::test(flavor = "multi_thread")]
async fn test_the_next_task_after_a_finished_one() {
    for (failed, next) in [
        (vec![], None),
        (vec![Step::RemoveState], None),
        (
            vec![Step::TruncateState, Step::RemoveState],
            Some(RejectReason::MissingPartial),
        ),
    ] {
        let (_dir, dest) = dest();
        let log = StepLog::default();
        for &step in &failed {
            log.fail(step, When::After(Step::Rename));
        }
        let status = finish(resumable(
            Source::new(Fault::None),
            &dest,
            &log,
            &CancellationToken::new(),
        ))
        .await;
        assert_eq!(status.state, TaskState::Finished, "{failed:?}");
        let source = Source::new(Fault::None);
        let content = source.content.clone();
        let status = finish(resumable(
            source,
            &dest,
            &StepLog::default(),
            &CancellationToken::new(),
        ))
        .await;
        match next {
            None => {
                assert_eq!(
                    status.state,
                    TaskState::Finished,
                    "{failed:?}: {:?}",
                    status.error
                );
                assert!(resumed(&status).is_none(), "{failed:?}");
                assert_eq!(std::fs::read(&dest).unwrap(), content);
            }
            Some(reason) => assert!(
                matches!(
                    status.error,
                    Some(DownloadError::ResumeRejected { reason: got, .. }) if got == reason
                ),
                "{failed:?}: {:?}",
                status.error
            ),
        }
    }
}

/// A task cancelled while it takes the lock, which finds a state to resume
/// from, leaves both files as they are.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_task_cancelled_while_locking_leaves_a_pair() {
    let (_dir, dest) = dest();
    kept_pair(&dest).await;
    let before = files(&dest);
    let token = CancellationToken::new();
    let cancel = token.clone();
    let task = crate::task::test_support::builder(
        DownloadMode::Concurrent,
        Source::new(Fault::None),
        &dest,
        &token,
    )
    .meta(meta())
    .resume(crate::task::builder::Resume::On)
    .lock_hooks(
        crate::task::instance::destination_lock::LockHooks::default()
            .before_lock(move |_| cancel.cancel()),
    )
    .spawn()
    .unwrap();
    let status = finish(task).await;
    assert_eq!(status.state, TaskState::Cancelled, "{:?}", status.error);
    assert_eq!(files(&dest), before);
    assert_eq!(
        (&status.kept_for_resume, &status.leftover_temp_file),
        (&None, &None)
    );
    assert_unlocked(&dest);
}

/// A partial file longer than the total, which the comparison of the
/// totals already ruled out as another download's, is cut to the total;
/// one shorter grows to it. Either way the file completes.
#[tokio::test(flavor = "multi_thread")]
async fn test_the_partial_file_is_sized_to_the_total() {
    for longer in [true, false] {
        let (_dir, dest) = dest();
        kept_pair(&dest).await;
        let partial = std::fs::OpenOptions::new()
            .write(true)
            .open(tmp_path(&dest))
            .unwrap();
        partial
            .set_len(if longer { SIZE + 4096 } else { 2 * BLOCK })
            .unwrap();
        drop(partial);
        let source = Source::new(Fault::None);
        let content = source.content.clone();
        let status = finish(resumable(
            source,
            &dest,
            &StepLog::default(),
            &CancellationToken::new(),
        ))
        .await;
        // The destination is the partial file renamed: of the total.
        assert_eq!(
            status.state,
            TaskState::Finished,
            "{longer}: {:?}",
            status.error
        );
        assert_eq!(std::fs::read(&dest).unwrap(), content, "{longer}");
        assert_eq!(resumed(&status).unwrap().kept_bytes, 2 * BLOCK, "{longer}");
    }
}

/// The first `Downloading` status of a resume reports what was kept, before
/// a byte of the rest arrives (task-api-spec STATUS-12).
#[tokio::test(flavor = "multi_thread")]
async fn test_the_progress_starts_with_what_was_kept() {
    let (_dir, dest) = dest();
    kept_pair(&dest).await;
    // The publisher records the progress of the start as it publishes it,
    // before the transfer runs: no later sample can replace it.
    let starts = crate::task::status::Starts::default();
    let source = Source::new(Fault::None);
    let content = source.content.clone();
    let task = crate::task::test_support::builder(
        DownloadMode::Concurrent,
        source,
        &dest,
        &CancellationToken::new(),
    )
    .meta(meta())
    .options(super::resume_state::options())
    .resume(crate::task::builder::Resume::On)
    .record_starts(starts.clone())
    .spawn()
    .unwrap();
    let status = finish(task).await;
    assert_eq!(status.state, TaskState::Finished, "{:?}", status.error);
    assert_eq!(std::fs::read(&dest).unwrap(), content);
    assert_eq!(
        *starts.lock().unwrap(),
        [crate::task::Progress::new(
            Some(SIZE),
            2 * BLOCK,
            vec![0..2 * BLOCK]
        )]
    );
    assert_eq!(resumed(&status).unwrap().kept_bytes, 2 * BLOCK);
}

/// A task cancelled while the adapter checks for ranges, during a resume,
/// ends: the check races the cancellation. Both files stay as they were,
/// neither is reported, and the lock is free.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_resume_cancelled_in_the_range_check_ends() {
    let (_dir, dest) = dest();
    kept_pair(&dest).await;
    let before = files(&dest);
    let gate = bolt_load_tests::adapter::fault::Gate::new();
    let source = Source {
        stall_range_check: Some(gate.clone()),
        ..Source::new(Fault::None)
    };
    let token = CancellationToken::new();
    let log = StepLog::default();
    let task = resumable(source, &dest, &log, &token);
    crate::task::test_support::watchdog(gate.entered()).await;
    token.cancel();
    let status = finish(task).await;
    gate.release();
    assert_eq!(status.state, TaskState::Cancelled, "{:?}", status.error);
    assert_eq!(files(&dest), before);
    assert_eq!(
        (&status.kept_for_resume, &status.leftover_temp_file),
        (&None, &None)
    );
    assert_eq!(log.steps(), [Step::ReadState]);
    assert_unlocked(&dest);
}

/// A task on `source` that may resume, from `resume_source` when given.
fn with_source(source: Source, dest: &Path, resume_source: Option<&str>) -> crate::task::Task {
    let mut builder = crate::task::test_support::builder(
        DownloadMode::Concurrent,
        source,
        dest,
        &CancellationToken::new(),
    )
    .meta(meta())
    .options(super::resume_state::options())
    .resume(crate::task::Resume::On);
    if let Some(resume_source) = resume_source {
        builder = builder.resume_source(resume_source);
    }
    builder.spawn().unwrap()
}

/// The source a state file names must be the one a resume gives: the
/// same one resumes; another, or none, is rejected and leaves both files.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_resume_needs_the_same_source() {
    for (resume_source, resumes) in [
        (Some("https://example.com/a"), true),
        (Some("https://example.com/b"), false),
        (None, false),
    ] {
        let (_dir, dest) = dest();
        let status = finish(with_source(
            Source::new(Fault::Error(
                UnretryableError::NotFound { status: 404 }.into(),
            )),
            &dest,
            Some("https://example.com/a"),
        ))
        .await;
        assert_eq!(status.kept_for_resume, Some(tmp_path(&dest)));
        let state = std::fs::read(state_path_for(&dest)).unwrap();
        assert_eq!(
            decode_header(&state).unwrap().source.as_deref(),
            Some("https://example.com/a")
        );
        let before = files(&dest);
        let status = finish(with_source(Source::new(Fault::None), &dest, resume_source)).await;
        if resumes {
            assert_eq!(status.state, TaskState::Finished, "{:?}", status.error);
        } else {
            assert!(
                matches!(
                    status.error,
                    Some(DownloadError::ResumeRejected {
                        reason: RejectReason::SourceMismatch,
                        ..
                    })
                ),
                "{resume_source:?}: {:?}",
                status.error
            );
            assert_eq!(files(&dest), before, "{resume_source:?}");
        }
    }
}

/// A source longer than a state file holds fails the builder's
/// validation; one exactly that long does not.
#[test]
fn test_a_resume_source_has_a_limit() {
    let (_dir, dest) = dest();
    let builder = |len: usize| {
        crate::task::test_support::builder(
            DownloadMode::Concurrent,
            Source::new(Fault::None),
            &dest,
            &CancellationToken::new(),
        )
        .resume(crate::task::Resume::On)
        .resume_source("s".repeat(len))
    };
    assert_eq!(builder(4096).validate(), Ok(()));
    assert_eq!(
        builder(4097).validate(),
        Err(crate::task::BuildError::InvalidOptions {
            reason: "the resume source is longer than 4096 bytes"
        })
    );
}
