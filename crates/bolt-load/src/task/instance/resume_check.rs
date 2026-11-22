//! The checks before a resume (design §11.5.10): the state file's header
//! against the fresh metadata (step 2), and every kept block against the
//! partial file (step 4); then the report (step 6).

#![cfg_attr(not(test), expect(dead_code, reason = "the task resumes in C20b"))]

use std::{io, ops::Range, path::PathBuf};

use super::{
    concurrent_task::checkpoint::BlockReader,
    state_file::{
        MAX_VALIDATOR_LEN,
        read::{StoredHeader, StoredRecord, try_push},
    },
};
use crate::{
    adapter::BoltLoadAdapterMeta,
    task::resume::{DropReason, DroppedRecord, RejectReason, ResumeReport},
};

/// Whether the state file's `header` names the resource of `meta`, which the
/// adapter retrieved for this task, from `source`, over ranges when
/// `ranges` (design §11.5.10 step 2, §11.5.3, §11.5.7).
///
/// What makes the resource not resumable at all is checked first, so an
/// unknown size is not reported as another size; then the size, the source
/// and the validator.
///
/// # Errors
///
/// The reason the task cannot resume.
pub(crate) fn compare(
    header: &StoredHeader,
    meta: &BoltLoadAdapterMeta,
    source: Option<&str>,
    ranges: bool,
) -> Result<(), RejectReason> {
    if meta.content_size == 0 {
        return Err(RejectReason::UnknownSize);
    }
    if meta.content_encoding.is_some() {
        return Err(RejectReason::ContentCoded);
    }
    if !ranges {
        return Err(RejectReason::RangeUnsupported);
    }
    if meta.content_size != header.total {
        return Err(RejectReason::TotalMismatch);
    }
    if source != header.source.as_deref() {
        return Err(RejectReason::SourceMismatch);
    }
    let validator = meta
        .validator
        .as_ref()
        .filter(|validator| validator.as_str().len() <= MAX_VALIDATOR_LEN)
        .ok_or(RejectReason::NoValidator)?;
    if !header.validator.matches(validator) {
        return Err(RejectReason::ValidatorMismatch);
    }
    Ok(())
}

/// Why a verification did not end.
#[derive(Debug)]
pub(crate) enum VerifyError {
    /// The task was cancelled between two blocks.
    Cancelled,
    /// A block could not be read: the task fails, it does not guess.
    Read {
        range: Range<u64>,
        source: io::Error,
    },
    /// There was no memory to report a dropped record: an io error of kind
    /// [`io::ErrorKind::OutOfMemory`]. The task fails as on a read error.
    OutOfMemory(
        #[cfg_attr(
            test,
            expect(
                dead_code,
                reason = "read by the task (C20b); a test cannot run out of memory"
            )
        )]
        io::Error,
    ),
}

/// The records whose blocks the partial file holds, of the `records` kept
/// by the scan, in their order; the others are pushed onto `dropped`
/// (design §11.5.10 step 4, §11.5.6). The kept ones are moved within
/// `records`, and `dropped` grows fallibly.
///
/// A block past `file_len`, the partial file's length, is
/// [`DropReason::BeyondFileLength`]; one whose bytes in `partial` do not
/// have the record's CRC is [`DropReason::CrcMismatch`]. `cancelled` is
/// asked before each block.
///
/// # Errors
///
/// The task was cancelled, a block could not be read, or there was no
/// memory for a dropped record.
pub(crate) fn verify(
    mut records: Vec<StoredRecord>,
    block_size: u32,
    file_len: u64,
    partial: &dyn BlockReader,
    cancelled: &dyn Fn() -> bool,
    dropped: &mut Vec<DroppedRecord>,
) -> Result<Vec<StoredRecord>, VerifyError> {
    let mut kept = 0;
    for at in 0..records.len() {
        if cancelled() {
            return Err(VerifyError::Cancelled);
        }
        let record = records[at];
        // The scan built the record from a range in bounds.
        let start = record.block.index * u64::from(block_size);
        let range = start..start + u64::from(record.block.len);
        let reason = if range.end > file_len {
            Some(DropReason::BeyondFileLength)
        } else {
            let data = partial
                .read(range.clone())
                .map_err(|source| VerifyError::Read {
                    range: range.clone(),
                    source,
                })?;
            (crc32c::crc32c(&data) != record.block.crc).then_some(DropReason::CrcMismatch)
        };
        match reason {
            None => {
                records[kept] = record;
                kept += 1;
            }
            Some(reason) => try_push(
                dropped,
                DroppedRecord {
                    position: record.position,
                    reason,
                    range: Some(range),
                },
            )
            .map_err(VerifyError::OutOfMemory)?,
        }
    }
    records.truncate(kept);
    Ok(records)
}

/// The report of a resume from `state_file` that keeps `kept`, of blocks
/// of `block_size`, and dropped `dropped` (design §11.5.10 step 6): the
/// kept ranges merged, the dropped records in file order. `kept` is
/// sorted by block in place; nothing that grows with them is allocated
/// infallibly.
///
/// # Errors
///
/// There was no memory for the ranges: an io error of kind
/// [`io::ErrorKind::OutOfMemory`].
pub(crate) fn report(
    state_file: PathBuf,
    kept: &mut [StoredRecord],
    block_size: u32,
    mut dropped: Vec<DroppedRecord>,
) -> io::Result<ResumeReport> {
    kept.sort_unstable_by_key(|record| record.block.index);
    let mut ranges: Vec<Range<u64>> = Vec::new();
    for record in kept.iter() {
        let start = record.block.index * u64::from(block_size);
        let range = start..start + u64::from(record.block.len);
        match ranges.last_mut() {
            Some(last) if last.end == range.start => last.end = range.end,
            _ => try_push(&mut ranges, range)?,
        }
    }
    // Positions are unique, and an unstable sort allocates nothing.
    dropped.sort_unstable_by_key(|drop| drop.position);
    Ok(ResumeReport {
        state_file,
        kept_bytes: ranges.iter().map(|range| range.end - range.start).sum(),
        kept: ranges,
        dropped,
    })
}

#[cfg(test)]
mod tests;
