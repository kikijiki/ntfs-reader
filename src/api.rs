// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! NTFS types the crate hands out: names, attribute types, the standard
//! information attribute, and file ids. Raw on-disk layouts stay private to
//! the crate.

use std::ffi::OsString;
use std::fmt;

use time::{OffsetDateTime, PrimitiveDateTime};

pub(crate) const SECTOR_SIZE: usize = 512;
/// The record number of the root directory. Fixed by the NTFS format, so
/// [`NtfsFileName::parent_number`] equal to it means the file is in the root.
pub const ROOT_RECORD: u64 = 5;
/// The first record number NTFS hands to ordinary files and directories.
/// Records below it (0 to 23) are the reserved metadata files (`$MFT`,
/// `$LogFile`, `$Volume`, the root directory, ...); [`crate::Mft::files`]
/// starts here.
pub const FIRST_NORMAL_RECORD: u64 = 24;
pub(crate) const FILE_RECORD_SIGNATURE: &[u8; 4] = b"FILE";
pub(crate) const EPOCH_DIFFERENCE: u64 = 116_444_736_000_000_000;

#[allow(unused)]
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub(crate) struct BootSector {
    pub crap_0: [u8; 11],
    pub sector_size: u16,
    pub sectors_per_cluster: u8,
    pub crap_1: [u8; 26],
    pub total_sectors: u64,
    pub mft_lcn: u64,
    pub mft_lcn_mirror: u64,
    pub file_record_size_info: i8,
    pub crap_2: [u8; 447],
}

#[repr(C, packed)]
pub(crate) struct NtfsFileRecordHeader {
    // Record
    pub signature: [u8; 4],
    pub update_sequence_offset: u16,
    pub update_sequence_length: u16,
    pub logfile_sequence_number: u64,
    // File
    pub sequence_value: u16,
    pub link_count: u16,
    pub attributes_offset: u16,
    pub flags: u16,
    pub used_size: u32,
    pub allocated_size: u32,
    pub base_reference: u64,
    pub next_attribute_id: u16,
}

#[repr(u16)]
pub(crate) enum NtfsFileFlags {
    InUse = 0x0001,
    IsDirectory = 0x0002,
}

#[allow(unused)]
#[repr(C, packed)]
pub(crate) struct NtfsAttributeHeader {
    pub type_id: u32,
    pub length: u32,
    pub is_non_resident: u8,
    pub name_length: u8,
    pub name_offset: u16,
    pub flags: u16,
    pub id: u16,
}

#[repr(C, packed)]
pub(crate) struct NtfsResidentAttributeHeader {
    pub attribute_header: NtfsAttributeHeader,
    pub value_length: u32,
    pub value_offset: u16,
    pub indexed_flag: u8,
}

#[repr(C, packed)]
pub(crate) struct NtfsNonResidentAttributeHeader {
    pub attribute_header: NtfsAttributeHeader,
    pub lowest_vcn: i64,
    pub highest_vcn: i64,
    pub data_runs_offset: u16,
    pub compression_unit_exponent: u8,
    pub reserved: [u8; 5],
    pub allocated_size: u64,
    pub data_size: u64,
    pub initialized_size: u64,
}

/// The `$STANDARD_INFORMATION` attribute of a file: its four timestamps and
/// Win32 attribute flags. Get one from
/// [`NtfsFile::standard_information`](crate::NtfsFile::standard_information).
///
/// The timestamps are Windows FILETIMEs on disk (100 ns ticks since
/// 1601-01-01 UTC), converted exactly: a time before 1970 is a real,
/// earlier date; one beyond the latest `OffsetDateTime` can represent
/// (year 9999, unless `time`'s `large-dates` feature is enabled) is clamped
/// to that latest time.
#[repr(C, packed)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct NtfsStandardInformation {
    creation_time: u64,
    modification_time: u64,
    mft_record_modification_time: u64,
    access_time: u64,
    file_attributes: u32,
}

impl NtfsStandardInformation {
    /// When the file was created.
    pub fn created(&self) -> OffsetDateTime {
        ntfs_to_unix_time(self.creation_time)
    }

    /// When the file's data was last written.
    pub fn modified(&self) -> OffsetDateTime {
        ntfs_to_unix_time(self.modification_time)
    }

    /// When the file's MFT record was last changed (attributes, names, not
    /// only data).
    pub fn mft_modified(&self) -> OffsetDateTime {
        ntfs_to_unix_time(self.mft_record_modification_time)
    }

    /// When the file was last read. NTFS can update this lazily, and Windows
    /// can turn the update off entirely.
    pub fn accessed(&self) -> OffsetDateTime {
        ntfs_to_unix_time(self.access_time)
    }

    /// The raw Windows `FILE_ATTRIBUTE_*` flags, as stored on disk. Can
    /// differ from what Win32 reports for a file behind a filter driver:
    /// the WOF (Windows Overlay Filter) hides `SPARSE_FILE` and
    /// `REPARSE_POINT` on a file it compressed, while the record still
    /// carries both.
    pub fn file_attributes(&self) -> u32 {
        self.file_attributes
    }
}

impl fmt::Debug for NtfsStandardInformation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NtfsStandardInformation")
            .field("created", &self.created())
            .field("modified", &self.modified())
            .field("mft_modified", &self.mft_modified())
            .field("accessed", &self.accessed())
            .field(
                "file_attributes",
                &format_args!("{:#x}", self.file_attributes()),
            )
            .finish()
    }
}

/// Namespace of a `$FILE_NAME` attribute. A non-8.3 long name gets a `Win32`
/// entry plus a separate `Dos` entry for its short name; `Win32AndDos` means
/// one entry covers both. `Posix`/`Win32` otherwise mark a real hard link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NtfsFileNamespace {
    /// Case-sensitive name with almost any character. A real hard link.
    Posix = 0,
    /// A long name. A real hard link.
    Win32 = 1,
    /// The generated 8.3 short name of a sibling `Win32` entry. Not a hard
    /// link of its own.
    Dos = 2,
    /// One entry that is both a valid `Win32` and a valid `Dos` name.
    Win32AndDos = 3,
}

impl TryFrom<u8> for NtfsFileNamespace {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Posix),
            1 => Ok(Self::Win32),
            2 => Ok(Self::Dos),
            3 => Ok(Self::Win32AndDos),
            other => Err(other),
        }
    }
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub(crate) struct NtfsFileNameHeader {
    pub parent_directory_reference: u64,
    pub crap_0: [u8; 32],
    pub allocated_size: u64,
    pub real_size: u64,
    pub file_attributes: u32,
    pub reparse_point_tag: u32,
    pub name_length: u8,
    pub namespace: u8,
}

#[repr(u32)]
pub(crate) enum NtfsFileNameFlags {
    ReadOnly = 0x0001,
    Hidden = 0x0002,
    System = 0x0004,
    // Only used to build synthetic test fixtures.
    #[cfg(test)]
    Archive = 0x0020,
    #[cfg(test)]
    SparseFile = 0x0200,
    ReparsePoint = 0x0400,
}

/// One `$FILE_NAME` attribute: a name of a file in a directory, plus a copy
/// of some of the file's metadata as of the last update to the name. A file
/// has one per hard link, plus one for a DOS 8.3 short name if it has one.
///
/// Get them from [`NtfsFile::names`](crate::NtfsFile::names) or
/// [`NtfsFile::hard_links`](crate::NtfsFile::hard_links). The name itself is
/// [`Self::to_os_string`] (lossless) or [`Display`](fmt::Display) (best
/// effort).
#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct NtfsFileName {
    pub(crate) header: NtfsFileNameHeader,
    pub(crate) data: [u16; 255],
}

/// The name as a `String`, best effort only. A Windows file name is a raw
/// UTF-16 code unit sequence with no requirement that it be valid UTF-16
/// (the filesystem accepts an unpaired surrogate), and a `str` cannot hold
/// one, so each such unit becomes U+FFFD. Use [`NtfsFileName::to_os_string`]
/// to name the actual file again.
impl fmt::Display for NtfsFileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let data = self.data;
        let string = String::from_utf16_lossy(&data[..self.header.name_length as usize]);
        write!(f, "{}", string)
    }
}

/// Lossless `[u16] -> OsString`, unlike `String::from_utf16_lossy` (which
/// cannot represent an unpaired surrogate and substitutes U+FFFD instead).
#[cfg(windows)]
pub(crate) fn utf16_to_os_string(units: &[u16]) -> OsString {
    use std::os::windows::ffi::OsStringExt;
    OsString::from_wide(units)
}

/// Only reachable off Windows, where the `internals` feature builds this
/// module for Unix-side unit tests (synthetic input, never a real file). No
/// portable "arbitrary UTF-16 to `OsString`" constructor exists, so this
/// encodes WTF-8, what a Windows `OsString` uses internally: ordinary UTF-8
/// per scalar value, and the 3-byte form of the code point for an unpaired
/// surrogate (which UTF-8 forbids). A Unix `OsString` can hold any bytes, so
/// the result is valid there; unit tests check the exact bytes.
#[cfg(unix)]
pub(crate) fn utf16_to_os_string(units: &[u16]) -> OsString {
    use std::os::unix::ffi::OsStringExt;

    let mut bytes = Vec::with_capacity(units.len() * 3);
    for unit in char::decode_utf16(units.iter().copied()) {
        match unit {
            Ok(ch) => {
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
            Err(unpaired) => {
                let unit = unpaired.unpaired_surrogate() as u32;
                bytes.push(0xE0 | (unit >> 12) as u8);
                bytes.push(0x80 | ((unit >> 6) & 0x3F) as u8);
                bytes.push(0x80 | (unit & 0x3F) as u8);
            }
        }
    }
    OsString::from_vec(bytes)
}

/// Neither Windows nor Unix: not a target the crate is used on. Lossy, only
/// to compile.
#[cfg(not(any(windows, unix)))]
pub(crate) fn utf16_to_os_string(units: &[u16]) -> OsString {
    String::from_utf16_lossy(units).into()
}

#[repr(C, packed)]
pub(crate) struct NtfsAttributeListEntry {
    pub type_id: u32,
    pub length: u16,
    pub name_length: u8,
    pub name_offset: u8,
    pub starting_vcn: u64,
    pub base_file_reference: u64,
    pub id: u16,
    //pub name: Option<String>,
}

impl NtfsAttributeListEntry {
    /// The record number of the entry's target record.
    pub fn number(&self) -> u64 {
        self.base_file_reference & 0x0000_FFFF_FFFF_FFFF
    }
}

impl NtfsFileName {
    /// The name, losslessly. Unlike `Display`/`to_string` (see the impl's
    /// doc comment), this always round-trips: any code unit sequence NTFS
    /// accepts, including an unpaired surrogate, survives unchanged.
    pub fn to_os_string(&self) -> OsString {
        let data = self.data;
        utf16_to_os_string(&data[..self.header.name_length as usize])
    }

    /// The parent directory's record number, without its sequence number.
    /// A record number alone cannot tell whether the parent was since freed
    /// and reused; use [`Self::parent_reference`] to check that.
    pub fn parent_number(&self) -> u64 {
        self.header.parent_directory_reference & 0x0000_FFFF_FFFF_FFFF
    }

    /// The parent directory as the id it has while live, the one a journal
    /// record carries and [`NtfsFile::file_id`](crate::NtfsFile::file_id)
    /// gives, live or deleted: `name.parent_id() == directory.file_id()`
    /// finds the names in a directory whatever its state. Same value as
    /// [`Self::parent_reference`], typed as an id.
    pub fn parent_id(&self) -> FileId {
        FileId::from(self.parent_reference())
    }

    /// The parent directory reference as stored, sequence number included,
    /// the sequence the directory had while live. Equals a live directory's
    /// [`reference`](crate::NtfsFile::reference) (how to detect a stale
    /// reference: the record number reused by an unrelated file after the
    /// parent was freed), but **not** the `reference()` of a freed
    /// directory, whose sequence is one higher. Compare [`Self::parent_id`]
    /// with [`NtfsFile::file_id`](crate::NtfsFile::file_id) instead, which
    /// holds for both.
    pub fn parent_reference(&self) -> u64 {
        self.header.parent_directory_reference
    }

    /// Whether this name is directly in the volume's root directory: its
    /// parent is record [`ROOT_RECORD`]. A name is one hard link, so a file
    /// with several links can be in the root under only one. The root is
    /// never freed, so this also holds for a deleted file's names. Only the
    /// record number is compared, not the sequence in
    /// [`Self::parent_reference`]; [`crate::Mft::resolve_path`] checks that
    /// too.
    pub fn is_in_root(&self) -> bool {
        self.parent_number() == ROOT_RECORD
    }

    /// Whether the read-only flag was set when this name was last updated.
    pub fn is_readonly(&self) -> bool {
        self.header.file_attributes & NtfsFileNameFlags::ReadOnly as u32 != 0
    }

    /// Whether the hidden flag was set when this name was last updated.
    pub fn is_hidden(&self) -> bool {
        self.header.file_attributes & NtfsFileNameFlags::Hidden as u32 != 0
    }

    /// Whether the system flag was set when this name was last updated.
    pub fn is_system(&self) -> bool {
        self.header.file_attributes & NtfsFileNameFlags::System as u32 != 0
    }

    /// Whether the reparse point flag was set when this name was last
    /// updated (a junction, a symbolic link, a cloud placeholder, etc.).
    pub fn is_reparse_point(&self) -> bool {
        self.header.file_attributes & NtfsFileNameFlags::ReparsePoint as u32 != 0
    }

    /// `None` only for a corrupt namespace byte outside 0-3.
    pub fn namespace(&self) -> Option<NtfsFileNamespace> {
        self.header.namespace.try_into().ok()
    }

    /// True for the DOS 8.3 short-name alias of a sibling `Win32` entry,
    /// which does not count as a separate hard link.
    pub fn is_dos_alias(&self) -> bool {
        self.namespace() == Some(NtfsFileNamespace::Dos)
    }
}

impl fmt::Debug for NtfsFileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NtfsFileName")
            .field("name", &self.to_os_string())
            .field("namespace", &self.namespace())
            .field(
                "parent_reference",
                &format_args!("{:#x}", self.parent_reference()),
            )
            .finish()
    }
}

/// The type of an attribute of an MFT record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum NtfsAttributeType {
    /// `$STANDARD_INFORMATION`: timestamps and attribute flags.
    StandardInformation = 0x10,
    /// `$ATTRIBUTE_LIST`: where the attributes of a file that spills into extension records live.
    AttributeList = 0x20,
    /// `$FILE_NAME`: one name of the file.
    FileName = 0x30,
    /// `$OBJECT_ID`: the file's object id.
    ObjectId = 0x40,
    /// `$SECURITY_DESCRIPTOR`: the file's security descriptor (old volumes;
    /// newer ones use `$Secure`).
    SecurityDescriptor = 0x50,
    /// `$VOLUME_NAME`: the volume label (`$Volume` only).
    VolumeName = 0x60,
    /// `$VOLUME_INFORMATION`: the NTFS version and volume flags (`$Volume` only).
    VolumeInformation = 0x70,
    /// `$DATA`: a data stream. Unnamed for the file contents, named for an alternate data stream.
    Data = 0x80,
    /// `$INDEX_ROOT`: the root of a directory's index, or of another index.
    IndexRoot = 0x90,
    /// `$INDEX_ALLOCATION`: the index blocks of a directory that outgrew its `$INDEX_ROOT`.
    IndexAllocation = 0xA0,
    /// `$BITMAP`: which records or index blocks are in use.
    Bitmap = 0xB0,
    /// `$REPARSE_POINT`: reparse data (symbolic links, junctions, placeholders).
    ReparsePoint = 0xC0,
    /// `$EA_INFORMATION`: summary of the extended attributes.
    EaInformation = 0xD0,
    /// `$EA`: extended attributes.
    Ea = 0xE0,
    /// `$LOGGED_UTILITY_STREAM`: EFS and other logged streams.
    LoggedUtilityStream = 0x100,
    /// End-of-attributes marker.
    End = 0xFFFF_FFFF,
}

impl TryFrom<u32> for NtfsAttributeType {
    type Error = u32;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Ok(match value {
            0x10 => Self::StandardInformation,
            0x20 => Self::AttributeList,
            0x30 => Self::FileName,
            0x40 => Self::ObjectId,
            0x50 => Self::SecurityDescriptor,
            0x60 => Self::VolumeName,
            0x70 => Self::VolumeInformation,
            0x80 => Self::Data,
            0x90 => Self::IndexRoot,
            0xA0 => Self::IndexAllocation,
            0xB0 => Self::Bitmap,
            0xC0 => Self::ReparsePoint,
            0xD0 => Self::EaInformation,
            0xE0 => Self::Ea,
            0x100 => Self::LoggedUtilityStream,
            0xFFFF_FFFF => Self::End,
            other => return Err(other),
        })
    }
}

/// The identity of a file on a volume, as the MFT and the USN journal
/// report it.
///
/// 128 bits wide so one type covers every form. On NTFS the id is the
/// 64-bit *file reference* (record number in the low 48 bits, sequence
/// number in the 16 above, so a reused record number gets a different id)
/// zero-extended to 128 bits. A V2 journal record carries the 64-bit form,
/// a V3 record the 128-bit form, and
/// [`NtfsFile::file_id`](crate::NtfsFile::file_id) builds it from the MFT,
/// so the same file compares equal however read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(u128);

impl FileId {
    /// All 128 bits. For an NTFS file the high 64 bits are zero.
    pub const fn as_u128(self) -> u128 {
        self.0
    }

    /// The 64-bit file reference (`sequence << 48 | record number`), or
    /// `None` if it does not fit in 64 bits, never the case for NTFS.
    pub fn as_reference(self) -> Option<u64> {
        u64::try_from(self.0).ok()
    }

    /// From the `FILE_ID_128` bytes of a V3 journal record. Windows lays the
    /// 128-bit value out little-endian.
    pub(crate) fn from_le_bytes(bytes: [u8; 16]) -> Self {
        FileId(u128::from_le_bytes(bytes))
    }
}

impl From<u64> for FileId {
    /// From a 64-bit file reference.
    fn from(reference: u64) -> Self {
        FileId(reference.into())
    }
}

impl From<u128> for FileId {
    fn from(id: u128) -> Self {
        FileId(id)
    }
}

/// Converts a FILETIME (100 ns ticks since 1601-01-01 UTC) to UTC, exactly:
/// a time before 1970 is the real, earlier date. A value outside what
/// `OffsetDateTime` can represent clamps to its latest or earliest time.
/// The latest is year 9999 unless `time`'s `large-dates` feature is
/// enabled, so `u64::MAX` (year 60056) clamps; the earliest is never
/// reached, since FILETIME 0 (1601) is in range.
pub(crate) fn ntfs_to_unix_time(ticks: u64) -> OffsetDateTime {
    let nanos = (ticks as i128 - EPOCH_DIFFERENCE as i128) * 100;
    OffsetDateTime::from_unix_timestamp_nanos(nanos).unwrap_or_else(|_| {
        if nanos < 0 {
            PrimitiveDateTime::MIN.assume_utc()
        } else {
            PrimitiveDateTime::MAX.assume_utc()
        }
    })
}

#[cfg(test)]
#[path = "tests/api.rs"]
mod tests;
