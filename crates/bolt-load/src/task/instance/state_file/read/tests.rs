//! Reading a state file back (design §11.5.5, §11.5.10).

use pretty_assertions::assert_eq;

use super::*;
use crate::task::instance::state_file::{Header, encode_header, encode_record};

const BS: u64 = BLOCK_SIZE.get();
/// Two whole blocks and a short third one.
const TOTAL: u64 = 2 * BS + 100;

fn etag() -> Validator {
    Validator::StrongEtag("\"v1\"".to_owned())
}

/// The fields of a header, written raw: any value, a valid CRC unless
/// told otherwise.
#[derive(Clone)]
struct Raw {
    magic: [u8; 8],
    version: u16,
    flags: u16,
    block_size: u32,
    total: u64,
    kind: u8,
    validator: Vec<u8>,
    source: Option<Vec<u8>>,
    /// The source length written, when it is not the source's.
    source_len: Option<u16>,
    crc: Option<u32>,
}

impl Default for Raw {
    fn default() -> Self {
        Self {
            magic: MAGIC,
            version: VERSION,
            flags: 0,
            block_size: BS as u32,
            total: TOTAL,
            kind: 1,
            validator: b"\"v1\"".to_vec(),
            source: None,
            source_len: None,
            crc: None,
        }
    }
}

impl Raw {
    fn bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.magic);
        bytes.extend_from_slice(&self.version.to_le_bytes());
        bytes.extend_from_slice(&self.flags.to_le_bytes());
        bytes.extend_from_slice(&self.block_size.to_le_bytes());
        bytes.extend_from_slice(&self.total.to_le_bytes());
        bytes.push(self.kind);
        bytes.extend_from_slice(&(self.validator.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&self.validator);
        match &self.source {
            Some(source) => {
                let len = self.source_len.unwrap_or(source.len() as u16);
                bytes.extend_from_slice(&len.to_le_bytes());
                bytes.extend_from_slice(source);
            }
            None => bytes.extend_from_slice(&self.source_len.unwrap_or(NO_SOURCE).to_le_bytes()),
        }
        let crc = self.crc.unwrap_or_else(|| crc32c::crc32c(&bytes));
        bytes.extend_from_slice(&crc.to_le_bytes());
        bytes
    }
}

fn header() -> Vec<u8> {
    encode_header(&Header {
        block_size: BS as u32,
        total: TOTAL,
        validator: &etag(),
        source: None,
    })
    .unwrap()
}

/// The length block `index` has in a file of [`TOTAL`] bytes.
fn len_of(index: u64) -> u32 {
    block_range(index, BS as u32, TOTAL).map_or(BS as u32, |r| (r.end - r.start) as u32)
}

fn block(index: u64, crc: u32) -> CheckpointBlock {
    CheckpointBlock {
        index,
        len: len_of(index),
        crc,
    }
}

/// A record of `index` with `crc` as the block's CRC.
fn rec(index: u64, crc: u32) -> [u8; RECORD_LEN] {
    encode_record(&block(index, crc))
}

/// A record whose 20 bytes are `edit`ed, then given a valid record CRC.
fn edited(index: u64, edit: impl FnOnce(&mut [u8; RECORD_LEN])) -> [u8; RECORD_LEN] {
    let mut record = rec(index, 7);
    edit(&mut record);
    let crc = crc32c::crc32c(&record[..20]);
    record[20..24].copy_from_slice(&crc.to_le_bytes());
    record
}

/// A state file of [`header`] and `records`.
fn file(records: &[[u8; RECORD_LEN]]) -> Vec<u8> {
    let mut bytes = header();
    for record in records {
        bytes.extend_from_slice(record);
    }
    bytes
}

/// Where record `n` starts.
fn at(n: u64) -> u64 {
    header().len() as u64 + n * RECORD_LEN as u64
}

fn dropped(position: u64, reason: DropReason, range: Option<Range<u64>>) -> DroppedRecord {
    DroppedRecord {
        position,
        reason,
        range,
    }
}

fn kept(n: u64, index: u64, crc: u32) -> StoredRecord {
    StoredRecord {
        position: at(n),
        block: block(index, crc),
    }
}

fn range(index: u64) -> Option<Range<u64>> {
    block_range(index, BS as u32, TOTAL)
}

/// What the writer writes reads back field by field.
#[test]
fn test_a_written_header_reads_back() {
    for (validator, source) in [
        (etag(), None),
        (
            Validator::LastModified("Sun, 08 Nov 2026 12:00:10 GMT".to_owned()),
            Some("https://example.com/a"),
        ),
        (Validator::StrongEtag("\"\"".to_owned()), Some("")),
    ] {
        let bytes = encode_header(&Header {
            block_size: BS as u32,
            total: TOTAL,
            validator: &validator,
            source,
        })
        .unwrap();
        assert_eq!(
            parse_header(&bytes),
            Ok(StoredHeader {
                block_size: BS as u32,
                total: TOTAL,
                validator,
                source: source.map(str::to_owned),
                len: bytes.len() as u64,
            })
        );
    }
    // The raw writer of these tests writes the same bytes.
    assert_eq!(Raw::default().bytes(), header());
}

/// The limits hold exactly: a validator of 1024 bytes and a source of
/// 4096 read back.
#[test]
fn test_the_longest_header_reads_back() {
    let raw = Raw {
        validator: vec![b'v'; MAX_VALIDATOR_LEN],
        source: Some(vec![b's'; MAX_SOURCE_LEN]),
        ..Raw::default()
    };
    let header = parse_header(&raw.bytes()).unwrap();
    assert_eq!(header.validator.as_str().len(), MAX_VALIDATOR_LEN);
    assert_eq!(header.source.unwrap().len(), MAX_SOURCE_LEN);
}

/// Every way a header can be bad rejects the file (design §11.5.5).
#[test]
fn test_a_bad_header_rejects_the_file() {
    let good = Raw::default();
    let cases = [
        (
            "magic",
            Raw {
                magic: *b"BOLTLOAX",
                ..good.clone()
            },
        ),
        (
            "version",
            Raw {
                version: 2,
                ..good.clone()
            },
        ),
        (
            "flags",
            Raw {
                flags: 1,
                ..good.clone()
            },
        ),
        (
            "block size",
            Raw {
                block_size: (BS / 2) as u32,
                ..good.clone()
            },
        ),
        (
            "a total of zero",
            Raw {
                total: 0,
                ..good.clone()
            },
        ),
        (
            "kind 0",
            Raw {
                kind: 0,
                ..good.clone()
            },
        ),
        (
            "kind 3",
            Raw {
                kind: 3,
                ..good.clone()
            },
        ),
        (
            "a validator of 1025 bytes",
            Raw {
                validator: vec![b'v'; MAX_VALIDATOR_LEN + 1],
                ..good.clone()
            },
        ),
        (
            "a validator that is no UTF-8",
            Raw {
                validator: vec![0xFF, 0xFE],
                ..good.clone()
            },
        ),
        (
            "a source of 4097 bytes",
            Raw {
                source: Some(vec![b's'; MAX_SOURCE_LEN + 1]),
                ..good.clone()
            },
        ),
        (
            "a source that is no UTF-8",
            Raw {
                source: Some(vec![0xC3]),
                ..good.clone()
            },
        ),
        (
            "a source length past the file",
            Raw {
                source: Some(b"ab".to_vec()),
                source_len: Some(4000),
                ..good.clone()
            },
        ),
        (
            "the CRC",
            Raw {
                crc: Some(0xDEAD_BEEF),
                ..good.clone()
            },
        ),
    ];
    for (case, raw) in cases {
        assert_eq!(
            parse_header(&raw.bytes()),
            Err(RejectReason::BadHeader),
            "{case}"
        );
        assert_eq!(scan(&raw.bytes()), Err(RejectReason::BadHeader), "{case}");
    }
    // A header cut short anywhere, a torn header write included.
    let bytes = good.bytes();
    for len in 0..bytes.len() {
        assert_eq!(
            parse_header(&bytes[..len]),
            Err(RejectReason::BadHeader),
            "{len} bytes"
        );
    }
    // A flipped bit anywhere.
    for at in 0..bytes.len() {
        let mut flipped = bytes.clone();
        flipped[at] ^= 0x10;
        assert_eq!(
            parse_header(&flipped),
            Err(RejectReason::BadHeader),
            "byte {at}"
        );
    }
}

/// Valid records are kept by block, the last, shorter block included.
#[test]
fn test_valid_records_are_kept() {
    let scanned = scan(&file(&[rec(2, 30), rec(0, 10), rec(1, 20)])).unwrap();
    assert_eq!(scanned.header.len, header().len() as u64);
    assert_eq!(
        scanned.records,
        [kept(1, 0, 10), kept(2, 1, 20), kept(0, 2, 30)]
    );
    assert_eq!(scanned.records[2].block.len, 100);
    assert_eq!(scanned.dropped, []);
    // A header alone holds no records.
    assert_eq!(scan(&header()).unwrap().records, []);
}

/// A tail shorter than a record is cut off, whatever its length.
#[test]
fn test_a_torn_tail_is_cut_off() {
    for torn in 1..RECORD_LEN {
        let mut bytes = file(&[rec(0, 10)]);
        bytes.extend_from_slice(&rec(1, 20)[..torn]);
        let scanned = scan(&bytes).unwrap();
        assert_eq!(scanned.records, [kept(0, 0, 10)], "{torn}");
        assert_eq!(
            scanned.dropped,
            [dropped(at(1), DropReason::TornTail, None)],
            "{torn}"
        );
    }
}

/// A record whose CRC, kind, reserved bytes or length is wrong is dropped;
/// the records around it are kept, a valid one of the same block after it
/// included.
#[test]
fn test_bad_records_are_dropped() {
    let mut flipped = rec(1, 20);
    flipped[3] ^= 1;
    let cases = [
        ("the record CRC", flipped, None),
        ("the kind", edited(1, |r| r[16] = 2), range(1)),
        ("kind 0", edited(1, |r| r[16] = 0), range(1)),
        ("the reserved bytes", edited(1, |r| r[18] = 1), range(1)),
        (
            "length 0",
            edited(1, |r| r[8..12].copy_from_slice(&0u32.to_le_bytes())),
            range(1),
        ),
        (
            "a short length",
            edited(1, |r| {
                r[8..12].copy_from_slice(&(BS as u32 - 1).to_le_bytes())
            }),
            range(1),
        ),
        (
            "a whole block's length for the last one",
            edited(2, |r| r[8..12].copy_from_slice(&(BS as u32).to_le_bytes())),
            range(2),
        ),
    ];
    for (case, bad, range) in cases {
        let scanned = scan(&file(&[rec(0, 10), bad, rec(1, 20)])).unwrap();
        assert_eq!(scanned.records, [kept(0, 0, 10), kept(2, 1, 20)], "{case}");
        assert_eq!(
            scanned.dropped,
            [dropped(at(1), DropReason::BadRecord, range)],
            "{case}"
        );
    }
}

/// A block past the total, or whose offset overflows, is out of bounds;
/// the last block of a total near the limit is not.
#[test]
fn test_blocks_out_of_bounds_are_dropped() {
    for index in [3, 4, u64::MAX / BS + 1, u64::MAX] {
        let scanned = scan(&file(&[
            rec(0, 10),
            edited(0, |r| {
                r[0..8].copy_from_slice(&index.to_le_bytes());
            }),
        ]))
        .unwrap();
        assert_eq!(scanned.records, [kept(0, 0, 10)], "{index}");
        assert_eq!(
            scanned.dropped,
            [dropped(at(1), DropReason::OutOfBounds, None)],
            "{index}"
        );
    }
    let last = u64::MAX / BS;
    assert_eq!(
        block_range(last, BS as u32, u64::MAX),
        Some(last * BS..u64::MAX)
    );
    assert_eq!(block_range(last + 1, BS as u32, u64::MAX), None);
    assert_eq!(block_range(0, BS as u32, 0), None);
}

/// A record that repeats one kept is dropped silently.
#[test]
fn test_repeated_records_are_kept_once() {
    let scanned = scan(&file(&[rec(0, 10), rec(1, 20), rec(0, 10), rec(0, 10)])).unwrap();
    assert_eq!(scanned.records, [kept(0, 0, 10), kept(1, 1, 20)]);
    assert_eq!(scanned.dropped, []);
}

/// Once two records of a block differ, none of it is kept, in any order:
/// the one kept and every one after are conflicts; a repeat before the
/// conflict was dropped silently (design §11.5.5).
#[test]
fn test_conflicting_records_drop_the_block() {
    let (a, b) = (rec(1, 1), rec(1, 2));
    let conflict = |n| dropped(at(n), DropReason::Conflict, range(1));
    // Each case: the records of the block, and which of them conflict.
    type Case = (&'static str, Vec<[u8; RECORD_LEN]>, Vec<u64>);
    let cases: [Case; 6] = [
        ("A,B", vec![a, b], vec![0, 1]),
        ("A,B,A", vec![a, b, a], vec![0, 1, 2]),
        ("A,A,B", vec![a, a, b], vec![0, 2]),
        ("B,A,A", vec![b, a, a], vec![0, 1, 2]),
        ("A,B,B", vec![a, b, b], vec![0, 1, 2]),
        ("A,B,C", vec![a, b, rec(1, 3)], vec![0, 1, 2]),
    ];
    for (case, records, conflicts) in cases {
        let mut records = records;
        // Another block before and after is not touched.
        records.insert(0, rec(0, 10));
        records.push(rec(2, 30));
        let scanned = scan(&file(&records)).unwrap();
        let last = records.len() as u64 - 1;
        assert_eq!(
            scanned.records,
            [kept(0, 0, 10), kept(last, 2, 30)],
            "{case}"
        );
        assert_eq!(
            scanned.dropped,
            conflicts
                .iter()
                .map(|&n| conflict(n + 1))
                .collect::<Vec<_>>(),
            "{case}"
        );
    }
}

/// The dropped records come in file order, though a conflict drops one
/// kept earlier.
#[test]
fn test_dropped_records_are_in_file_order() {
    let mut bad = rec(2, 30);
    bad[0] ^= 1;
    let scanned = scan(&file(&[rec(1, 1), bad, rec(1, 2)])).unwrap();
    assert_eq!(scanned.records, []);
    assert_eq!(
        scanned.dropped,
        [
            dropped(at(0), DropReason::Conflict, range(1)),
            dropped(at(1), DropReason::BadRecord, None),
            dropped(at(2), DropReason::Conflict, range(1)),
        ]
    );
}

/// The record at place `n` after the header of a [`Counted`] file.
type Records = Box<dyn Fn(u64) -> [u8; RECORD_LEN]>;

/// A state file only as long as it says: a header, then the records
/// `records` makes. It counts the bytes read and the longest buffer read
/// into, reads at most 4 KiB at a time, and may fail every other read with
/// `Interrupted`.
struct Counted {
    len: u64,
    head: Vec<u8>,
    records: Records,
    interrupt: bool,
    calls: std::cell::Cell<u64>,
    read: std::cell::Cell<u64>,
    /// The longest buffer asked to fill after the header.
    widest: std::cell::Cell<usize>,
}

impl Counted {
    fn new(head: Vec<u8>, records: impl Fn(u64) -> [u8; RECORD_LEN] + 'static, len: u64) -> Self {
        Self {
            len,
            head,
            records: Box::new(records),
            interrupt: false,
            calls: std::cell::Cell::new(0),
            read: std::cell::Cell::new(0),
            widest: std::cell::Cell::new(0),
        }
    }

    /// Repeats of one record.
    fn repeating(head: Vec<u8>, record: [u8; RECORD_LEN], len: u64) -> Self {
        Self::new(head, move |_| record, len)
    }
}

impl ReadAt for Counted {
    fn len(&self) -> io::Result<u64> {
        Ok(self.len)
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.calls.set(self.calls.get() + 1);
        if self.interrupt && self.calls.get() % 2 == 1 {
            return Err(io::ErrorKind::Interrupted.into());
        }
        if offset >= self.head.len() as u64 {
            self.widest.set(self.widest.get().max(buf.len()));
        }
        // Short reads, as a file may give them.
        let n = (buf.len() as u64)
            .min(self.len.saturating_sub(offset))
            .min(4096) as usize;
        let head = self.head.len() as u64;
        let mut cached: Option<(u64, [u8; RECORD_LEN])> = None;
        for (i, byte) in buf[..n].iter_mut().enumerate() {
            let at = offset + i as u64;
            *byte = match self.head.get(at as usize) {
                Some(&byte) if at < head => byte,
                _ => {
                    let place = (at - head) / RECORD_LEN as u64;
                    let record = match cached {
                        Some((cached_place, record)) if cached_place == place => record,
                        _ => {
                            let record = (self.records)(place);
                            cached = Some((place, record));
                            record
                        }
                    };
                    record[((at - head) % RECORD_LEN as u64) as usize]
                }
            };
        }
        self.read.set(self.read.get() + n as u64);
        Ok(n)
    }
}

/// The bound allows twice the records of every block and one more,
/// whatever the total.
#[test]
fn test_the_bound_follows_the_header() {
    let header = parse_header(&header()).unwrap();
    assert_eq!(len_bound(&header), header.len + 7 * RECORD_LEN as u64);
    let huge = StoredHeader {
        total: u64::MAX,
        ..header.clone()
    };
    // 2^44 blocks: no limit but the arithmetic's.
    let blocks = u64::MAX.div_ceil(BS);
    assert_eq!(
        len_bound(&huge),
        header.len + (2 * blocks + 1) * RECORD_LEN as u64
    );
    let longest = Raw {
        validator: vec![b'v'; MAX_VALIDATOR_LEN],
        source: Some(vec![b's'; MAX_SOURCE_LEN]),
        ..Raw::default()
    };
    assert_eq!(MAX_HEADER_LEN as usize, longest.bytes().len());
    let shortest = Raw {
        validator: Vec::new(),
        ..Raw::default()
    };
    assert_eq!(MIN_HEADER_LEN as usize, shortest.bytes().len());
}

/// A file exactly as long as the bound is read whole; one byte longer is
/// rejected after no more than the longest header was read.
#[test]
fn test_a_file_past_its_bound_is_not_read() {
    // A thousand blocks: the bound is far past the longest header.
    let head = Raw {
        total: 1000 * BS,
        ..Raw::default()
    }
    .bytes();
    let bound = len_bound(&parse_header(&head).unwrap());
    assert_eq!(bound, head.len() as u64 + 2001 * RECORD_LEN as u64);
    assert!(bound > 2 * MAX_HEADER_LEN);
    // Repeats of one record, which a scan keeps once.
    let at_bound = Counted::repeating(head.clone(), rec(0, 10), bound);
    let scanned = read_state(&at_bound).unwrap();
    assert_eq!(scanned.records.len(), 1);
    assert_eq!(scanned.dropped, []);
    assert_eq!(
        at_bound.read.get(),
        MAX_HEADER_LEN + bound - head.len() as u64
    );
    let past = Counted::repeating(head.clone(), rec(0, 10), bound + 1);
    assert!(matches!(
        read_state(&past),
        Err(ReadError::Rejected(RejectReason::Oversized))
    ));
    assert_eq!(past.read.get(), MAX_HEADER_LEN);
    let endless = Counted::repeating(head, rec(0, 10), u64::MAX);
    assert!(matches!(
        read_state(&endless),
        Err(ReadError::Rejected(RejectReason::Oversized))
    ));
    assert_eq!(endless.read.get(), MAX_HEADER_LEN);
}

/// What a writer leaves for a download of 3 TiB cancelled one block before
/// its end: a record for every other block, 75,497,448 bytes after the
/// header, more than any fixed limit of 64 MiB would take. It is read
/// whole, through one chunk at a time.
#[test]
fn test_a_long_state_file_is_read_in_chunks() {
    let total = 3 << 40;
    let blocks = total / BS;
    let head = Raw {
        total,
        ..Raw::default()
    }
    .bytes();
    let records = blocks - 1;
    let len = head.len() as u64 + records * RECORD_LEN as u64;
    assert_eq!(len - head.len() as u64, 75_497_448);
    assert!(len > 64 << 20);
    let file = Counted::new(
        head,
        |n| {
            encode_record(&CheckpointBlock {
                index: n,
                len: BS as u32,
                crc: n as u32,
            })
        },
        len,
    );
    let scanned = read_state(&file).unwrap();
    assert_eq!(scanned.records.len() as u64, records);
    assert_eq!(scanned.dropped, []);
    let last = scanned.records.last().unwrap();
    assert_eq!(last.block.index, records - 1);
    assert_eq!(last.block.crc, (records - 1) as u32);
    assert_eq!(file.widest.get(), CHUNK_RECORDS * RECORD_LEN);
}

/// The longest header, read in short reads that are interrupted, and a
/// file shorter than the longest header, both read back; a file shorter
/// than any header is not read at all.
#[test]
fn test_headers_of_any_length_are_read() {
    let longest = Raw {
        validator: vec![b'v'; MAX_VALIDATOR_LEN],
        source: Some(vec![b's'; MAX_SOURCE_LEN]),
        ..Raw::default()
    }
    .bytes();
    let len = longest.len() as u64 + 3 * RECORD_LEN as u64;
    let mut longest_file = Counted::new(longest, |n| rec(n, 10 + n as u32), len);
    longest_file.interrupt = true;
    let scanned = read_state(&longest_file).unwrap();
    assert_eq!(scanned.header.len, MAX_HEADER_LEN);
    assert_eq!(
        scanned
            .records
            .iter()
            .map(|record| record.block)
            .collect::<Vec<_>>(),
        [block(0, 10), block(1, 11), block(2, 12)]
    );
    // One header, two records and a torn one: shorter than the longest
    // header.
    let mut short = file(&[rec(0, 10), rec(1, 20)]);
    short.extend_from_slice(&rec(2, 30)[..5]);
    assert!((short.len() as u64) < MAX_HEADER_LEN);
    let scanned = read_state(&short.as_slice()).unwrap();
    assert_eq!(scanned.records, [kept(0, 0, 10), kept(1, 1, 20)]);
    assert_eq!(
        scanned.dropped,
        [dropped(at(2), DropReason::TornTail, None)]
    );
    for len in [0, MIN_HEADER_LEN - 1] {
        let tiny = Counted::repeating(header(), rec(0, 10), len);
        assert!(matches!(
            read_state(&tiny),
            Err(ReadError::Rejected(RejectReason::BadHeader))
        ));
        assert_eq!(tiny.read.get(), 0, "{len}");
    }
}

/// A bad header is rejected after the longest header was read, not the
/// whole file.
#[test]
fn test_a_bad_header_is_rejected_from_its_start() {
    let bad = Counted::repeating(b"NOTSTATE".to_vec(), [0; RECORD_LEN], 1 << 20);
    assert!(matches!(
        read_state(&bad),
        Err(ReadError::Rejected(RejectReason::BadHeader))
    ));
    assert_eq!(bad.read.get(), MAX_HEADER_LEN);
}

/// A file read through the destination's real locked handle, which is the
/// only way on Windows: the lock forbids any other handle (design
/// §11.5.4). A file of one header and records past one read.
#[test]
fn test_the_state_is_read_through_the_locked_handle() {
    use crate::task::instance::{
        destination_lock::{DestinationLock, LockHooks, Unsupported, state_path_for},
        state_file::{LockedFile, StateFile},
    };

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("file.bin");
    let lock = DestinationLock::acquire(&dest, Unsupported::Fail, &LockHooks::default()).unwrap();
    let handle = lock.handle().expect("locked");
    let total = 4000 * BS;
    let header = encode_header(&Header {
        block_size: BS as u32,
        total,
        validator: &etag(),
        source: Some("src"),
    })
    .unwrap();
    let mut state = StateFile::new(Box::new(LockedFile(handle.clone())));
    state.write_header(&header).unwrap();
    let blocks: Vec<_> = (0..3000)
        .map(|index| CheckpointBlock {
            index,
            len: BS as u32,
            crc: index as u32,
        })
        .collect();
    state.append(&blocks).unwrap();
    let scanned = read_state(&*handle).unwrap();
    assert_eq!(scanned.header.total, total);
    assert_eq!(scanned.header.source.as_deref(), Some("src"));
    assert_eq!(scanned.records.len(), 3000);
    assert_eq!(scanned.records[2999].block, blocks[2999]);
    assert_eq!(scanned.dropped, []);
    assert!(state_path_for(&dest).exists());
    drop(state);
    drop(handle);
    lock.release();
}

/// A file that cannot be read is an error, not a rejection.
#[test]
fn test_a_failed_read_is_an_error() {
    struct Failing;

    impl ReadAt for Failing {
        fn len(&self) -> io::Result<u64> {
            Ok(100)
        }

        fn read_at(&self, _: &mut [u8], _: u64) -> io::Result<usize> {
            Err(io::Error::other("bad sector"))
        }
    }

    let Err(ReadError::Io(source)) = read_state(&Failing) else {
        panic!("not an io error");
    };
    assert_eq!(source.to_string(), "bad sector");
}

/// A file of `records`, which name blocks of a download of 6000 blocks,
/// served from a [`Counted`] reader.
fn chunked(records: Vec<[u8; RECORD_LEN]>, torn: usize) -> (Counted, u64) {
    let head = Raw {
        total: 6000 * BS,
        ..Raw::default()
    }
    .bytes();
    let header_len = head.len() as u64;
    let len = header_len + (records.len() * RECORD_LEN + torn) as u64;
    let records = std::rc::Rc::new(records);
    let file = Counted::new(
        head,
        move |n| {
            records
                .get(n as usize)
                .copied()
                .unwrap_or([0xAB; RECORD_LEN])
        },
        len,
    );
    (file, header_len)
}

/// A record of block `index`, of 6000 whole blocks.
fn whole(index: u64, crc: u32) -> [u8; RECORD_LEN] {
    encode_record(&CheckpointBlock {
        index,
        len: BS as u32,
        crc,
    })
}

/// Repeats and conflicts whose records lie on both sides of the edge of
/// the first chunk are judged as within one: the first of a block is kept,
/// its repeats dropped silently, and a conflict reports every record from
/// the first on, in file order.
#[test]
fn test_records_across_a_chunk_edge() {
    const X: u64 = 5000;
    let edge = CHUNK_RECORDS as u64;
    let (a, b) = (whole(X, 1), whole(X, 2));
    // The records at edge - 1, edge and edge + 1, and which conflict.
    type Case = (&'static str, [[u8; RECORD_LEN]; 3], &'static [u64]);
    let cases: [Case; 5] = [
        ("A,A,other", [a, a, whole(X + 1, 9)], &[]),
        ("A,B,other", [a, b, whole(X + 1, 9)], &[0, 1]),
        ("A,A,B", [a, a, b], &[0, 2]),
        ("B,A,A", [b, a, a], &[0, 1, 2]),
        ("A,B,A", [a, b, a], &[0, 1, 2]),
    ];
    for (case, around, conflicts) in cases {
        let mut records: Vec<_> = (0..edge - 1).map(|n| whole(n, n as u32)).collect();
        records.extend(around);
        let (file, header_len) = chunked(records, 0);
        let at = |n: u64| header_len + n * RECORD_LEN as u64;
        let scanned = read_state(&file).unwrap();
        assert_eq!(
            scanned
                .dropped
                .iter()
                .map(|drop| (drop.position, drop.reason))
                .collect::<Vec<_>>(),
            conflicts
                .iter()
                .map(|&n| (at(edge - 1 + n), DropReason::Conflict))
                .collect::<Vec<_>>(),
            "{case}"
        );
        let x = scanned
            .records
            .iter()
            .find(|record| record.block.index == X);
        match conflicts {
            [] => assert_eq!(
                x.map(|record| record.position),
                Some(at(edge - 1)),
                "{case}"
            ),
            _ => assert_eq!(x, None, "{case}"),
        }
        // Every other block is kept where it is.
        for record in &scanned.records {
            if record.block.index < edge - 1 {
                assert_eq!(record.position, at(record.block.index), "{case}");
            }
        }
    }
}

/// A block whose records are many and spread over the file keeps the
/// first of them, and a conflict reports exactly the first and the
/// conflicting one, however a sort may move equal blocks.
#[test]
fn test_many_records_of_a_block_keep_file_order() {
    const X: u64 = 5999;
    let mut records = Vec::new();
    for n in 0..3000 {
        records.push(whole(n, n as u32));
        records.push(whole(X, 7));
    }
    let (file, header_len) = chunked(records.clone(), 0);
    let at = |n: u64| header_len + n * RECORD_LEN as u64;
    let scanned = read_state(&file).unwrap();
    assert_eq!(scanned.dropped, []);
    let x = scanned.records.iter().find(|r| r.block.index == X).unwrap();
    assert_eq!(x.position, at(1));
    records.push(whole(X, 8));
    let last = records.len() as u64 - 1;
    let (file, _) = chunked(records, 0);
    let scanned = read_state(&file).unwrap();
    assert_eq!(
        scanned
            .dropped
            .iter()
            .map(|drop| drop.position)
            .collect::<Vec<_>>(),
        [at(1), at(last)]
    );
    assert!(scanned.records.iter().all(|r| r.block.index != X));
}

/// A torn tail right after a full chunk is cut off at the chunk's end,
/// whatever its length.
#[test]
fn test_a_torn_tail_after_a_full_chunk() {
    let edge = CHUNK_RECORDS as u64;
    for torn in 1..RECORD_LEN {
        let records: Vec<_> = (0..edge).map(|n| whole(n, n as u32)).collect();
        let (file, header_len) = chunked(records, torn);
        let scanned = read_state(&file).unwrap();
        assert_eq!(scanned.records.len() as u64, edge, "{torn}");
        assert_eq!(
            scanned.records.last().unwrap().position,
            header_len + (edge - 1) * RECORD_LEN as u64
        );
        assert_eq!(
            scanned.dropped,
            [DroppedRecord {
                position: header_len + edge * RECORD_LEN as u64,
                reason: DropReason::TornTail,
                range: None,
            }],
            "{torn}"
        );
    }
}
