// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use binread::BinRead;
use time::OffsetDateTime;

pub const SECTOR_SIZE: usize = 512;
pub const MFT_RECORD: u64 = 0;
pub const ROOT_RECORD: u64 = 5;
pub const FIRST_NORMAL_RECORD: u64 = 24;
pub const FILE_RECORD_SIGNATURE: &[u8; 4] = b"FILE";
pub const EPOCH_DIFFERENCE: u64 = 116_444_736_000_000_000;

#[allow(unused)]
#[repr(C, packed)]
#[derive(Clone, Copy, BinRead)]
pub struct BootSector {
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
pub struct NtfsFileRecordHeader {
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
pub enum NtfsFileFlags {
    InUse = 0x0001,
    IsDirectory = 0x0002,
}

#[allow(unused)]
#[repr(C, packed)]
pub struct NtfsAttributeHeader {
    pub type_id: u32,
    pub length: u32,
    pub is_non_resident: u8,
    pub name_length: u8,
    pub name_offset: u16,
    pub flags: u16,
    pub id: u16,
}

#[repr(C, packed)]
pub struct NtfsResidentAttributeHeader {
    pub attribute_header: NtfsAttributeHeader,
    pub value_length: u32,
    pub value_offset: u16,
    pub indexed_flag: u8,
}

#[repr(C, packed)]
pub struct NtfsNonResidentAttributeHeader {
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

#[repr(C, packed)]
pub struct NtfsStandardInformation {
    pub creation_time: u64,
    pub modification_time: u64,
    pub mft_record_modification_time: u64,
    pub access_time: u64,
    pub file_attributes: u32,
}

#[repr(u8)]
pub enum NtfsFileNamespace {
    Posix = 0,
    Win32 = 1,
    Dos = 2,
    Win32AndDos = 3,
}

#[repr(C, packed)]
pub struct NtfsFileNameHeader {
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
pub enum NtfsFileNameFlags {
    ReadOnly = 0x0001,
    Hidden = 0x0002,
    System = 0x0004,
    Archive = 0x0020,
    Device = 0x0040,
    Normal = 0x0080,
    Temporary = 0x0100,
    SparseFile = 0x0200,
    ReparsePoint = 0x0400,
    Compressed = 0x0800,
    Offline = 0x1000,
    NotContentIndexed = 0x2000,
    Encrypted = 0x4000,
    IsDirectory = 0x1000_0000,
}

#[repr(C, packed)]
pub struct NtfsFileName {
    pub header: NtfsFileNameHeader,
    pub data: [u16; 255],
}

impl NtfsFileName {
    pub fn to_string(&self) -> String {
        let data = self.data;
        String::from_utf16_lossy(&data[..self.header.name_length as usize])
    }

    pub fn parent(&self) -> u64 {
        self.header.parent_directory_reference & 0x0000_FFFF_FFFF_FFFF
    }

    pub fn is_readonly(&self) -> bool {
        self.header.file_attributes & NtfsFileNameFlags::ReadOnly as u32 != 0
    }

    pub fn is_hidden(&self) -> bool {
        self.header.file_attributes & NtfsFileNameFlags::Hidden as u32 != 0
    }

    pub fn is_system(&self) -> bool {
        self.header.file_attributes & NtfsFileNameFlags::System as u32 != 0
    }

    pub fn is_reparse_point(&self) -> bool {
        self.header.file_attributes & NtfsFileNameFlags::ReparsePoint as u32 != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum NtfsAttributeType {
    StandardInformation = 0x10,
    AttributeList = 0x20,
    FileName = 0x30,
    Data = 0x80,
    Bitmap = 0xB0,
    End = 0xFFFF_FFFF,
}

pub fn ntfs_to_unix_time(src: u64) -> OffsetDateTime {
    let unix = (src - EPOCH_DIFFERENCE) as i128;
    OffsetDateTime::from_unix_timestamp_nanos(unix * 100).unwrap()
}
