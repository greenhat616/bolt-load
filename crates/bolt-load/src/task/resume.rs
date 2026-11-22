//! What a task reports when it resumes from a state file, or why it cannot
//! (design §11.5.10).

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the task resumes in C20b, and they are public in C20c"
    )
)]

use std::{fmt, ops::Range, path::PathBuf};

/// What a task continued from.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResumeReport {
    /// The state file read, `<dest>.boltload`.
    pub state_file: PathBuf,
    /// The ranges continued from, merged.
    pub kept: Vec<Range<u64>>,
    /// The bytes of `kept`.
    pub kept_bytes: u64,
    /// The records dropped, in file order.
    pub dropped: Vec<DroppedRecord>,
}

/// A record of the state file that was not continued from.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DroppedRecord {
    /// The byte offset of the record in the state file.
    pub position: u64,
    pub reason: DropReason,
    /// The block the record names, when it could be read.
    pub range: Option<Range<u64>>,
}

/// Why a record was dropped; its block is downloaded again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DropReason {
    /// Fewer bytes than a record at the end of the file, from a write cut
    /// short; they are cut off.
    TornTail,
    /// The record's checksum, kind or reserved bytes are wrong, or its
    /// length is not its block's.
    BadRecord,
    /// Another valid record names the same block with another checksum.
    Conflict,
    /// The block lies beyond the total.
    OutOfBounds,
    /// The block lies beyond the end of the partial file.
    BeyondFileLength,
    /// The block's bytes in the partial file do not match the record.
    CrcMismatch,
}

impl fmt::Display for DropReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TornTail => "a torn record at the end",
            Self::BadRecord => "a bad record",
            Self::Conflict => "a conflicting record",
            Self::OutOfBounds => "a block beyond the total",
            Self::BeyondFileLength => "a block beyond the partial file",
            Self::CrcMismatch => "a block whose bytes do not match",
        })
    }
}

/// Why a task cannot continue from what an earlier task kept. The task
/// changed neither file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RejectReason {
    /// The state file is there, the partial file is not.
    MissingPartial,
    /// The state file's header is not one this version reads.
    BadHeader,
    /// The state file is longer than any this version writes for its
    /// header; it is not read.
    Oversized,
    /// The resource's size is not the one the state file names.
    TotalMismatch,
    /// The source is not the one the state file names.
    SourceMismatch,
    /// The resource's validator is not the one the state file names: it
    /// changed.
    ValidatorMismatch,
    /// The resource has no validator that tells its versions apart.
    NoValidator,
    /// The resource is served with a content coding.
    ContentCoded,
    /// The resource's size is unknown.
    UnknownSize,
    /// The server does not take range requests.
    RangeUnsupported,
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MissingPartial => "the partial file is missing",
            Self::BadHeader => "the state file's header is bad",
            Self::Oversized => "the state file is too long",
            Self::TotalMismatch => "the size changed",
            Self::SourceMismatch => "the source changed",
            Self::ValidatorMismatch => "the resource changed",
            Self::NoValidator => "the resource has no usable validator",
            Self::ContentCoded => "the resource is content-coded",
            Self::UnknownSize => "the size is unknown",
            Self::RangeUnsupported => "the server does not take ranges",
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// Every reason has its own message.
    #[test]
    fn test_every_reason_has_a_message() {
        let rejects = [
            RejectReason::MissingPartial,
            RejectReason::BadHeader,
            RejectReason::Oversized,
            RejectReason::TotalMismatch,
            RejectReason::SourceMismatch,
            RejectReason::ValidatorMismatch,
            RejectReason::NoValidator,
            RejectReason::ContentCoded,
            RejectReason::UnknownSize,
            RejectReason::RangeUnsupported,
        ];
        let messages: HashSet<_> = rejects.iter().map(ToString::to_string).collect();
        assert_eq!(messages.len(), rejects.len());
        let drops = [
            DropReason::TornTail,
            DropReason::BadRecord,
            DropReason::Conflict,
            DropReason::OutOfBounds,
            DropReason::BeyondFileLength,
            DropReason::CrcMismatch,
        ];
        let messages: HashSet<_> = drops.iter().map(ToString::to_string).collect();
        assert_eq!(messages.len(), drops.len());
    }
}
