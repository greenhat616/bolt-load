//! Reading a state file back (design §11.5.5, §11.5.10 steps 1 and 3): the
//! header, which rejects the whole file when it is bad, then the records,
//! each kept or dropped with a reason.

#![cfg_attr(not(test), expect(dead_code, reason = "the task resumes in C20b"))]

use std::{fs::File, io, ops::Range};

use bolt_load_core::adapter::Validator;

use super::{
    COMPLETE_BLOCK, MAGIC, MAX_SOURCE_LEN, MAX_VALIDATOR_LEN, NO_SOURCE, RECORD_LEN, VERSION,
};
use crate::task::{
    instance::concurrent_task::checkpoint::{BLOCK_SIZE, CheckpointBlock},
    resume::{DropReason, DroppedRecord, RejectReason},
};

/// A header read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredHeader {
    pub(crate) block_size: u32,
    pub(crate) total: u64,
    pub(crate) validator: Validator,
    pub(crate) source: Option<String>,
    /// Its length in the file, its CRC included: where the records start.
    pub(crate) len: u64,
}

/// A record read back that names a block of the file, at `position` in the
/// state file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StoredRecord {
    pub(crate) position: u64,
    pub(crate) block: CheckpointBlock,
}

/// A state file read back: its header, the records kept, by block index,
/// and those dropped, in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Scanned {
    pub(crate) header: StoredHeader,
    pub(crate) records: Vec<StoredRecord>,
    pub(crate) dropped: Vec<DroppedRecord>,
}

/// Takes the bytes of a header in order; running out is a bad header.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], RejectReason> {
        let end = self.at.checked_add(n).ok_or(RejectReason::BadHeader)?;
        let taken = self
            .bytes
            .get(self.at..end)
            .ok_or(RejectReason::BadHeader)?;
        self.at = end;
        Ok(taken)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], RejectReason> {
        Ok(self.take(N)?.try_into().expect("took N bytes"))
    }

    fn u8(&mut self) -> Result<u8, RejectReason> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, RejectReason> {
        self.array().map(u16::from_le_bytes)
    }

    fn u32(&mut self) -> Result<u32, RejectReason> {
        self.array().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Result<u64, RejectReason> {
        self.array().map(u64::from_le_bytes)
    }

    fn text(&mut self, len: usize) -> Result<String, RejectReason> {
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| RejectReason::BadHeader)
    }
}

/// The header at the start of `bytes`, or [`RejectReason::BadHeader`]: the
/// file is shorter than a header, or its magic, version, flags, block
/// size, total, validator kind or a length is not one this version writes,
/// or its CRC does not match (design §11.5.5). A writer never writes a
/// total of zero: a resumable download has a known, non-zero size.
pub(crate) fn parse_header(bytes: &[u8]) -> Result<StoredHeader, RejectReason> {
    let mut cursor = Cursor { bytes, at: 0 };
    let bad = Err(RejectReason::BadHeader);
    if cursor.array::<8>()? != MAGIC || cursor.u16()? != VERSION || cursor.u16()? != 0 {
        return bad;
    }
    let block_size = cursor.u32()?;
    if u64::from(block_size) != BLOCK_SIZE.get() {
        return bad;
    }
    let total = cursor.u64()?;
    if total == 0 {
        return bad;
    }
    let kind = cursor.u8()?;
    let len = usize::from(cursor.u16()?);
    if len > MAX_VALIDATOR_LEN {
        return bad;
    }
    let value = cursor.text(len)?;
    let validator = match kind {
        1 => Validator::StrongEtag(value),
        2 => Validator::LastModified(value),
        _ => return bad,
    };
    let source = match cursor.u16()? {
        NO_SOURCE => None,
        len if usize::from(len) <= MAX_SOURCE_LEN => Some(cursor.text(len.into())?),
        _ => return bad,
    };
    let covered = cursor.at;
    if cursor.u32()? != crc32c::crc32c(&bytes[..covered]) {
        return bad;
    }
    Ok(StoredHeader {
        block_size,
        total,
        validator,
        source,
        len: cursor.at as u64,
    })
}

/// The range of block `index` of a file of `total` bytes in blocks of
/// `block_size`, or `None` when its offset overflows or it starts at or
/// past the total (design §11.5.5: checked arithmetic). The last block may
/// be shorter.
pub(crate) fn block_range(index: u64, block_size: u32, total: u64) -> Option<Range<u64>> {
    let start = index.checked_mul(block_size.into())?;
    let left = total.checked_sub(start).filter(|&left| left > 0)?;
    Some(start..start + left.min(block_size.into()))
}

/// A record whose checksum, kind, reserved bytes and length are right, at
/// `position` in the file; whether it is kept depends on the other records
/// of its block.
#[derive(Debug, Clone, Copy)]
struct Valid {
    index: u64,
    position: u64,
    crc: u32,
}

/// Read `bytes`, a whole state file: the header, then every record after it
/// (design §11.5.5, §11.5.10 steps 1 and 3), as [`read_state`] does without
/// its bound.
///
/// - A tail shorter than a record is cut off: [`DropReason::TornTail`].
/// - A record whose CRC, kind or reserved bytes are wrong, or whose length
///   is not its block's (zero included): [`DropReason::BadRecord`].
/// - A block that starts at or past the total, or whose offset overflows:
///   [`DropReason::OutOfBounds`].
/// - A record that repeats the one kept is dropped silently.
/// - Once two valid records name a block with different CRCs, no record of
///   it is kept: the one kept and every one after are
///   [`DropReason::Conflict`].
///
/// # Errors
///
/// The header is bad: [`RejectReason::BadHeader`].
///
/// # Panics
///
/// There was no memory for the records. Only tests scan bytes in memory;
/// a task reads through [`read_state`], which reports that as an error.
#[cfg(test)]
pub(crate) fn scan(bytes: &[u8]) -> Result<Scanned, RejectReason> {
    let header = parse_header(bytes)?;
    match scan_records(&bytes, header, bytes.len() as u64) {
        Ok(scanned) => Ok(scanned),
        Err(ReadError::Rejected(reason)) => Err(reason),
        Err(ReadError::Io(e)) => panic!("the bytes in memory could not be scanned: {e}"),
    }
}

/// The records a chunk holds: 65,520 bytes, so the records are read through
/// one buffer of that size, however long the file is.
pub(crate) const CHUNK_RECORDS: usize = 2730;

/// Scan the records of `file` after `header`, up to `len`, one chunk of
/// [`CHUNK_RECORDS`] at a time; what grows with them is allocated
/// fallibly.
fn scan_records(file: &dyn ReadAt, header: StoredHeader, len: u64) -> Result<Scanned, ReadError> {
    let mut valid = Vec::new();
    let mut dropped = Vec::new();
    let mut chunk = vec![0; CHUNK_RECORDS * RECORD_LEN];
    let mut position = header.len;
    while position < len {
        let want = (chunk.len() as u64).min(len - position) as usize;
        let got = read_full(file, &mut chunk[..want], position)?;
        let (records, tail) = chunk[..got].as_chunks::<RECORD_LEN>();
        for record in records {
            if let Some(record) = check_record(record, &header, position, &mut dropped)? {
                try_push(&mut valid, record)?;
            }
            position += RECORD_LEN as u64;
        }
        if !tail.is_empty() {
            try_push(
                &mut dropped,
                DroppedRecord {
                    position,
                    reason: DropReason::TornTail,
                    range: None,
                },
            )?;
            break;
        }
        if got < want {
            // The file ended sooner than its length said.
            break;
        }
    }
    let records = resolve(valid, &header, &mut dropped)?;
    // A conflict drops a record kept before it, so the order is restored.
    // Positions are unique, and an unstable sort allocates nothing.
    dropped.sort_unstable_by_key(|drop| drop.position);
    Ok(Scanned {
        header,
        records,
        dropped,
    })
}

/// Push `item`, failing rather than aborting when there is no memory: an
/// io error of kind [`io::ErrorKind::OutOfMemory`].
pub(crate) fn try_push<T>(items: &mut Vec<T>, item: T) -> io::Result<()> {
    items
        .try_reserve(1)
        .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e))?;
    items.push(item);
    Ok(())
}

/// Keep, of the `valid` records in file order, the first of each block
/// unless another of it has another CRC: then report the one kept and
/// every one after as conflicts. A repeat of the one kept before that is
/// dropped silently (design §11.5.5).
fn resolve(
    mut valid: Vec<Valid>,
    header: &StoredHeader,
    dropped: &mut Vec<DroppedRecord>,
) -> io::Result<Vec<StoredRecord>> {
    // By block, then in file order; an unstable sort allocates nothing,
    // and positions are unique.
    valid.sort_unstable_by_key(|record| (record.index, record.position));
    let mut records = Vec::new();
    for block in valid.chunk_by(|a, b| a.index == b.index) {
        let first = block[0];
        let range = block_range(first.index, header.block_size, header.total)
            .expect("a valid record is in bounds");
        let conflict = |position| DroppedRecord {
            position,
            reason: DropReason::Conflict,
            range: Some(range.clone()),
        };
        let mut conflicting = false;
        for record in &block[1..] {
            if !conflicting && record.crc == first.crc {
                continue;
            }
            if !conflicting {
                conflicting = true;
                try_push(dropped, conflict(first.position))?;
            }
            try_push(dropped, conflict(record.position))?;
        }
        if !conflicting {
            try_push(
                &mut records,
                StoredRecord {
                    position: first.position,
                    block: CheckpointBlock {
                        index: first.index,
                        len: (range.end - range.start) as u32,
                        crc: first.crc,
                    },
                },
            )?;
        }
    }
    Ok(records)
}

/// Check the record at `position` against `header`: the record when it is
/// valid; otherwise `None`, and why in `dropped`.
fn check_record(
    record: &[u8; RECORD_LEN],
    header: &StoredHeader,
    position: u64,
    dropped: &mut Vec<DroppedRecord>,
) -> io::Result<Option<Valid>> {
    let mut drop = |reason, range| {
        try_push(
            dropped,
            DroppedRecord {
                position,
                reason,
                range,
            },
        )
        .map(|()| None)
    };
    let u32_at = |at: usize| u32::from_le_bytes(record[at..at + 4].try_into().expect("4 bytes"));
    if u32_at(20) != crc32c::crc32c(&record[..20]) {
        return drop(DropReason::BadRecord, None);
    }
    let index = u64::from_le_bytes(record[0..8].try_into().expect("8 bytes"));
    let Some(range) = block_range(index, header.block_size, header.total) else {
        return drop(DropReason::OutOfBounds, None);
    };
    if record[16] != COMPLETE_BLOCK
        || record[17..20] != [0; 3]
        || u64::from(u32_at(8)) != range.end - range.start
    {
        return drop(DropReason::BadRecord, Some(range));
    }
    Ok(Some(Valid {
        index,
        position,
        crc: u32_at(12),
    }))
}

/// The longest header: a validator of [`MAX_VALIDATOR_LEN`] bytes and a
/// source of [`MAX_SOURCE_LEN`].
pub(crate) const MAX_HEADER_LEN: u64 =
    (8 + 2 + 2 + 4 + 8 + 1 + 2 + MAX_VALIDATOR_LEN + 2 + MAX_SOURCE_LEN + 4) as u64;

/// The shortest header: an empty validator and no source.
pub(crate) const MIN_HEADER_LEN: u64 = 8 + 2 + 2 + 4 + 8 + 1 + 2 + 2 + 4;

/// The longest file a writer leaves after `header`, with room to spare
/// (design §14.30.1): a run records a block at most once, and a resume
/// rewrites the records it keeps, so a file holds at most one record per
/// block and a torn one. Twice as many records and one more are allowed.
pub(crate) fn len_bound(header: &StoredHeader) -> u64 {
    let blocks = header.total.div_ceil(header.block_size.into());
    let records = blocks.saturating_mul(2).saturating_add(1);
    header
        .len
        .saturating_add(records.saturating_mul(RECORD_LEN as u64))
}

/// A file read at offsets: the state file's locked handle, or a test's.
pub(crate) trait ReadAt {
    /// The length of the file.
    fn len(&self) -> io::Result<u64>;
    /// Read into `buf` at `offset`; how many bytes were read, 0 at the end.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;
}

impl ReadAt for File {
    fn len(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        read_at(self, buf, offset)
    }
}

#[cfg(test)]
impl ReadAt for &[u8] {
    fn len(&self) -> io::Result<u64> {
        Ok(<[u8]>::len(self) as u64)
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let rest = usize::try_from(offset)
            .ok()
            .and_then(|offset| self.get(offset..))
            .unwrap_or_default();
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        Ok(n)
    }
}

/// Why a state file was not read.
#[derive(Debug)]
pub(crate) enum ReadError {
    /// The file is bad as a whole: the task does not resume from it.
    Rejected(RejectReason),
    /// It could not be read, or there was no memory for its records.
    Io(io::Error),
}

impl From<io::Error> for ReadError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

/// Read and scan the state file through `file`, its locked handle (design
/// §11.5.4: only through that handle), within the bound of design
/// §14.30.1: a file shorter than any header is not read; of another, the
/// header is read first, and the records only when the file is no longer
/// than [`len_bound`] allows for that header, one chunk at a time.
///
/// # Errors
///
/// [`RejectReason::BadHeader`] or [`RejectReason::Oversized`], or the file
/// could not be read.
pub(crate) fn read_state(file: &dyn ReadAt) -> Result<Scanned, ReadError> {
    let len = file.len()?;
    if len < MIN_HEADER_LEN {
        return Err(ReadError::Rejected(RejectReason::BadHeader));
    }
    let mut head = vec![0; len.min(MAX_HEADER_LEN) as usize];
    let got = read_full(file, &mut head, 0)?;
    let header = parse_header(&head[..got]).map_err(ReadError::Rejected)?;
    if len > len_bound(&header) {
        return Err(ReadError::Rejected(RejectReason::Oversized));
    }
    scan_records(file, header, len)
}

/// Fill `buf` from `offset`; fewer bytes only when the file ends sooner.
fn read_full(file: &dyn ReadAt, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut at = 0;
    while at < buf.len() {
        match file.read_at(&mut buf[at..], offset + at as u64) {
            Ok(0) => break,
            Ok(n) => at += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(at)
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

#[cfg(test)]
mod tests;
