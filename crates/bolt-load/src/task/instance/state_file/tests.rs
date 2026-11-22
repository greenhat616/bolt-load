//! The state file's format and its writer (design §11.5.5).

use std::sync::Mutex;

use pretty_assertions::assert_eq;

use super::*;

/// A file operation, as a [`MemoryIo`] saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Op {
    Write { offset: u64, len: usize },
    SetLen(u64),
    Sync,
}

#[derive(Default)]
struct MemoryState {
    bytes: Vec<u8>,
    /// What survived the last sync.
    synced: Vec<u8>,
    ops: Vec<Op>,
    /// The operations to fail, by their number in `ops`, counted from 0.
    fail: Vec<usize>,
    /// A failed write still writes this many of its bytes.
    torn: usize,
}

/// A state file in memory: it logs every operation, fails those the test
/// names, and keeps what the last sync made durable.
#[derive(Clone, Default)]
pub(crate) struct MemoryIo(Arc<Mutex<MemoryState>>);

impl MemoryIo {
    pub(crate) fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().bytes.clone()
    }

    /// The bytes as of the last sync that succeeded.
    pub(crate) fn synced(&self) -> Vec<u8> {
        self.0.lock().unwrap().synced.clone()
    }

    pub(crate) fn ops(&self) -> Vec<Op> {
        self.0.lock().unwrap().ops.clone()
    }

    /// Fail the operation numbered `n`, counted from the first, 0.
    pub(crate) fn fail_op(&self, n: usize) {
        self.0.lock().unwrap().fail.push(n);
    }

    /// A failed write writes its first `n` bytes all the same.
    pub(crate) fn tear(&self, n: usize) {
        self.0.lock().unwrap().torn = n;
    }

    /// Run the operation `op`; `true` if it is to fail.
    fn log(state: &mut MemoryState, op: Op) -> bool {
        let n = state.ops.len();
        state.ops.push(op);
        state.fail.contains(&n)
    }
}

fn injected() -> io::Error {
    io::Error::other("injected state file failure")
}

impl StateIo for MemoryIo {
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        let mut state = self.0.lock().unwrap();
        let fails = Self::log(
            &mut state,
            Op::Write {
                offset,
                len: bytes.len(),
            },
        );
        let written = if fails {
            state.torn.min(bytes.len())
        } else {
            bytes.len()
        };
        let end = offset as usize + written;
        if state.bytes.len() < end {
            state.bytes.resize(end, 0);
        }
        state.bytes[offset as usize..end].copy_from_slice(&bytes[..written]);
        if fails { Err(injected()) } else { Ok(()) }
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        let mut state = self.0.lock().unwrap();
        if Self::log(&mut state, Op::SetLen(len)) {
            return Err(injected());
        }
        state.bytes.resize(len as usize, 0);
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        let mut state = self.0.lock().unwrap();
        if Self::log(&mut state, Op::Sync) {
            return Err(injected());
        }
        state.synced = state.bytes.clone();
        Ok(())
    }
}

/// A header as read back, for tests: C20 reads it for real.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedHeader {
    pub(crate) block_size: u32,
    pub(crate) total: u64,
    pub(crate) validator: Validator,
    pub(crate) source: Option<String>,
    pub(crate) len: usize,
}

/// Read the header at the start of `bytes`, checking its CRC; `None` if it
/// is not a valid one.
pub(crate) fn decode_header(bytes: &[u8]) -> Option<DecodedHeader> {
    let u16_at = |at: usize| Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?));
    if bytes.get(..8)? != MAGIC || u16_at(8)? != VERSION || u16_at(10)? != 0 {
        return None;
    }
    let block_size = u32::from_le_bytes(bytes.get(12..16)?.try_into().ok()?);
    let total = u64::from_le_bytes(bytes.get(16..24)?.try_into().ok()?);
    let kind = *bytes.get(24)?;
    let validator_len = usize::from(u16_at(25)?);
    let value = String::from_utf8(bytes.get(27..27 + validator_len)?.to_vec()).ok()?;
    let validator = match kind {
        1 => Validator::StrongEtag(value),
        2 => Validator::LastModified(value),
        _ => return None,
    };
    let mut at = 27 + validator_len;
    let source_len = u16_at(at)?;
    at += 2;
    let source = if source_len == NO_SOURCE {
        None
    } else {
        let len = usize::from(source_len);
        let source = String::from_utf8(bytes.get(at..at + len)?.to_vec()).ok()?;
        at += len;
        Some(source)
    };
    let crc = u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?);
    (crc == crc32c::crc32c(&bytes[..at])).then_some(DecodedHeader {
        block_size,
        total,
        validator,
        source,
        len: at + 4,
    })
}

/// The valid records in `bytes`, after a header of `header_len` bytes; a
/// torn or corrupt record ends them.
pub(crate) fn decode_records(bytes: &[u8], header_len: usize) -> Vec<CheckpointBlock> {
    bytes[header_len..]
        .as_chunks::<RECORD_LEN>()
        .0
        .iter()
        .map_while(|record| {
            let crc = u32::from_le_bytes(record[20..24].try_into().unwrap());
            (crc == crc32c::crc32c(&record[..20]) && record[16..20] == [1, 0, 0, 0]).then(|| {
                CheckpointBlock {
                    index: u64::from_le_bytes(record[0..8].try_into().unwrap()),
                    len: u32::from_le_bytes(record[8..12].try_into().unwrap()),
                    crc: u32::from_le_bytes(record[12..16].try_into().unwrap()),
                }
            })
        })
        .collect()
}

fn etag() -> Validator {
    Validator::StrongEtag("\"v1\"".to_owned())
}

fn header_bytes() -> Vec<u8> {
    encode_header(&Header {
        block_size: 1 << 20,
        total: 5 << 20,
        validator: &etag(),
        source: Some("src"),
    })
    .unwrap()
}

fn block(index: u64) -> CheckpointBlock {
    CheckpointBlock {
        index,
        len: 1 << 20,
        crc: 0x1234_5678 ^ index as u32,
    }
}

/// The header's bytes, field by field, as §11.5.5 lays them out.
#[test]
fn test_the_header_layout() {
    let bytes = header_bytes();
    let mut expected = Vec::new();
    expected.extend_from_slice(b"BOLTLOAD");
    expected.extend_from_slice(&[1, 0]);
    expected.extend_from_slice(&[0, 0]);
    expected.extend_from_slice(&[0, 0, 0x10, 0]);
    expected.extend_from_slice(&[0, 0, 0x50, 0, 0, 0, 0, 0]);
    expected.push(1);
    expected.extend_from_slice(&[4, 0]);
    expected.extend_from_slice(b"\"v1\"");
    expected.extend_from_slice(&[3, 0]);
    expected.extend_from_slice(b"src");
    let crc = crc32c::crc32c(&expected);
    expected.extend_from_slice(&crc.to_le_bytes());
    assert_eq!(bytes, expected);
    assert_eq!(
        decode_header(&bytes),
        Some(DecodedHeader {
            block_size: 1 << 20,
            total: 5 << 20,
            validator: etag(),
            source: Some("src".to_owned()),
            len: bytes.len(),
        })
    );
}

/// A `Last-Modified` is kind 2; no source is length 0xFFFF; an empty
/// source is length 0.
#[test]
fn test_the_validator_kinds_and_the_source() {
    let date = Validator::LastModified("Sun, 08 Nov 2026 12:00:09 GMT".to_owned());
    for (source, len) in [(None, [0xFF, 0xFF]), (Some(""), [0, 0])] {
        let bytes = encode_header(&Header {
            block_size: 1 << 20,
            total: 1,
            validator: &date,
            source,
        })
        .unwrap();
        assert_eq!(bytes[24], 2);
        let at = 27 + date.as_str().len();
        assert_eq!(bytes[at..at + 2], len);
        let decoded = decode_header(&bytes).unwrap();
        assert_eq!(decoded.validator, date);
        assert_eq!(decoded.source.as_deref(), source);
    }
}

/// A validator of 1024 bytes and a source of 4096 fit; one byte more does
/// not, and nothing is written for it.
#[test]
fn test_the_length_limits() {
    let header = |validator: &Validator, source: &str| {
        encode_header(&Header {
            block_size: 1 << 20,
            total: 1,
            validator,
            source: Some(source),
        })
    };
    let quoted = |len: usize| Validator::StrongEtag(format!("\"{}\"", "a".repeat(len - 2)));
    assert!(header(&quoted(MAX_VALIDATOR_LEN), "").is_ok());
    assert_eq!(
        header(&quoted(MAX_VALIDATOR_LEN + 1), ""),
        Err(HeaderError::ValidatorTooLong)
    );
    assert!(header(&etag(), &"s".repeat(MAX_SOURCE_LEN)).is_ok());
    assert_eq!(
        header(&etag(), &"s".repeat(MAX_SOURCE_LEN + 1)),
        Err(HeaderError::SourceTooLong)
    );
}

/// A record's bytes, field by field; its CRC covers the 20 bytes before it.
#[test]
fn test_the_record_layout() {
    let record = encode_record(&CheckpointBlock {
        index: 0x0102,
        len: 0x0300,
        crc: 0xAABB_CCDD,
    });
    let mut expected = vec![0x02, 0x01, 0, 0, 0, 0, 0, 0];
    expected.extend_from_slice(&[0, 3, 0, 0]);
    expected.extend_from_slice(&[0xDD, 0xCC, 0xBB, 0xAA]);
    expected.extend_from_slice(&[1, 0, 0, 0]);
    let crc = crc32c::crc32c(&expected);
    expected.extend_from_slice(&crc.to_le_bytes());
    assert_eq!(record.to_vec(), expected);
}

/// The header, then the records of each checkpoint after it, each synced
/// before the end moves.
#[test]
fn test_records_append_after_the_header() {
    let io = MemoryIo::default();
    let mut state = StateFile::new(Box::new(io.clone()));
    let header = header_bytes();
    state.write_header(&header).unwrap();
    assert_eq!(state.end(), header.len() as u64);
    state.append(&[block(0), block(1)]).unwrap();
    state.append(&[block(4)]).unwrap();
    let end = header.len() as u64 + 3 * RECORD_LEN as u64;
    assert_eq!(state.end(), end);
    assert_eq!(
        io.ops(),
        [
            Op::Write {
                offset: 0,
                len: header.len()
            },
            Op::Sync,
            Op::Write {
                offset: header.len() as u64,
                len: 2 * RECORD_LEN
            },
            Op::Sync,
            Op::Write {
                offset: header.len() as u64 + 2 * RECORD_LEN as u64,
                len: RECORD_LEN
            },
            Op::Sync,
        ]
    );
    assert_eq!(io.synced(), io.bytes());
    let decoded = decode_header(&io.bytes()).unwrap();
    assert_eq!(
        decode_records(&io.bytes(), decoded.len),
        [block(0), block(1), block(4)]
    );
}

/// A failed write or sync of an append leaves the end where it was; the
/// rollback cuts what it left there and syncs, and the next append goes at
/// the same place.
#[test]
fn test_a_failed_append_rolls_back_to_the_end() {
    // Ops: 0 write header, 1 sync, 2 append write, 3 append sync.
    for failed in [2, 3] {
        let io = MemoryIo::default();
        io.fail_op(failed);
        io.tear(RECORD_LEN / 2);
        let mut state = StateFile::new(Box::new(io.clone()));
        let header = header_bytes();
        state.write_header(&header).unwrap();
        assert!(state.append(&[block(0), block(1)]).is_err(), "{failed}");
        assert_eq!(state.end(), header.len() as u64);
        state.roll_back().unwrap();
        let ops = io.ops();
        assert_eq!(
            ops[ops.len() - 2..],
            [Op::SetLen(header.len() as u64), Op::Sync],
            "{failed}"
        );
        assert_eq!(io.bytes(), header);
        assert_eq!(io.synced(), header);
        state.append(&[block(2)]).unwrap();
        assert_eq!(
            decode_records(&io.bytes(), header.len()),
            [block(2)],
            "{failed}"
        );
    }
}

/// A failed rollback is an error: the checkpoints stop. The file is valid
/// up to the end, and what lies past it is a torn record, which a reader
/// drops.
#[test]
fn test_a_failed_rollback_is_an_error() {
    // Ops: 0 write header, 1 sync, 2 append write (torn), 3 set_len.
    let io = MemoryIo::default();
    io.fail_op(2);
    io.fail_op(3);
    io.tear(RECORD_LEN - 1);
    let mut state = StateFile::new(Box::new(io.clone()));
    let header = header_bytes();
    state.write_header(&header).unwrap();
    assert!(state.append(&[block(0)]).is_err());
    assert!(state.roll_back().is_err());
    let bytes = io.bytes();
    assert_eq!(bytes.len(), header.len() + RECORD_LEN - 1);
    assert_eq!(decode_header(&bytes).unwrap().len, header.len());
    assert_eq!(decode_records(&bytes, header.len()), []);
}

/// A failed header write or sync is an error, and the end does not move.
#[test]
fn test_a_failed_header_is_an_error() {
    for failed in [0, 1] {
        let io = MemoryIo::default();
        io.fail_op(failed);
        let mut state = StateFile::new(Box::new(io.clone()));
        assert!(state.write_header(&header_bytes()).is_err(), "{failed}");
        assert_eq!(state.end(), 0);
    }
}

/// Invalidation empties the file and syncs it; nothing is left to read.
#[test]
fn test_invalidation_empties_the_file() {
    let io = MemoryIo::default();
    let mut state = StateFile::new(Box::new(io.clone()));
    state.write_header(&header_bytes()).unwrap();
    state.append(&[block(0)]).unwrap();
    state.invalidate().unwrap();
    assert_eq!(state.end(), 0);
    let ops = io.ops();
    assert_eq!(ops[ops.len() - 2..], [Op::SetLen(0), Op::Sync]);
    assert_eq!(io.synced(), Vec::<u8>::new());
}

/// Through a real file's handle: header, records, a rollback and an
/// invalidation land in the file.
#[test]
fn test_the_locked_file_writes_the_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.bin.boltload");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut state = StateFile::new(Box::new(LockedFile(Arc::new(file))));
    let header = header_bytes();
    state.write_header(&header).unwrap();
    state.append(&[block(0), block(1)]).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(decode_header(&bytes).unwrap().len, header.len());
    assert_eq!(decode_records(&bytes, header.len()), [block(0), block(1)]);
    // A rollback at the end changes nothing.
    state.roll_back().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    state.invalidate().unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
}

/// Syncing a file's directory succeeds; it does nothing on Windows.
#[test]
fn test_the_parent_directory_syncs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.bin");
    std::fs::write(&path, b"x").unwrap();
    sync_parent_dir(&path).unwrap();
}

/// The state file is written through the destination's locked handle, so
/// another task that wants the destination finds it in use meanwhile; the
/// release removes it.
#[test]
fn test_the_state_is_written_through_the_lock() {
    use crate::task::{
        DownloadError,
        instance::destination_lock::{DestinationLock, LockHooks, Unsupported, state_path_for},
    };

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out.bin");
    let lock = DestinationLock::acquire(&dest, Unsupported::Fail, &LockHooks::default()).unwrap();
    let mut state = StateFile::new(Box::new(LockedFile(lock.handle().unwrap())));
    let header = header_bytes();
    state.write_header(&header).unwrap();
    state.append(&[block(0)]).unwrap();
    assert!(matches!(
        DestinationLock::acquire(&dest, Unsupported::Fail, &LockHooks::default()),
        Err(DownloadError::InUse { .. })
    ));
    drop(state);
    let path = state_path_for(&dest);
    let bytes = lock
        .handle()
        .map(|file| {
            use std::io::{Read, Seek, SeekFrom};
            let mut file = &*file;
            file.seek(SeekFrom::Start(0)).unwrap();
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            bytes
        })
        .unwrap();
    assert_eq!(decode_records(&bytes, header.len()), [block(0)]);
    lock.release();
    assert!(!path.exists());
}

/// A rewrite cuts the file to the header and syncs before it writes the
/// kept records, then syncs them (design §11.5.5, "续传"); records go after
/// them.
#[test]
fn test_a_rewrite_cuts_then_writes_the_records() {
    let io = MemoryIo::default();
    let mut state = StateFile::new(Box::new(io.clone()));
    let header = header_bytes();
    let len = header.len() as u64;
    state.write_header(&header).unwrap();
    state.append(&[block(0), block(3), block(1)]).unwrap();
    let before = io.ops().len();
    state.rewrite(len, [block(0), block(1)]).unwrap();
    assert_eq!(
        io.ops()[before..],
        [
            Op::SetLen(len),
            Op::Sync,
            Op::Write {
                offset: len,
                len: 2 * RECORD_LEN
            },
            Op::Sync,
        ]
    );
    assert_eq!(state.end(), len + 2 * RECORD_LEN as u64);
    state.append(&[block(2)]).unwrap();
    assert_eq!(io.synced(), io.bytes());
    assert_eq!(
        decode_records(&io.bytes(), header.len()),
        [block(0), block(1), block(2)]
    );
    assert_eq!(io.bytes().len(), header.len() + 3 * RECORD_LEN);
}

/// With no records kept, a rewrite only cuts and syncs.
#[test]
fn test_a_rewrite_of_nothing_only_cuts() {
    let io = MemoryIo::default();
    let mut state = StateFile::new(Box::new(io.clone()));
    let header = header_bytes();
    state.write_header(&header).unwrap();
    state.append(&[block(0)]).unwrap();
    let before = io.ops().len();
    state.rewrite(header.len() as u64, []).unwrap();
    assert_eq!(
        io.ops()[before..],
        [Op::SetLen(header.len() as u64), Op::Sync]
    );
    assert_eq!(io.synced(), header);
    assert_eq!(state.end(), header.len() as u64);
}

/// A failed step of a rewrite is an error. The file holds the old records
/// when the cut failed, and otherwise the header and a prefix of the kept
/// records; a cut that did not sync is never followed by a write.
#[test]
fn test_a_failed_rewrite_is_an_error() {
    // Ops after the setup: 0 set_len, 1 sync, 2 write, 3 sync.
    let header = header_bytes();
    let len = header.len();
    let mut old = header.clone();
    for index in [0, 3] {
        old.extend_from_slice(&encode_record(&block(index)));
    }
    for (failed, ops, bytes) in [
        (0, 1, old.len()),
        (1, 2, len),
        (2, 3, len + RECORD_LEN),
        (3, 4, len + 2 * RECORD_LEN),
    ] {
        let io = MemoryIo::default();
        let mut state = StateFile::new(Box::new(io.clone()));
        state.write_header(&header).unwrap();
        state.append(&[block(0), block(3)]).unwrap();
        let setup = io.ops().len();
        io.fail_op(setup + failed);
        io.tear(RECORD_LEN);
        assert!(
            state.rewrite(len as u64, [block(0), block(1)]).is_err(),
            "{failed}"
        );
        assert_eq!(io.ops().len() - setup, ops, "{failed}");
        assert_eq!(io.bytes().len(), bytes, "{failed}");
        let records = decode_records(&io.bytes(), len);
        assert!(
            records.is_empty()
                || records == [block(0), block(3)]
                || records[..] == [block(0), block(1)][..records.len()],
            "{failed}: {records:?}"
        );
    }
}

/// A rewrite of more records than one batch writes them all, in order,
/// one batch at a time, and syncs once; a failed batch leaves a prefix and
/// the end at the header.
#[test]
fn test_a_rewrite_writes_in_batches() {
    let header = header_bytes();
    let len = header.len() as u64;
    let count = 2 * read::CHUNK_RECORDS + 5;
    let blocks: Vec<_> = (0..count as u64)
        .map(|index| CheckpointBlock {
            index,
            len: 1 << 20,
            crc: index as u32,
        })
        .collect();
    let io = MemoryIo::default();
    let mut state = StateFile::new(Box::new(io.clone()));
    state.write_header(&header).unwrap();
    let before = io.ops().len();
    state.rewrite(len, blocks.iter().copied()).unwrap();
    let batch = read::CHUNK_RECORDS * RECORD_LEN;
    assert_eq!(
        io.ops()[before..],
        [
            Op::SetLen(len),
            Op::Sync,
            Op::Write {
                offset: len,
                len: batch
            },
            Op::Write {
                offset: len + batch as u64,
                len: batch
            },
            Op::Write {
                offset: len + 2 * batch as u64,
                len: 5 * RECORD_LEN
            },
            Op::Sync,
        ]
    );
    assert_eq!(decode_records(&io.bytes(), header.len()), blocks);
    assert_eq!(state.end(), len + (count * RECORD_LEN) as u64);
    // The second batch fails: the first is all that is written.
    let io = MemoryIo::default();
    let mut state = StateFile::new(Box::new(io.clone()));
    state.write_header(&header).unwrap();
    let before = io.ops().len();
    io.fail_op(before + 3);
    assert!(state.rewrite(len, blocks.iter().copied()).is_err());
    assert_eq!(state.end(), len);
    assert_eq!(
        decode_records(&io.bytes(), header.len()),
        blocks[..read::CHUNK_RECORDS]
    );
}

/// A batch torn inside a record leaves the complete records before it,
/// which a reader keeps, and a torn tail, which it cuts off; the end stays
/// at the header. A retried rewrite replaces it all, and an append lands
/// after it.
#[test]
fn test_a_rewrite_torn_inside_a_batch() {
    use crate::task::resume::{DropReason, DroppedRecord};

    let count = 2 * read::CHUNK_RECORDS + 5;
    let header = encode_header(&Header {
        block_size: 1 << 20,
        total: (count as u64 + 1) << 20,
        validator: &etag(),
        source: Some("src"),
    })
    .unwrap();
    let len = header.len() as u64;
    let block = |index: u64| CheckpointBlock {
        index,
        len: 1 << 20,
        crc: index as u32,
    };
    let blocks: Vec<_> = (0..count as u64).map(block).collect();
    let io = MemoryIo::default();
    let mut state = StateFile::new(Box::new(io.clone()));
    state.write_header(&header).unwrap();
    let before = io.ops().len();
    // The second batch writes one record and 7 bytes of the next.
    io.fail_op(before + 3);
    io.tear(RECORD_LEN + 7);
    assert!(state.rewrite(len, blocks.iter().copied()).is_err());
    assert_eq!(state.end(), len);
    let bytes = io.bytes();
    let kept = read::CHUNK_RECORDS + 1;
    assert_eq!(bytes.len(), header.len() + kept * RECORD_LEN + 7);
    let scanned = read::read_state(&bytes.as_slice()).unwrap();
    assert_eq!(
        scanned
            .records
            .iter()
            .map(|record| record.block)
            .collect::<Vec<_>>(),
        blocks[..kept]
    );
    assert_eq!(
        scanned.dropped,
        [DroppedRecord {
            position: len + (kept * RECORD_LEN) as u64,
            reason: DropReason::TornTail,
            range: None,
        }]
    );
    // Retried, it writes everything; an append goes after it.
    state.rewrite(len, blocks.iter().copied()).unwrap();
    let end = len + (count * RECORD_LEN) as u64;
    assert_eq!(state.end(), end);
    state.append(&[block(count as u64)]).unwrap();
    let ops = io.ops();
    assert_eq!(
        ops[ops.len() - 2..],
        [
            Op::Write {
                offset: end,
                len: RECORD_LEN
            },
            Op::Sync
        ]
    );
    let scanned = read::read_state(&io.bytes().as_slice()).unwrap();
    assert_eq!(scanned.dropped, []);
    assert_eq!(scanned.records.len(), count + 1);
    assert_eq!(scanned.records[count].block, block(count as u64));
}
