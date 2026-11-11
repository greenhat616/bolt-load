//! The checks before a resume (design §11.5.10).

use std::{cell::Cell, sync::Mutex};

use bolt_load_core::adapter::Validator;
use pretty_assertions::assert_eq;

use super::*;
use crate::task::instance::concurrent_task::checkpoint::{BLOCK_SIZE, CheckpointBlock};

const BS: u64 = BLOCK_SIZE.get();
const TOTAL: u64 = 2 * BS + 100;
const DATE: &str = "Sun, 08 Nov 2026 12:00:10 GMT";

fn etag(value: &str) -> Validator {
    Validator::StrongEtag(format!("\"{value}\""))
}

fn header(validator: Validator, source: Option<&str>) -> StoredHeader {
    StoredHeader {
        block_size: BS as u32,
        total: TOTAL,
        validator,
        source: source.map(str::to_owned),
        len: 40,
    }
}

fn meta(validator: Option<Validator>) -> BoltLoadAdapterMeta {
    BoltLoadAdapterMeta::new(TOTAL).with_validator(validator)
}

/// The same resource, from the same source, over ranges, resumes; a
/// `Last-Modified` date matches in another format.
#[test]
fn test_the_same_resource_resumes() {
    let stored = header(etag("v1"), Some("src"));
    assert_eq!(
        compare(&stored, &meta(Some(etag("v1"))), Some("src"), true),
        Ok(())
    );
    let stored = header(Validator::LastModified(DATE.to_owned()), None);
    let other_format = Validator::LastModified("Sunday, 08-Nov-26 12:00:10 GMT".to_owned());
    assert_eq!(
        compare(&stored, &meta(Some(other_format)), None, true),
        Ok(())
    );
}

/// Every reason the resource of the fresh metadata is not the stored one,
/// or cannot be resumed.
#[test]
fn test_another_resource_is_rejected() {
    let stored = header(etag("v1"), Some("src"));
    let good = meta(Some(etag("v1")));
    let cases = [
        (
            "unknown size",
            BoltLoadAdapterMeta::new(0).with_validator(Some(etag("v1"))),
            Some("src"),
            true,
            RejectReason::UnknownSize,
        ),
        (
            "content-coded",
            good.clone().with_content_encoding(Some("gzip".to_owned())),
            Some("src"),
            true,
            RejectReason::ContentCoded,
        ),
        (
            "no ranges",
            good.clone(),
            Some("src"),
            false,
            RejectReason::RangeUnsupported,
        ),
        (
            "another size",
            BoltLoadAdapterMeta::new(TOTAL + 1).with_validator(Some(etag("v1"))),
            Some("src"),
            true,
            RejectReason::TotalMismatch,
        ),
        (
            "another source",
            good.clone(),
            Some("other"),
            true,
            RejectReason::SourceMismatch,
        ),
        (
            "no source",
            good.clone(),
            None,
            true,
            RejectReason::SourceMismatch,
        ),
        (
            "no validator",
            meta(None),
            Some("src"),
            true,
            RejectReason::NoValidator,
        ),
        (
            "a validator of 1025 bytes",
            meta(Some(Validator::StrongEtag(format!(
                "\"{}\"",
                "v".repeat(1023)
            )))),
            Some("src"),
            true,
            RejectReason::NoValidator,
        ),
        (
            "another entity tag",
            meta(Some(etag("v2"))),
            Some("src"),
            true,
            RejectReason::ValidatorMismatch,
        ),
        (
            "another kind",
            meta(Some(Validator::LastModified("\"v1\"".to_owned()))),
            Some("src"),
            true,
            RejectReason::ValidatorMismatch,
        ),
    ];
    for (case, meta, source, ranges, reason) in cases {
        assert_eq!(
            compare(&stored, &meta, source, ranges),
            Err(reason),
            "{case}"
        );
    }
    // A source stored, none given; and the other way around.
    let unnamed = header(etag("v1"), None);
    assert_eq!(compare(&unnamed, &good, None, true), Ok(()));
    assert_eq!(
        compare(&unnamed, &good, Some("src"), true),
        Err(RejectReason::SourceMismatch)
    );
    // An unknown size is not reported as another one.
    assert_eq!(
        compare(&stored, &BoltLoadAdapterMeta::new(0), Some("src"), true),
        Err(RejectReason::UnknownSize)
    );
    // A validator of exactly 1024 bytes is usable.
    let limit = Validator::StrongEtag(format!("\"{}\"", "v".repeat(1022)));
    assert_eq!(
        compare(&header(limit.clone(), None), &meta(Some(limit)), None, true),
        Ok(())
    );
}

/// The partial file in memory; it counts its reads and can fail one.
struct Memory {
    bytes: Vec<u8>,
    reads: Mutex<Vec<Range<u64>>>,
    fail: Option<u64>,
}

impl Memory {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            reads: Mutex::default(),
            fail: None,
        }
    }
}

impl BlockReader for Memory {
    fn read(&self, range: Range<u64>) -> io::Result<Vec<u8>> {
        self.reads.lock().unwrap().push(range.clone());
        if self.fail == Some(range.start) {
            return Err(io::Error::other("bad sector"));
        }
        Ok(self.bytes[range.start as usize..range.end as usize].to_vec())
    }
}

fn content() -> Vec<u8> {
    (0..TOTAL).map(|i| (i % 253) as u8).collect()
}

/// The record of block `index` of `content`, at the `n`-th place.
fn record(content: &[u8], index: u64, n: u64) -> StoredRecord {
    let start = index * BS;
    let end = (start + BS).min(TOTAL);
    StoredRecord {
        position: 40 + 24 * n,
        block: CheckpointBlock {
            index,
            len: (end - start) as u32,
            crc: crc32c::crc32c(&content[start as usize..end as usize]),
        },
    }
}

fn not_cancelled() -> bool {
    false
}

/// Verify `records` onto no dropped records yet.
fn check(
    records: Vec<StoredRecord>,
    block_size: u32,
    file_len: u64,
    partial: &dyn BlockReader,
    cancelled: &dyn Fn() -> bool,
) -> Result<(Vec<StoredRecord>, Vec<DroppedRecord>), VerifyError> {
    let mut dropped = Vec::new();
    let kept = verify(
        records,
        block_size,
        file_len,
        partial,
        cancelled,
        &mut dropped,
    )?;
    Ok((kept, dropped))
}

/// The dropped records go after those already dropped, and the kept ones
/// stay in their order.
#[test]
fn test_verification_adds_to_the_dropped_records() {
    let content = content();
    let records: Vec<_> = [2, 0, 1]
        .iter()
        .enumerate()
        .map(|(n, &i)| record(&content, i, n as u64))
        .collect();
    let mut flipped = content.clone();
    flipped[5] ^= 1;
    let earlier = DroppedRecord {
        position: 9,
        reason: DropReason::TornTail,
        range: None,
    };
    let mut dropped = vec![earlier.clone()];
    let kept = verify(
        records.clone(),
        BS as u32,
        TOTAL,
        &Memory::new(flipped),
        &not_cancelled,
        &mut dropped,
    )
    .unwrap();
    assert_eq!(kept, [records[0], records[2]]);
    assert_eq!(
        dropped,
        [
            earlier,
            DroppedRecord {
                position: records[1].position,
                reason: DropReason::CrcMismatch,
                range: Some(0..BS),
            }
        ]
    );
}

/// Blocks whose bytes match are kept; a flipped byte drops its block.
#[test]
fn test_blocks_are_verified_against_the_file() {
    let content = content();
    let records: Vec<_> = (0..3).map(|i| record(&content, i, i)).collect();
    let (kept, dropped) = check(
        records.clone(),
        BS as u32,
        TOTAL,
        &Memory::new(content.clone()),
        &not_cancelled,
    )
    .unwrap();
    assert_eq!(kept, records);
    assert_eq!(dropped, []);
    let mut flipped = content.clone();
    flipped[(BS + 5) as usize] ^= 0x80;
    let (kept, dropped) = check(
        records.clone(),
        BS as u32,
        TOTAL,
        &Memory::new(flipped),
        &not_cancelled,
    )
    .unwrap();
    assert_eq!(kept, [records[0], records[2]]);
    assert_eq!(
        dropped,
        [DroppedRecord {
            position: records[1].position,
            reason: DropReason::CrcMismatch,
            range: Some(BS..2 * BS),
        }]
    );
}

/// A block past the end of the file is dropped without reading it, a
/// block that ends one byte past it included.
#[test]
fn test_blocks_past_the_file_are_dropped() {
    let content = content();
    let records: Vec<_> = (0..3).map(|i| record(&content, i, i)).collect();
    let short = TOTAL - 1;
    let memory = Memory::new(content[..short as usize].to_vec());
    let (kept, dropped) =
        check(records.clone(), BS as u32, short, &memory, &not_cancelled).unwrap();
    assert_eq!(kept, records[..2]);
    assert_eq!(
        dropped,
        [DroppedRecord {
            position: records[2].position,
            reason: DropReason::BeyondFileLength,
            range: Some(2 * BS..TOTAL),
        }]
    );
    assert_eq!(*memory.reads.lock().unwrap(), [0..BS, BS..2 * BS]);
}

/// Cancellation is asked before every block, and stops the verification.
#[test]
fn test_verification_stops_when_cancelled() {
    let content = content();
    let records: Vec<_> = (0..3).map(|i| record(&content, i, i)).collect();
    let memory = Memory::new(content.clone());
    let asked = Cell::new(0);
    let cancelled = || {
        asked.set(asked.get() + 1);
        asked.get() > 2
    };
    let result = check(records, BS as u32, TOTAL, &memory, &cancelled);
    assert!(matches!(result, Err(VerifyError::Cancelled)), "{result:?}");
    assert_eq!(asked.get(), 3);
    assert_eq!(*memory.reads.lock().unwrap(), [0..BS, BS..2 * BS]);
}

/// A block that cannot be read fails the verification: it is not taken
/// for a mismatch.
#[test]
fn test_a_failed_read_is_an_error() {
    let content = content();
    let records: Vec<_> = (0..3).map(|i| record(&content, i, i)).collect();
    let memory = Memory {
        fail: Some(BS),
        ..Memory::new(content.clone())
    };
    let result = check(records, BS as u32, TOTAL, &memory, &not_cancelled);
    let Err(VerifyError::Read { range, source }) = result else {
        panic!("{result:?}");
    };
    assert_eq!(range, BS..2 * BS);
    assert_eq!(source.to_string(), "bad sector");
}

/// The report merges the kept blocks and sorts the dropped records.
#[test]
fn test_the_report_merges_and_sorts() {
    let content = content();
    let mut kept = [record(&content, 2, 0), record(&content, 0, 1)];
    let drop = |position, reason| DroppedRecord {
        position,
        reason,
        range: None,
    };
    let report = report(
        PathBuf::from("a.boltload"),
        &mut kept,
        BS as u32,
        vec![
            drop(100, DropReason::CrcMismatch),
            drop(64, DropReason::TornTail),
        ],
    )
    .unwrap();
    assert_eq!(
        report,
        ResumeReport {
            state_file: PathBuf::from("a.boltload"),
            kept: vec![0..BS, 2 * BS..TOTAL],
            kept_bytes: BS + 100,
            dropped: vec![
                drop(64, DropReason::TornTail),
                drop(100, DropReason::CrcMismatch)
            ],
        }
    );
    let mut all: Vec<_> = (0..3).map(|i| record(&content, i, i)).collect();
    let merged = super::report(PathBuf::new(), &mut all, BS as u32, Vec::new()).unwrap();
    assert_eq!(merged.kept, vec![0..TOTAL]);
    assert_eq!(merged.kept_bytes, TOTAL);
    let none = super::report(PathBuf::new(), &mut [], BS as u32, Vec::new()).unwrap();
    assert_eq!((none.kept, none.kept_bytes), (vec![], 0));
}

/// Records of blocks `0..3` of the unflipped content, and a dropped list
/// that already holds one entry.
fn three_with_a_drop() -> (Vec<StoredRecord>, Vec<DroppedRecord>, DroppedRecord) {
    let records = (0..3).map(|i| record(&content(), i, i)).collect();
    let earlier = DroppedRecord {
        position: 9,
        reason: DropReason::TornTail,
        range: None,
    };
    (records, vec![earlier.clone()], earlier)
}

/// A verification stopped by cancellation or a failed read leaves the
/// caller's dropped list as it was, plus the drops it completed: no
/// duplicates, and no read after the stop.
#[test]
fn test_a_stopped_verification_keeps_the_dropped_records() {
    // Block 0 no longer matches; block 1 does.
    let mut flipped = content();
    flipped[3] ^= 1;
    let crc_mismatch = DroppedRecord {
        position: record(&content(), 0, 0).position,
        reason: DropReason::CrcMismatch,
        range: Some(0..BS),
    };
    // Cancelled before block 2.
    let (records, mut dropped, earlier) = three_with_a_drop();
    let memory = Memory::new(flipped.clone());
    let asked = Cell::new(0);
    let cancelled = || {
        asked.set(asked.get() + 1);
        asked.get() > 2
    };
    let result = verify(records, BS as u32, TOTAL, &memory, &cancelled, &mut dropped);
    assert!(matches!(result, Err(VerifyError::Cancelled)), "{result:?}");
    assert_eq!(dropped, [earlier.clone(), crc_mismatch.clone()]);
    assert_eq!(*memory.reads.lock().unwrap(), [0..BS, BS..2 * BS]);
    // Block 2 cannot be read.
    let (records, mut dropped, earlier) = three_with_a_drop();
    let memory = Memory {
        fail: Some(2 * BS),
        ..Memory::new(flipped)
    };
    let result = verify(
        records,
        BS as u32,
        TOTAL,
        &memory,
        &not_cancelled,
        &mut dropped,
    );
    assert!(
        matches!(result, Err(VerifyError::Read { .. })),
        "{result:?}"
    );
    assert_eq!(dropped, [earlier, crc_mismatch]);
    assert_eq!(
        *memory.reads.lock().unwrap(),
        [0..BS, BS..2 * BS, 2 * BS..TOTAL]
    );
}
