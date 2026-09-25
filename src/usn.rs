// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! The USN record wire format and the one parser for it.
//!
//! `journal.rs` (Windows only, talks to the volume through
//! `DeviceIoControl`) calls [`parse_usn_records`] on the bytes an
//! `FSCTL_READ_USN_JOURNAL` returns; the unit and property tests call the
//! same function on Linux under the `internals` feature, so what is tested
//! is what runs.
//!
//! The record layouts below are declared here rather than taken from the
//! `windows` crate so this module builds without it. Layout tests
//! (`src/tests/usn/layout.rs`, Windows only) pin every field offset and
//! struct size to the `windows` crate's `USN_RECORD_*` types, so drift
//! fails a test instead of misparsing.

use std::ffi::OsString;
use std::mem::size_of;

use time::OffsetDateTime;

use crate::api::{ntfs_to_unix_time, utf16_to_os_string, FileId};
use crate::errors::{NtfsReaderError, NtfsReaderResult};

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct CommonHeader {
    pub record_length: u32,
    pub major_version: u16,
    pub minor_version: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct RecordV2 {
    pub record_length: u32,
    pub major_version: u16,
    pub minor_version: u16,
    pub file_reference_number: u64,
    pub parent_file_reference_number: u64,
    pub usn: i64,
    pub time_stamp: i64,
    pub reason: u32,
    pub source_info: u32,
    pub security_id: u32,
    pub file_attributes: u32,
    pub file_name_length: u16,
    pub file_name_offset: u16,
    pub file_name: [u16; 1],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct FileId128 {
    pub identifier: [u8; 16],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct RecordV3 {
    pub record_length: u32,
    pub major_version: u16,
    pub minor_version: u16,
    pub file_reference_number: FileId128,
    pub parent_file_reference_number: FileId128,
    pub usn: i64,
    pub time_stamp: i64,
    pub reason: u32,
    pub source_info: u32,
    pub security_id: u32,
    pub file_attributes: u32,
    pub file_name_length: u16,
    pub file_name_offset: u16,
    pub file_name: [u16; 1],
}

/// A set of USN change reasons (the `USN_REASON_*` flags).
///
/// Combine them with `|`; test a record's reason with [`Reason::contains`]
/// (every flag asked for is set) or [`Reason::intersects`] (at least one is).
///
/// ```no_run
/// use ntfs_reader::Reason;
///
/// let wanted = Reason::FILE_CREATE | Reason::FILE_DELETE;
/// let reason = Reason::FILE_CREATE | Reason::CLOSE;
/// assert!(reason.intersects(wanted));
/// assert!(!reason.contains(wanted));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Reason(u32);

impl Reason {
    /// No reason at all.
    pub const EMPTY: Reason = Reason(0);
    /// Every reason, including bits this crate does not name. Ask for this to see all changes.
    pub const ALL: Reason = Reason(0xFFFF_FFFF);

    /// The data in the default stream was overwritten.
    pub const DATA_OVERWRITE: Reason = Reason(0x0000_0001);
    /// The default stream grew.
    pub const DATA_EXTEND: Reason = Reason(0x0000_0002);
    /// The default stream was truncated.
    pub const DATA_TRUNCATION: Reason = Reason(0x0000_0004);
    /// The data in a named stream was overwritten.
    pub const NAMED_DATA_OVERWRITE: Reason = Reason(0x0000_0010);
    /// A named stream grew.
    pub const NAMED_DATA_EXTEND: Reason = Reason(0x0000_0020);
    /// A named stream was truncated.
    pub const NAMED_DATA_TRUNCATION: Reason = Reason(0x0000_0040);
    /// The file or directory was created.
    pub const FILE_CREATE: Reason = Reason(0x0000_0100);
    /// The file or directory was deleted.
    pub const FILE_DELETE: Reason = Reason(0x0000_0200);
    /// The extended attributes changed.
    pub const EA_CHANGE: Reason = Reason(0x0000_0400);
    /// The access rights changed.
    pub const SECURITY_CHANGE: Reason = Reason(0x0000_0800);
    /// The record carries the name the file had before a rename.
    pub const RENAME_OLD_NAME: Reason = Reason(0x0000_1000);
    /// The record carries the name the file has after a rename.
    pub const RENAME_NEW_NAME: Reason = Reason(0x0000_2000);
    /// The "content indexed" attribute changed.
    pub const INDEXABLE_CHANGE: Reason = Reason(0x0000_4000);
    /// Attributes or timestamps changed.
    pub const BASIC_INFO_CHANGE: Reason = Reason(0x0000_8000);
    /// A hard link was added or removed.
    pub const HARD_LINK_CHANGE: Reason = Reason(0x0001_0000);
    /// The compression state changed.
    pub const COMPRESSION_CHANGE: Reason = Reason(0x0002_0000);
    /// The encryption state changed.
    pub const ENCRYPTION_CHANGE: Reason = Reason(0x0004_0000);
    /// The object id changed.
    pub const OBJECT_ID_CHANGE: Reason = Reason(0x0008_0000);
    /// The reparse point changed.
    pub const REPARSE_POINT_CHANGE: Reason = Reason(0x0010_0000);
    /// A stream was added, removed or renamed.
    pub const STREAM_CHANGE: Reason = Reason(0x0020_0000);
    /// A transacted change.
    pub const TRANSACTED_CHANGE: Reason = Reason(0x0040_0000);
    /// The integrity setting of a file or directory changed.
    pub const INTEGRITY_CHANGE: Reason = Reason(0x0080_0000);
    /// The desired storage class changed.
    pub const DESIRED_STORAGE_CLASS_CHANGE: Reason = Reason(0x0100_0000);
    /// The file or directory was closed, which ends a batch of changes to it.
    pub const CLOSE: Reason = Reason(0x8000_0000);

    /// The raw `USN_REASON_*` bits.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// From raw `USN_REASON_*` bits, including any this crate does not name.
    pub const fn from_bits(bits: u32) -> Reason {
        Reason(bits)
    }

    /// `true` if every flag in `other` is set here. An empty `other` is always contained.
    pub const fn contains(self, other: Reason) -> bool {
        self.0 & other.0 == other.0
    }

    /// `true` if at least one flag in `other` is set here.
    pub const fn intersects(self, other: Reason) -> bool {
        self.0 & other.0 != 0
    }
}

impl std::ops::BitOr for Reason {
    type Output = Reason;

    fn bitor(self, rhs: Reason) -> Reason {
        Reason(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for Reason {
    fn bitor_assign(&mut self, rhs: Reason) {
        self.0 |= rhs.0;
    }
}

impl std::fmt::Display for Reason {
    /// Names of the set flags, alphabetical, space-separated
    /// (`CLOSE FILE_CREATE`); empty for none.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const FLAGS: &[(Reason, &str)] = &[
            (Reason::BASIC_INFO_CHANGE, "BASIC_INFO_CHANGE"),
            (Reason::CLOSE, "CLOSE"),
            (Reason::COMPRESSION_CHANGE, "COMPRESSION_CHANGE"),
            (Reason::DATA_EXTEND, "DATA_EXTEND"),
            (Reason::DATA_OVERWRITE, "DATA_OVERWRITE"),
            (Reason::DATA_TRUNCATION, "DATA_TRUNCATION"),
            (
                Reason::DESIRED_STORAGE_CLASS_CHANGE,
                "DESIRED_STORAGE_CLASS_CHANGE",
            ),
            (Reason::EA_CHANGE, "EA_CHANGE"),
            (Reason::ENCRYPTION_CHANGE, "ENCRYPTION_CHANGE"),
            (Reason::FILE_CREATE, "FILE_CREATE"),
            (Reason::FILE_DELETE, "FILE_DELETE"),
            (Reason::HARD_LINK_CHANGE, "HARD_LINK_CHANGE"),
            (Reason::INDEXABLE_CHANGE, "INDEXABLE_CHANGE"),
            (Reason::INTEGRITY_CHANGE, "INTEGRITY_CHANGE"),
            (Reason::NAMED_DATA_EXTEND, "NAMED_DATA_EXTEND"),
            (Reason::NAMED_DATA_OVERWRITE, "NAMED_DATA_OVERWRITE"),
            (Reason::NAMED_DATA_TRUNCATION, "NAMED_DATA_TRUNCATION"),
            (Reason::OBJECT_ID_CHANGE, "OBJECT_ID_CHANGE"),
            (Reason::RENAME_NEW_NAME, "RENAME_NEW_NAME"),
            (Reason::RENAME_OLD_NAME, "RENAME_OLD_NAME"),
            (Reason::REPARSE_POINT_CHANGE, "REPARSE_POINT_CHANGE"),
            (Reason::SECURITY_CHANGE, "SECURITY_CHANGE"),
            (Reason::STREAM_CHANGE, "STREAM_CHANGE"),
            (Reason::TRANSACTED_CHANGE, "TRANSACTED_CHANGE"),
        ];

        let mut first = true;
        for (flag, name) in FLAGS {
            if self.intersects(*flag) {
                if !first {
                    write!(f, " ")?;
                }
                write!(f, "{name}")?;
                first = false;
            }
        }

        Ok(())
    }
}

/// One journal record. `name` is the file name only (the record's
/// `FileName`); resolving a full path is a separate, explicit step
/// ([`Journal::resolve_path`](crate::Journal::resolve_path)), since it
/// costs handle opens most callers reading a large journal skip.
///
/// `name` is an `OsString`, not a `String`: a file name is UTF-16 with no
/// guarantee of validity (an unpaired surrogate is a real name), and the
/// lossless value compares equal to the name the file has in the MFT.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UsnRecord {
    /// The record's position in the journal.
    pub usn: i64,
    /// When the change happened. A time before 1970 is the real date. The
    /// kernel never writes a negative time or one past year 9999, but a
    /// clamp guards both: negative reads as 1601-01-01, later as the last
    /// instant `OffsetDateTime` holds.
    pub timestamp: OffsetDateTime,
    /// The file the change belongs to.
    pub file_id: FileId,
    /// The directory that held the file when the change happened.
    pub parent_id: FileId,
    /// What changed. A record can carry several reasons, and NTFS writes
    /// more than one record per operation (a rename writes old-name and
    /// new-name records).
    pub reason: Reason,
    /// The file's attributes when the change happened: `FILE_ATTRIBUTE_*`
    /// flags, as [`FileInfo::file_attributes`](crate::FileInfo::file_attributes)
    /// has them. After `FILE_DELETE` this tells a deleted directory
    /// (`FILE_ATTRIBUTE_DIRECTORY`, 0x10) from a deleted file.
    pub file_attributes: u32,
    /// The file's name, without its directory.
    pub name: OsString,
}

/// USN timestamps use the same 1601-epoch, 100ns-interval representation as
/// `$STANDARD_INFORMATION`. The kernel never produces a negative one, but a
/// corrupt or synthetic one reads as 0 (1601-01-01); `ntfs_to_unix_time`
/// clamps the far end.
fn usn_record_time(timestamp: i64) -> OffsetDateTime {
    ntfs_to_unix_time(timestamp.max(0) as u64)
}

/// Copies a `T` out of the start of `bytes`, or `None` if shorter than `T`.
/// Works at any alignment (`read_unaligned`).
fn read_struct<T: Copy>(bytes: &[u8]) -> Option<T> {
    if bytes.len() < size_of::<T>() {
        return None;
    }
    // SAFETY: the length is checked above, and every `T` used here is a
    // `repr(C)` struct of plain integers and integer arrays, for which any
    // bit pattern is valid.
    Some(unsafe { (bytes.as_ptr() as *const T).read_unaligned() })
}

/// Decodes `record_bytes[name_offset..name_offset + name_length]` as a
/// UTF-16LE name, bounds checked against `record_len` (the record's own
/// declared, already-validated extent). No pointer casts, so it cannot
/// read past `record_bytes`.
fn slice_usn_name(
    record_bytes: &[u8],
    record_len: usize,
    name_offset: u16,
    name_length: u16,
) -> NtfsReaderResult<OsString> {
    let start = name_offset as usize;
    let end = start
        .checked_add(name_length as usize)
        .ok_or(NtfsReaderError::InvalidUsnRecord {
            details: "FileNameOffset + FileNameLength overflows",
        })?;

    if end > record_len || end > record_bytes.len() {
        return Err(NtfsReaderError::InvalidUsnRecord {
            details: "FileNameOffset/FileNameLength exceeds the record's own RecordLength",
        });
    }

    let units: Vec<u16> = record_bytes[start..end]
        .as_chunks::<2>()
        .0
        .iter()
        .copied()
        .map(u16::from_le_bytes)
        .collect();
    Ok(utf16_to_os_string(&units))
}

/// Parses the records in `data`, the bytes an `FSCTL_READ_USN_JOURNAL` call
/// returned: the next USN (8 bytes), then the records.
///
/// Every header is copied out with `read_unaligned` rather than referenced
/// in place. A record's `RecordLength` is validated against the bytes
/// available, against being a multiple of 8 (real USN records always are;
/// otherwise the buffer is corrupt with no reliable way to find the next
/// record), and against its version's minimum size, before any field past
/// the common header is trusted. Corruption anywhere aborts the whole read
/// with an error rather than silently misreading the rest of the batch. A
/// record of a major version other than 2 or 3 is skipped.
pub(crate) fn parse_usn_records(data: &[u8]) -> NtfsReaderResult<Vec<UsnRecord>> {
    let mut results = Vec::new();

    let mut offset = size_of::<i64>(); // the leading next USN
    while offset < data.len() {
        let record_bytes = &data[offset..];
        let Some(header) = read_struct::<CommonHeader>(record_bytes) else {
            break;
        };

        if header.record_length == 0 {
            break;
        }
        let record_len = header.record_length as usize;
        if record_len > record_bytes.len() {
            return Err(NtfsReaderError::InvalidUsnRecord {
                details: "RecordLength exceeds the bytes actually returned",
            });
        }
        if !record_len.is_multiple_of(8) {
            return Err(NtfsReaderError::InvalidUsnRecord {
                details: "RecordLength is not a multiple of 8",
            });
        }

        let record = match header.major_version {
            2 => {
                let rec = read_struct::<RecordV2>(&record_bytes[..record_len]).ok_or(
                    NtfsReaderError::InvalidUsnRecord {
                        details: "V2 record shorter than USN_RECORD_V2",
                    },
                )?;
                Some(UsnRecord {
                    usn: rec.usn,
                    timestamp: usn_record_time(rec.time_stamp),
                    file_id: FileId::from(rec.file_reference_number),
                    parent_id: FileId::from(rec.parent_file_reference_number),
                    reason: Reason(rec.reason),
                    file_attributes: rec.file_attributes,
                    name: slice_usn_name(
                        record_bytes,
                        record_len,
                        rec.file_name_offset,
                        rec.file_name_length,
                    )?,
                })
            }
            3 => {
                let rec = read_struct::<RecordV3>(&record_bytes[..record_len]).ok_or(
                    NtfsReaderError::InvalidUsnRecord {
                        details: "V3 record shorter than USN_RECORD_V3",
                    },
                )?;
                Some(UsnRecord {
                    usn: rec.usn,
                    timestamp: usn_record_time(rec.time_stamp),
                    file_id: FileId::from_le_bytes(rec.file_reference_number.identifier),
                    parent_id: FileId::from_le_bytes(rec.parent_file_reference_number.identifier),
                    reason: Reason(rec.reason),
                    file_attributes: rec.file_attributes,
                    name: slice_usn_name(
                        record_bytes,
                        record_len,
                        rec.file_name_offset,
                        rec.file_name_length,
                    )?,
                })
            }
            _ => None,
        };

        if let Some(record) = record {
            results.push(record);
        }

        offset += record_len;
    }

    Ok(results)
}

#[cfg(test)]
#[path = "tests/usn/mod.rs"]
mod tests;
