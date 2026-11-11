//! The state file of a resumable download, `<dest>.boltload` (design
//! §11.5.5): a header that names the download, then one fixed-size record
//! per block whose data was synced, appended at every checkpoint.
//!
//! It is the destination's lock file too (design §11.5.4), so it is read and
//! written only through the locked handle. This module writes it; [`read`]
//! reads it back.
//!
//! ```text
//! header:
//!   magic        [u8; 8]  = b"BOLTLOAD"
//!   version      u16      = 1
//!   flags        u16      = 0 (reserved)
//!   block_size   u32
//!   total        u64
//!   validator    u8 kind (1 = strong ETag, 2 = Last-Modified) + u16 len + bytes (len <= 1024)
//!   source       u16 len (0xFFFF = none) + bytes (len <= 4096)
//!   header_crc   u32      CRC32C of every header byte before it
//! record (24 bytes, repeated until EOF):
//!   index        u64      block number
//!   len          u32      block length: block_size, or less for the last block
//!   data_crc     u32      CRC32C of the block's bytes in the partial file
//!   kind         u8 = 1 (complete block), [u8; 3] reserved = 0
//!   record_crc   u32      CRC32C of the 20 bytes before it
//! ```
//!
//! Every integer is little-endian.

use std::{fmt, fs::File, io, path::Path, sync::Arc};

use bolt_load_core::adapter::Validator;

use super::concurrent_task::checkpoint::{CheckpointBlock, CheckpointReceiver};

/// The first bytes of every state file.
pub(crate) const MAGIC: [u8; 8] = *b"BOLTLOAD";
/// The version of the format this module writes.
pub(crate) const VERSION: u16 = 1;
/// The longest validator a header holds; a longer one is no usable
/// validator (design §11.5.3).
pub(crate) const MAX_VALIDATOR_LEN: usize = 1024;
/// The longest source a header holds; a longer one fails the builder's
/// validation.
pub(crate) const MAX_SOURCE_LEN: usize = 4096;
/// The length of a record.
pub(crate) const RECORD_LEN: usize = 24;
/// The source length that means no source.
const NO_SOURCE: u16 = 0xFFFF;
/// The kind of a record of a complete block.
const COMPLETE_BLOCK: u8 = 1;

/// What a header names.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Header<'a> {
    pub(crate) block_size: u32,
    pub(crate) total: u64,
    pub(crate) validator: &'a Validator,
    pub(crate) source: Option<&'a str>,
}

/// Why a header cannot be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeaderError {
    /// The validator is longer than [`MAX_VALIDATOR_LEN`].
    ValidatorTooLong,
    /// The source is longer than [`MAX_SOURCE_LEN`].
    SourceTooLong,
}

impl fmt::Display for HeaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ValidatorTooLong => {
                write!(f, "the validator is longer than {MAX_VALIDATOR_LEN} bytes")
            }
            Self::SourceTooLong => write!(f, "the source is longer than {MAX_SOURCE_LEN} bytes"),
        }
    }
}

/// The kind byte of `validator`.
fn validator_kind(validator: &Validator) -> u8 {
    match validator {
        Validator::StrongEtag(_) => 1,
        Validator::LastModified(_) => 2,
    }
}

/// The bytes of `header`, its CRC included.
pub(crate) fn encode_header(header: &Header<'_>) -> Result<Vec<u8>, HeaderError> {
    let validator = header.validator.as_str().as_bytes();
    if validator.len() > MAX_VALIDATOR_LEN {
        return Err(HeaderError::ValidatorTooLong);
    }
    let source = header.source.map(str::as_bytes);
    if source.is_some_and(|source| source.len() > MAX_SOURCE_LEN) {
        return Err(HeaderError::SourceTooLong);
    }
    let mut bytes = Vec::with_capacity(
        8 + 2 + 2 + 4 + 8 + 3 + validator.len() + 2 + source.map_or(0, <[u8]>::len) + 4,
    );
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&header.block_size.to_le_bytes());
    bytes.extend_from_slice(&header.total.to_le_bytes());
    bytes.push(validator_kind(header.validator));
    bytes.extend_from_slice(&(validator.len() as u16).to_le_bytes());
    bytes.extend_from_slice(validator);
    match source {
        Some(source) => {
            bytes.extend_from_slice(&(source.len() as u16).to_le_bytes());
            bytes.extend_from_slice(source);
        }
        None => bytes.extend_from_slice(&NO_SOURCE.to_le_bytes()),
    }
    let crc = crc32c::crc32c(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
    Ok(bytes)
}

/// The record of `block`.
pub(crate) fn encode_record(block: &CheckpointBlock) -> [u8; RECORD_LEN] {
    let mut record = [0u8; RECORD_LEN];
    record[0..8].copy_from_slice(&block.index.to_le_bytes());
    record[8..12].copy_from_slice(&block.len.to_le_bytes());
    record[12..16].copy_from_slice(&block.crc.to_le_bytes());
    record[16] = COMPLETE_BLOCK;
    let crc = crc32c::crc32c(&record[..20]);
    record[20..24].copy_from_slice(&crc.to_le_bytes());
    record
}

/// The file operations the state file needs; tests inject failures.
pub(crate) trait StateIo: Send + 'static {
    /// Write all of `bytes` at `offset`.
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()>;
    /// Cut or extend the file to `len` bytes.
    fn set_len(&mut self, len: u64) -> io::Result<()>;
    /// Sync the file's data and size to the disk.
    fn sync(&mut self) -> io::Result<()>;
}

/// The locked handle of the destination's lock file.
pub(crate) struct LockedFile(pub(crate) Arc<File>);

impl StateIo for LockedFile {
    fn write_at(&mut self, mut offset: u64, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            match write_at(&self.0, bytes, offset) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write the whole state record",
                    ));
                }
                Ok(written) => {
                    bytes = &bytes[written..];
                    offset += written as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.0.sync_all()
    }
}

#[cfg(unix)]
fn write_at(file: &File, bytes: &[u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(file, bytes, offset)
}

#[cfg(windows)]
fn write_at(file: &File, bytes: &[u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(file, bytes, offset)
}

/// The state file of one attempt, written through `io`.
///
/// `end` is where the next records go: the end of the header and of the
/// records of the last checkpoint that succeeded (design §11.5.5). It moves
/// only once what was written there is synced.
pub(crate) struct StateFile {
    io: Box<dyn StateIo>,
    end: u64,
}

impl StateFile {
    /// A state file written through `io`, empty as far as it knows.
    pub(crate) fn new(io: Box<dyn StateIo>) -> Self {
        Self { io, end: 0 }
    }

    /// Where the next records go.
    #[cfg(test)]
    pub(crate) fn end(&self) -> u64 {
        self.end
    }

    /// Write `header` at the start of the file, which the task emptied
    /// before it touched the partial file, and sync it (design §11.5.5,
    /// step 3 of a resumable new start). Records go after it.
    ///
    /// # Errors
    ///
    /// The write or the sync failed; the file may hold part of the header,
    /// which no reader takes for a valid one.
    pub(crate) fn write_header(&mut self, header: &[u8]) -> io::Result<()> {
        self.io.write_at(0, header)?;
        self.io.sync()?;
        self.end = header.len() as u64;
        Ok(())
    }

    /// Append the records of `blocks` at the end and sync them; the end
    /// moves past them only then.
    ///
    /// # Errors
    ///
    /// The write or the sync failed: [`Self::roll_back`] cuts what it may
    /// have left.
    pub(crate) fn append(&mut self, blocks: &[CheckpointBlock]) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(blocks.len() * RECORD_LEN);
        for block in blocks {
            bytes.extend_from_slice(&encode_record(block));
        }
        self.io.write_at(self.end, &bytes)?;
        self.io.sync()?;
        self.end += bytes.len() as u64;
        Ok(())
    }

    /// Replace the records after the header of `header_len` bytes with
    /// `records`, in their order, before a resume touches the partial file
    /// (design §11.5.5, "续传"): cut the file to the header and sync, so
    /// the old records are gone for sure; then write the records, at most
    /// [`CHUNK_RECORDS`](read::CHUNK_RECORDS) at a time through one buffer,
    /// and sync. Records go after them.
    ///
    /// # Errors
    ///
    /// A step failed. The file holds the header and a prefix of
    /// `records`, or the old records, when the cut was not taken.
    #[cfg_attr(not(test), expect(dead_code, reason = "the task resumes in C20b"))]
    pub(crate) fn rewrite(
        &mut self,
        header_len: u64,
        records: impl IntoIterator<Item = CheckpointBlock>,
    ) -> io::Result<()> {
        self.io.set_len(header_len)?;
        self.io.sync()?;
        self.end = header_len;
        let mut records = records.into_iter().peekable();
        if records.peek().is_none() {
            return Ok(());
        }
        let mut batch = Vec::with_capacity(read::CHUNK_RECORDS * RECORD_LEN);
        let mut at = header_len;
        while records.peek().is_some() {
            batch.clear();
            for block in records.by_ref().take(read::CHUNK_RECORDS) {
                batch.extend_from_slice(&encode_record(&block));
            }
            self.io.write_at(at, &batch)?;
            at += batch.len() as u64;
        }
        self.io.sync()?;
        self.end = at;
        Ok(())
    }

    /// Cut the file back to the end of the last checkpoint that succeeded,
    /// and sync.
    ///
    /// # Errors
    ///
    /// The cut or its sync failed: the task takes no more checkpoints. The
    /// file is still valid up to the end; past it, a torn or complete
    /// record of blocks whose data was synced (design §11.5.5).
    pub(crate) fn roll_back(&mut self) -> io::Result<()> {
        self.io.set_len(self.end)?;
        self.io.sync()
    }

    /// Empty the file and sync it: no state any more (design §11.5.5,
    /// "作废").
    ///
    /// # Errors
    ///
    /// The cut or its sync failed.
    ///
    /// The task invalidates through [`DestinationLock::invalidate`](super::destination_lock::DestinationLock::invalidate)
    /// instead, which knows when it went on unlocked; this one is the
    /// writer's own, for its tests.
    #[cfg(test)]
    pub(crate) fn invalidate(&mut self) -> io::Result<()> {
        self.io.set_len(0)?;
        self.io.sync()?;
        self.end = 0;
        Ok(())
    }
}

impl CheckpointReceiver for StateFile {
    fn record(&mut self, blocks: &[CheckpointBlock]) -> io::Result<()> {
        self.append(blocks)
    }

    fn roll_back(&mut self) -> io::Result<()> {
        StateFile::roll_back(self)
    }
}

/// Sync the directory that holds `path`, so that a new, renamed or removed
/// entry in it survives a power loss (design §11.5.5).
///
/// Windows has no directory sync; NTFS journals the metadata. This does
/// nothing there, a known limitation.
///
/// # Errors
///
/// The directory could not be opened or synced.
pub(crate) fn sync_parent_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let parent = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        File::open(parent)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

pub(crate) mod read;
#[cfg(test)]
pub(crate) mod tests;
