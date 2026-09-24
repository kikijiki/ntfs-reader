// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! The USN record wire format and the one parser for it.
//!
//! `journal.rs` (Windows only, it talks to the volume through `DeviceIoControl`) calls
//! [`parse_usn_records`] on the bytes an `FSCTL_READ_USN_JOURNAL` returns, and the unit and
//! property tests call the same function on Linux under the `internals` feature, so what is
//! tested is what runs.
//! The record layouts below are declared here instead of taken from the `windows` crate so this
//! module builds without it; [`layout_tests`] (Windows only) pin every field offset and struct
//! size to the `windows` crate's `USN_RECORD_*` types, so a layout drift fails a test instead of
//! misparsing.

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
/// Combine them with `|`; test a record's reason with [`Reason::contains`] (every flag asked for
/// is set) or [`Reason::intersects`] (at least one is).
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
    /// The names of the set flags in alphabetical order, separated by spaces (`CLOSE FILE_CREATE`);
    /// empty for none.
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

/// One journal record. `name` is the file name only (the record's `FileName`); resolving it to a
/// full path is a separate, explicit step ([`Journal::resolve_path`](crate::Journal::resolve_path))
/// since it costs one or two handle opens and most callers reading a large journal don't need it
/// for every record.
///
/// `name` is an `OsString`, not a `String`: a file name is UTF-16 with no guarantee of being
/// valid (an unpaired surrogate is a real name), and the lossless value is what compares equal
/// to the name the file has in the MFT.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UsnRecord {
    /// The record's position in the journal.
    pub usn: i64,
    /// When the change happened. A time before 1970 is the real date. The kernel writes neither a
    /// negative time nor one past the year 9999, so those are clamped instead of trusted: a negative
    /// one reads as 1601-01-01, and a later one as the last instant `OffsetDateTime` holds.
    pub timestamp: OffsetDateTime,
    /// The file the change belongs to.
    pub file_id: FileId,
    /// The directory that held the file when the change happened.
    pub parent_id: FileId,
    /// What changed. A record can carry several reasons, and NTFS writes more than one record
    /// per operation (a rename writes an old-name and a new-name record).
    pub reason: Reason,
    /// The file's attributes when the change happened: `FILE_ATTRIBUTE_*` flags, as
    /// [`FileInfo::file_attributes`](crate::FileInfo::file_attributes) has them. After a
    /// `FILE_DELETE` this is what tells a deleted directory
    /// (`FILE_ATTRIBUTE_DIRECTORY`, 0x10) from a deleted file.
    pub file_attributes: u32,
    /// The file's name, without its directory.
    pub name: OsString,
}

/// USN timestamps use the same 1601-epoch, 100ns-interval representation as
/// `$STANDARD_INFORMATION`. The kernel does not produce negative ones, but a corrupt or
/// synthetic one is read as 0 (1601-01-01) rather than trusted; `ntfs_to_unix_time` clamps the
/// far end.
fn usn_record_time(timestamp: i64) -> OffsetDateTime {
    ntfs_to_unix_time(timestamp.max(0) as u64)
}

/// Copy a `T` out of the start of `bytes`, or `None` if `bytes` is shorter than `T`. Works at any
/// alignment (`read_unaligned`), so it does not depend on where the buffer happens to start.
fn read_struct<T: Copy>(bytes: &[u8]) -> Option<T> {
    if bytes.len() < size_of::<T>() {
        return None;
    }
    // SAFETY: the length is checked above, and every `T` used here is a `repr(C)` struct of plain
    // integers and integer arrays, for which any bit pattern is a valid value.
    Some(unsafe { (bytes.as_ptr() as *const T).read_unaligned() })
}

/// Decode `record_bytes[name_offset..name_offset + name_length]` as a UTF-16LE name, bounds
/// checked against `record_len` (the record's own declared, already-validated extent). Works on
/// raw bytes with no pointer casts, so it cannot read past `record_bytes`.
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

/// Parse the records in `data`, the bytes an `FSCTL_READ_USN_JOURNAL` call returned: the next USN
/// (8 bytes), then the records.
///
/// Every header is copied out with `read_unaligned` rather than referenced in place, and a
/// record's own `RecordLength` is validated - against the bytes actually available, against a
/// multiple of 8 (real USN records are always emitted that way; a `RecordLength` that isn't means
/// the buffer is corrupt and there is no reliable way to find the start of the next record), and
/// against the minimum size of its declared version - before any field past the common header is
/// trusted. Corruption anywhere aborts the whole read with an error rather than silently dropping
/// or misreading the rest of the batch. A record of a major version other than 2 or 3 is skipped.
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
mod structured_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mft::test_records::assert_os_str_is;

    // Byte offsets in a `USN_RECORD_V2`, written out here instead of taken from `RecordV2`, so a
    // wrong struct cannot make the tests agree with it. The name starts at 60, the size is 64.
    const V2_FILE_REFERENCE: usize = 8;
    const V2_PARENT_REFERENCE: usize = 16;
    const V2_USN: usize = 24;
    const V2_TIME_STAMP: usize = 32;
    const V2_REASON: usize = 40;
    const V2_NAME_LENGTH: usize = 56;
    const V2_NAME_OFFSET: usize = 58;
    const V2_NAME: usize = 60;

    fn put(bytes: &mut [u8], at: usize, value: &[u8]) {
        bytes[at..at + value.len()].copy_from_slice(value);
    }

    /// A 64-byte V2 record with the header fields the parser reads set and everything else zero.
    fn v2_record(record_len: u32, major_version: u16, usn: i64, file_ref: u64) -> Vec<u8> {
        let mut record = vec![0u8; 64];
        put(&mut record, 0, &record_len.to_le_bytes());
        put(&mut record, 4, &major_version.to_le_bytes());
        put(&mut record, V2_FILE_REFERENCE, &file_ref.to_le_bytes());
        put(&mut record, V2_USN, &usn.to_le_bytes());
        put(&mut record, V2_NAME_OFFSET, &(V2_NAME as u16).to_le_bytes());
        record
    }

    /// A V2 record whose name is `name`, laid out the way the kernel does.
    fn v2_with_name(name: &[u16]) -> Vec<u8> {
        let length = (V2_NAME + name.len() * 2).next_multiple_of(8);
        let mut record = v2_record(length as u32, 2, 1, 1);
        record.resize(length, 0);
        put(
            &mut record,
            V2_NAME_LENGTH,
            &((name.len() * 2) as u16).to_le_bytes(),
        );
        for (index, unit) in name.iter().enumerate() {
            put(&mut record, V2_NAME + index * 2, &unit.to_le_bytes());
        }
        record
    }

    /// The bytes an FSCTL returns: the next USN, then `records`.
    fn buffer_of(records: &[Vec<u8>]) -> Vec<u8> {
        let mut buffer = vec![0u8; 8];
        for record in records {
            buffer.extend_from_slice(record);
        }
        buffer
    }

    #[test]
    fn a_v2_record_parses_to_its_fields() {
        // 2023-11-14T22:13:20Z, 1_700_000_000 s after the epoch, as a FILETIME.
        let ticks = 116_444_736_000_000_000u64 + 1_700_000_000 * 10_000_000;
        let name: Vec<u16> = "a.txt".encode_utf16().collect();
        let mut record = v2_with_name(&name);
        put(
            &mut record,
            V2_FILE_REFERENCE,
            &0x0007_0000_0000_0123u64.to_le_bytes(),
        );
        put(
            &mut record,
            V2_PARENT_REFERENCE,
            &0x0001_0000_0000_0005u64.to_le_bytes(),
        );
        put(&mut record, V2_USN, &100i64.to_le_bytes());
        put(&mut record, V2_TIME_STAMP, &ticks.to_le_bytes());
        put(&mut record, V2_REASON, &0x8000_0100u32.to_le_bytes());

        let records = parse_usn_records(&buffer_of(&[record])).expect("valid record");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].usn, 100);
        assert_eq!(
            records[0].timestamp,
            OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
        );
        assert_eq!(records[0].file_id, FileId::from(0x0007_0000_0000_0123u64));
        assert_eq!(records[0].parent_id, FileId::from(0x0001_0000_0000_0005u64));
        assert_eq!(records[0].reason, Reason::FILE_CREATE | Reason::CLOSE);
        assert_eq!(records[0].name, "a.txt");
    }

    // The record's own attributes reach the caller, from a V2 and a V3 record alike: after a
    // `FILE_DELETE` they are all that says whether a directory or a file went away. The
    // neighbouring fields hold other values, so a misplaced offset reads a wrong one.
    #[test]
    fn a_record_carries_its_file_attributes() {
        // FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED.
        const ATTRIBUTES: u32 = 0x0000_2010;

        let mut v2 = v2_record(64, 2, 1, 1);
        put(&mut v2, 44, &0x1111u32.to_le_bytes()); // SourceInfo
        put(&mut v2, 48, &0x2222u32.to_le_bytes()); // SecurityId
        put(&mut v2, 52, &ATTRIBUTES.to_le_bytes());

        let mut v3 = vec![0u8; 80];
        put(&mut v3, 0, &80u32.to_le_bytes());
        put(&mut v3, 4, &3u16.to_le_bytes());
        put(&mut v3, 60, &0x1111u32.to_le_bytes()); // SourceInfo
        put(&mut v3, 64, &0x2222u32.to_le_bytes()); // SecurityId
        put(&mut v3, 68, &ATTRIBUTES.to_le_bytes());

        let records = parse_usn_records(&buffer_of(&[v2, v3])).expect("valid records");

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].file_attributes, ATTRIBUTES, "V2");
        assert_eq!(records[1].file_attributes, ATTRIBUTES, "V3");
    }

    // The same file must compare equal whether a V2 record (64-bit reference) or a V3 record
    // (128-bit id, the reference zero-extended and laid out little-endian) named it.
    #[test]
    fn a_v3_record_id_equals_the_v2_id_of_the_same_file() {
        let reference = 0x0007_0000_0000_0123u64;
        let v2 = v2_record(64, 2, 1, reference);

        let mut v3 = vec![0u8; 80];
        v3[0..4].copy_from_slice(&80u32.to_le_bytes());
        v3[4..6].copy_from_slice(&3u16.to_le_bytes());
        v3[8..16].copy_from_slice(&reference.to_le_bytes()); // low half of FileReferenceNumber
        v3[24..32].copy_from_slice(&5u64.to_le_bytes()); // parent

        let from_v2 = parse_usn_records(&buffer_of(&[v2])).expect("v2");
        let from_v3 = parse_usn_records(&buffer_of(&[v3])).expect("v3");

        assert_eq!(from_v2[0].file_id, from_v3[0].file_id);
        assert_eq!(from_v3[0].file_id, FileId::from(u128::from(reference)));
        assert_eq!(from_v3[0].parent_id, FileId::from(5u64));
    }

    #[test]
    fn a_v3_id_keeps_all_128_bits() {
        let mut v3 = vec![0u8; 80];
        v3[0..4].copy_from_slice(&80u32.to_le_bytes());
        v3[4..6].copy_from_slice(&3u16.to_le_bytes());
        let id: [u8; 16] = std::array::from_fn(|i| i as u8 + 1);
        v3[8..24].copy_from_slice(&id);

        let records = parse_usn_records(&buffer_of(&[v3])).expect("v3");

        assert_eq!(records[0].file_id.as_u128(), u128::from_le_bytes(id));
        assert_eq!(records[0].file_id.as_reference(), None);
    }

    // A USN record's name is a raw UTF-16 sequence with no guarantee of being valid, and the
    // parsed name must be the record's own name, unpaired surrogate included, or it cannot be
    // matched against a name read from the MFT.
    #[test]
    fn a_name_with_an_unpaired_surrogate_is_kept() {
        // "bad", a lone high surrogate, "name".
        let units = [0x62, 0x61, 0x64, 0xD800, 0x6E, 0x61, 0x6D, 0x65];
        let buffer = buffer_of(&[v2_with_name(&units)]);

        let records = parse_usn_records(&buffer).expect("valid record");

        assert_eq!(records.len(), 1);
        // Not a U+FFFD substitute: the lone surrogate is WTF-8's three bytes ED A0 80.
        assert_os_str_is(&records[0].name, &units, b"bad\xED\xA0\x80name");
    }

    #[test]
    fn a_name_length_not_bounded_by_the_record_length_is_rejected() {
        // A record whose real content has a 2-char name, but whose FileNameLength lies and
        // claims 100 chars. Bytes follow it in the buffer (whatever the next record or stale
        // buffer content is); they must not be read as part of the name.
        let mut record = v2_with_name(&"ab".encode_utf16().collect::<Vec<_>>());
        put(&mut record, V2_NAME_LENGTH, &200u16.to_le_bytes());
        let mut buffer = buffer_of(&[record]);
        buffer.resize(512, 0xAA);

        assert!(
            parse_usn_records(&buffer).is_err(),
            "a FileNameLength beyond the record's RecordLength must be rejected"
        );
    }

    // `RecordLength` is validated (bounds, multiple of 8, minimum size for the version) with
    // `read_unaligned` before any field is trusted, so a corrupt one is rejected instead of being
    // misinterpreted.
    #[test]
    fn a_record_length_that_is_not_a_multiple_of_8_is_rejected() {
        // Two correctly laid out 64-byte V2 records back to back, but the first's header lies
        // about its own RecordLength (61). There is no reliable way to know where the next record
        // starts once a record's own length is corrupt, so the whole read is rejected.
        let mut record1 = v2_record(64, 2, 100, 100);
        record1[0..4].copy_from_slice(&61u32.to_le_bytes());
        let record2 = v2_record(64, 2, 200, 200);

        assert!(
            parse_usn_records(&buffer_of(&[record1, record2])).is_err(),
            "a RecordLength that is not a multiple of 8 must be rejected"
        );
    }

    #[test]
    fn a_record_length_past_the_returned_bytes_is_rejected() {
        let record = v2_record(64, 2, 1, 1);
        let buffer = buffer_of(&[record]);

        assert!(parse_usn_records(&buffer[..buffer.len() - 8]).is_err());
    }

    #[test]
    fn a_record_shorter_than_its_version_is_rejected() {
        let mut record = vec![0u8; 16];
        record[0..4].copy_from_slice(&16u32.to_le_bytes());
        record[4..6].copy_from_slice(&2u16.to_le_bytes());

        assert!(parse_usn_records(&buffer_of(&[record])).is_err());
    }

    #[test]
    fn a_zero_record_length_ends_the_parse_without_error() {
        let first = v2_record(64, 2, 1, 1);
        let records = parse_usn_records(&buffer_of(&[first, vec![0u8; 64]])).expect("stops");

        assert_eq!(records.len(), 1);
    }

    #[test]
    fn an_unknown_major_version_is_skipped() {
        let unknown = v2_record(64, 9, 1, 1);
        let known = v2_record(64, 2, 2, 2);

        let records = parse_usn_records(&buffer_of(&[unknown, known])).expect("skips");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].usn, 2);
    }

    #[test]
    fn a_buffer_with_only_the_leading_usn_has_no_records() {
        assert!(parse_usn_records(&[0u8; 8]).expect("empty").is_empty());
        assert!(parse_usn_records(&[]).expect("empty").is_empty());
    }

    #[test]
    fn reason_contains_needs_all_flags_and_intersects_any() {
        let reason = Reason::FILE_CREATE | Reason::CLOSE;

        assert!(reason.contains(Reason::FILE_CREATE));
        assert!(reason.contains(Reason::FILE_CREATE | Reason::CLOSE));
        assert!(!reason.contains(Reason::FILE_CREATE | Reason::FILE_DELETE));
        assert!(reason.intersects(Reason::FILE_CREATE | Reason::FILE_DELETE));
        assert!(!reason.intersects(Reason::FILE_DELETE));
        assert!(reason.contains(Reason::EMPTY));
        assert!(!reason.intersects(Reason::EMPTY));
    }

    // The flags are listed in alphabetical order, not bit order.
    #[test]
    fn reason_display_lists_set_flags_by_name() {
        let reason = Reason::FILE_CREATE | Reason::CLOSE;

        assert_eq!(reason.to_string(), "CLOSE FILE_CREATE");
    }

    // Each row: a `TimeStamp` and the time it must read as, worked out by hand from
    // 1601-01-01 = -11_644_473_600 s (FILETIME 0) and 1970-01-01 = FILETIME 116_444_736_000_000_000.
    #[test]
    fn a_timestamp_is_the_real_date_and_clamps_only_out_of_range() {
        const EPOCH: i64 = 116_444_736_000_000_000;
        // 9999-12-31T23:59:59.9999999Z, the last 100 ns tick `OffsetDateTime` holds.
        const LAST_TICK: i64 = 2_650_467_743_999_999_999;
        let cases: [(&str, i64, i128); 8] = [
            (
                "FILETIME 0 is 1601-01-01",
                0,
                -11_644_473_600 * 1_000_000_000,
            ),
            (
                "1960-01-01 plus 1234567 ticks, before the epoch",
                EPOCH - 315_619_200 * 10_000_000 + 1_234_567,
                -315_619_200 * 1_000_000_000 + 123_456_700,
            ),
            ("one tick before the epoch", EPOCH - 1, -100),
            ("the epoch", EPOCH, 0),
            (
                "the last tick of 9999",
                LAST_TICK,
                253_402_300_799_999_999_900,
            ),
            (
                "one tick past 9999 clamps to the last instant",
                LAST_TICK + 1,
                253_402_300_799_999_999_999,
            ),
            (
                "the largest value clamps to the last instant",
                i64::MAX,
                253_402_300_799_999_999_999,
            ),
            (
                "a negative value reads as 1601-01-01",
                -1,
                -11_644_473_600 * 1_000_000_000,
            ),
        ];

        for (what, ticks, unix_nanos) in cases {
            let mut record = v2_record(64, 2, 1, 1);
            put(&mut record, V2_TIME_STAMP, &ticks.to_le_bytes());

            let records = parse_usn_records(&buffer_of(&[record])).expect("valid record");

            assert_eq!(
                records[0].timestamp,
                OffsetDateTime::from_unix_timestamp_nanos(unix_nanos).unwrap(),
                "{what}"
            );
        }
    }
}

/// Windows-only: compares this module's struct layouts and `Reason` values against the real
/// `windows::Win32::System::Ioctl`/`Storage::FileSystem` types, field for field. Only meaningful
/// with `windows` in the dependency graph, so gated `cfg(windows)`. Runs in a plain
/// `cargo test` on Windows, because this module is always built there.
#[cfg(all(test, windows))]
mod layout_tests {
    use super::*;
    use std::mem::{offset_of, size_of};
    use windows::Win32::Storage::FileSystem::FILE_ID_128;
    use windows::Win32::System::Ioctl;

    #[test]
    fn common_header_matches_windows_layout() {
        assert_eq!(
            size_of::<CommonHeader>(),
            size_of::<Ioctl::USN_RECORD_COMMON_HEADER>()
        );
        assert_eq!(
            offset_of!(CommonHeader, record_length),
            offset_of!(Ioctl::USN_RECORD_COMMON_HEADER, RecordLength)
        );
        assert_eq!(
            offset_of!(CommonHeader, major_version),
            offset_of!(Ioctl::USN_RECORD_COMMON_HEADER, MajorVersion)
        );
        assert_eq!(
            offset_of!(CommonHeader, minor_version),
            offset_of!(Ioctl::USN_RECORD_COMMON_HEADER, MinorVersion)
        );
    }

    #[test]
    fn record_v2_matches_windows_layout() {
        assert_eq!(size_of::<RecordV2>(), size_of::<Ioctl::USN_RECORD_V2>());
        assert_eq!(
            offset_of!(RecordV2, record_length),
            offset_of!(Ioctl::USN_RECORD_V2, RecordLength)
        );
        assert_eq!(
            offset_of!(RecordV2, major_version),
            offset_of!(Ioctl::USN_RECORD_V2, MajorVersion)
        );
        assert_eq!(
            offset_of!(RecordV2, minor_version),
            offset_of!(Ioctl::USN_RECORD_V2, MinorVersion)
        );
        assert_eq!(
            offset_of!(RecordV2, file_reference_number),
            offset_of!(Ioctl::USN_RECORD_V2, FileReferenceNumber)
        );
        assert_eq!(
            offset_of!(RecordV2, parent_file_reference_number),
            offset_of!(Ioctl::USN_RECORD_V2, ParentFileReferenceNumber)
        );
        assert_eq!(
            offset_of!(RecordV2, usn),
            offset_of!(Ioctl::USN_RECORD_V2, Usn)
        );
        assert_eq!(
            offset_of!(RecordV2, time_stamp),
            offset_of!(Ioctl::USN_RECORD_V2, TimeStamp)
        );
        assert_eq!(
            offset_of!(RecordV2, reason),
            offset_of!(Ioctl::USN_RECORD_V2, Reason)
        );
        assert_eq!(
            offset_of!(RecordV2, source_info),
            offset_of!(Ioctl::USN_RECORD_V2, SourceInfo)
        );
        assert_eq!(
            offset_of!(RecordV2, security_id),
            offset_of!(Ioctl::USN_RECORD_V2, SecurityId)
        );
        assert_eq!(
            offset_of!(RecordV2, file_attributes),
            offset_of!(Ioctl::USN_RECORD_V2, FileAttributes)
        );
        assert_eq!(
            offset_of!(RecordV2, file_name_length),
            offset_of!(Ioctl::USN_RECORD_V2, FileNameLength)
        );
        assert_eq!(
            offset_of!(RecordV2, file_name_offset),
            offset_of!(Ioctl::USN_RECORD_V2, FileNameOffset)
        );
        assert_eq!(
            offset_of!(RecordV2, file_name),
            offset_of!(Ioctl::USN_RECORD_V2, FileName)
        );
    }

    // Reason's constants are written as numbers so `Reason` builds without the `windows` crate;
    // this pins each one to the crate's own `USN_REASON_*`, and its `Display` to the name.
    #[test]
    fn reason_constants_match_the_windows_ones() {
        macro_rules! rows {
            ($($ours:ident => $theirs:ident),* $(,)?) => {
                [$((stringify!($ours), stringify!($theirs), Reason::$ours, Ioctl::$theirs)),*]
            };
        }
        let rows = rows![
            DATA_OVERWRITE => USN_REASON_DATA_OVERWRITE,
            DATA_EXTEND => USN_REASON_DATA_EXTEND,
            DATA_TRUNCATION => USN_REASON_DATA_TRUNCATION,
            NAMED_DATA_OVERWRITE => USN_REASON_NAMED_DATA_OVERWRITE,
            NAMED_DATA_EXTEND => USN_REASON_NAMED_DATA_EXTEND,
            NAMED_DATA_TRUNCATION => USN_REASON_NAMED_DATA_TRUNCATION,
            FILE_CREATE => USN_REASON_FILE_CREATE,
            FILE_DELETE => USN_REASON_FILE_DELETE,
            EA_CHANGE => USN_REASON_EA_CHANGE,
            SECURITY_CHANGE => USN_REASON_SECURITY_CHANGE,
            RENAME_OLD_NAME => USN_REASON_RENAME_OLD_NAME,
            RENAME_NEW_NAME => USN_REASON_RENAME_NEW_NAME,
            INDEXABLE_CHANGE => USN_REASON_INDEXABLE_CHANGE,
            BASIC_INFO_CHANGE => USN_REASON_BASIC_INFO_CHANGE,
            HARD_LINK_CHANGE => USN_REASON_HARD_LINK_CHANGE,
            COMPRESSION_CHANGE => USN_REASON_COMPRESSION_CHANGE,
            ENCRYPTION_CHANGE => USN_REASON_ENCRYPTION_CHANGE,
            OBJECT_ID_CHANGE => USN_REASON_OBJECT_ID_CHANGE,
            REPARSE_POINT_CHANGE => USN_REASON_REPARSE_POINT_CHANGE,
            STREAM_CHANGE => USN_REASON_STREAM_CHANGE,
            TRANSACTED_CHANGE => USN_REASON_TRANSACTED_CHANGE,
            INTEGRITY_CHANGE => USN_REASON_INTEGRITY_CHANGE,
            DESIRED_STORAGE_CLASS_CHANGE => USN_REASON_DESIRED_STORAGE_CLASS_CHANGE,
            CLOSE => USN_REASON_CLOSE,
        ];
        for (name, windows_name, ours, theirs) in rows {
            assert_eq!(ours.bits(), theirs, "{name}");
            assert_eq!(windows_name, format!("USN_REASON_{name}"));
            assert_eq!(ours.to_string(), name, "Display of {windows_name}");
        }
    }

    #[test]
    fn file_id_128_matches_windows_layout() {
        assert_eq!(size_of::<FileId128>(), size_of::<FILE_ID_128>());
        assert_eq!(
            offset_of!(FileId128, identifier),
            offset_of!(FILE_ID_128, Identifier)
        );
    }

    #[test]
    fn record_v3_matches_windows_layout() {
        assert_eq!(size_of::<RecordV3>(), size_of::<Ioctl::USN_RECORD_V3>());
        assert_eq!(
            offset_of!(RecordV3, record_length),
            offset_of!(Ioctl::USN_RECORD_V3, RecordLength)
        );
        assert_eq!(
            offset_of!(RecordV3, major_version),
            offset_of!(Ioctl::USN_RECORD_V3, MajorVersion)
        );
        assert_eq!(
            offset_of!(RecordV3, minor_version),
            offset_of!(Ioctl::USN_RECORD_V3, MinorVersion)
        );
        assert_eq!(
            offset_of!(RecordV3, file_reference_number),
            offset_of!(Ioctl::USN_RECORD_V3, FileReferenceNumber)
        );
        assert_eq!(
            offset_of!(RecordV3, parent_file_reference_number),
            offset_of!(Ioctl::USN_RECORD_V3, ParentFileReferenceNumber)
        );
        assert_eq!(
            offset_of!(RecordV3, usn),
            offset_of!(Ioctl::USN_RECORD_V3, Usn)
        );
        assert_eq!(
            offset_of!(RecordV3, time_stamp),
            offset_of!(Ioctl::USN_RECORD_V3, TimeStamp)
        );
        assert_eq!(
            offset_of!(RecordV3, reason),
            offset_of!(Ioctl::USN_RECORD_V3, Reason)
        );
        assert_eq!(
            offset_of!(RecordV3, source_info),
            offset_of!(Ioctl::USN_RECORD_V3, SourceInfo)
        );
        assert_eq!(
            offset_of!(RecordV3, security_id),
            offset_of!(Ioctl::USN_RECORD_V3, SecurityId)
        );
        assert_eq!(
            offset_of!(RecordV3, file_attributes),
            offset_of!(Ioctl::USN_RECORD_V3, FileAttributes)
        );
        assert_eq!(
            offset_of!(RecordV3, file_name_length),
            offset_of!(Ioctl::USN_RECORD_V3, FileNameLength)
        );
        assert_eq!(
            offset_of!(RecordV3, file_name_offset),
            offset_of!(Ioctl::USN_RECORD_V3, FileNameOffset)
        );
        assert_eq!(
            offset_of!(RecordV3, file_name),
            offset_of!(Ioctl::USN_RECORD_V3, FileName)
        );
    }
}
