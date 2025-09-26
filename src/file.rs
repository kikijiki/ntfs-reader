// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use std::mem::size_of;

use crate::{api::*, attribute::NtfsAttribute, mft::Mft};

pub struct NtfsFile<'a> {
    pub number: u64,
    pub header: &'a NtfsFileRecordHeader,
    pub data: &'a [u8],
}

impl<'a> NtfsFile<'a> {
    pub fn new(number: u64, data: &'a [u8]) -> Self {
        assert!(
            data.len() >= size_of::<NtfsFileRecordHeader>(),
            "NTFS file record too small",
        );
        let header = unsafe { &*(data.as_ptr() as *const NtfsFileRecordHeader) };
        NtfsFile {
            number,
            header,
            data,
        }
    }

    pub fn number(&self) -> u64 {
        self.number
    }

    pub fn reference_number(&self) -> u64 {
        let seq = self.header.sequence_value as u64;
        (seq << 48) | (self.number & 0x0000_FFFF_FFFF_FFFF)
    }

    pub fn get_file_id(&self) -> FileId {
        FileId::Normal(self.reference_number())
    }

    pub fn is_valid(data: &[u8]) -> bool {
        if data.len() < size_of::<NtfsFileRecordHeader>() {
            return false;
        }
        let header = unsafe { &*(data.as_ptr() as *const NtfsFileRecordHeader) };
        if &header.signature != FILE_RECORD_SIGNATURE {
            return false;
        }

        if header.update_sequence_length == 0 {
            return false;
        }

        if header.used_size as usize > data.len() {
            return false;
        }

        let usa_end =
            header.update_sequence_offset as usize + header.update_sequence_length as usize * 2;

        let usa_num = header.update_sequence_length as usize - 1;
        let sector_num = data.len() / SECTOR_SIZE;

        if usa_end > data.len() || usa_num > sector_num {
            return false;
        }

        if header.attributes_offset as usize >= header.used_size as usize {
            return false;
        }

        true
    }

    pub fn attributes<F>(&self, mut f: F)
    where
        F: FnMut(&NtfsAttribute),
    {
        let mut offset = self.header.attributes_offset as usize;
        let used = usize::min(self.header.used_size as usize, self.data.len());

        while offset < used {
            let slice = &self.data[offset..used];
            let attr = match NtfsAttribute::new(slice) {
                Some(attr) => attr,
                None => break,
            };

            if attr.header.type_id == NtfsAttributeType::End as u32 {
                break;
            }

            f(&attr);

            let attr_len = attr.len();
            if attr_len == 0 {
                break;
            }
            offset = match offset.checked_add(attr_len) {
                Some(next) if next <= used => next,
                _ => break,
            };
        }
    }

    pub fn get_attribute(&self, attribute_type: NtfsAttributeType) -> Option<NtfsAttribute<'_>> {
        let mut offset = self.header.attributes_offset as usize;
        let used = usize::min(self.header.used_size as usize, self.data.len());

        while offset < used {
            let slice = &self.data[offset..used];
            let attr = match NtfsAttribute::new(slice) {
                Some(attr) => attr,
                None => break,
            };

            if attr.header.type_id == NtfsAttributeType::End as u32 {
                break;
            }
            if attr.header.type_id == attribute_type as u32 {
                return Some(attr);
            }

            let attr_len = attr.len();
            if attr_len == 0 {
                break;
            }
            offset = match offset.checked_add(attr_len) {
                Some(next) if next <= used => next,
                _ => break,
            };
        }
        None
    }

    pub fn get_best_file_name(&self, mft: &Mft) -> Option<NtfsFileName> {
        let mut offset = self.header.attributes_offset as usize;
        let used = usize::min(self.header.used_size as usize, self.data.len());
        let mut best = None;

        while offset < used {
            let slice = &self.data[offset..used];
            let attr = match NtfsAttribute::new(slice) {
                Some(attr) => attr,
                None => break,
            };

            if attr.header.type_id == NtfsAttributeType::End as u32 {
                break;
            }

            if attr.header.type_id == NtfsAttributeType::FileName as u32 {
                if let Some(name) = attr.as_name() {
                    if !name.is_reparse_point() {
                        if name.header.namespace == NtfsFileNamespace::Win32 as u8
                            || name.header.namespace == NtfsFileNamespace::Win32AndDos as u8
                        {
                            return Some(name);
                        } else {
                            best = Some(name);
                        }
                    }
                }
            }

            if attr.header.type_id == NtfsAttributeType::AttributeList as u32 {
                if attr.header.is_non_resident != 0 {
                    // We do not support non-resident attribute lists here.
                    break;
                }
                let header = match attr.resident_header() {
                    Some(header) => header,
                    None => break,
                };
                let value_offset = header.value_offset as usize;
                let value_length = header.value_length as usize;
                let value_end = match value_offset.checked_add(value_length) {
                    Some(end) if end <= attr.data().len() => end,
                    _ => break,
                };
                let attr_slice = attr.data();
                let att_data = &attr_slice[value_offset..value_end];

                let mut att_offset = 0usize;
                while att_offset < att_data.len() {
                    let entry_slice = &att_data[att_offset..];
                    let entry = match parse_attribute_list_entry(entry_slice) {
                        Some(entry) => entry,
                        None => break,
                    };
                    let entry_len = entry.length as usize;
                    if entry.type_id == NtfsAttributeType::FileName as u32 {
                        let rec = mft.get_record(entry.reference())?;
                        let att = rec.get_attribute(NtfsAttributeType::FileName)?;

                        if let Some(name) = att.as_name() {
                            if !name.is_reparse_point() {
                                if name.header.namespace == NtfsFileNamespace::Win32 as u8
                                    || name.header.namespace == NtfsFileNamespace::Win32AndDos as u8
                                {
                                    return Some(name);
                                } else {
                                    best = Some(name);
                                    break;
                                }
                            }
                        }
                    }

                    if entry_len == 0 {
                        break;
                    }
                    att_offset = match att_offset.checked_add(entry_len) {
                        Some(next) if next <= att_data.len() => next,
                        _ => break,
                    };
                    let align = (8 - (att_offset % 8)) % 8;
                    att_offset = match att_offset.checked_add(align) {
                        Some(next) if next <= att_data.len() => next,
                        _ => break,
                    };
                }
            }

            let attr_len = attr.len();
            if attr_len == 0 {
                break;
            }
            offset = match offset.checked_add(attr_len) {
                Some(next) if next <= used => next,
                _ => break,
            };
        }

        best
    }

    // This cannot read nonresident data!
    pub fn read_data(&self) -> Option<&[u8]> {
        if let Some(att) = self.get_attribute(NtfsAttributeType::Data) {
            if att.header.is_non_resident == 0 {
                return att.as_resident_data();
            }
        }
        None
    }

    pub fn is_used(&self) -> bool {
        self.header.flags & NtfsFileFlags::InUse as u16 != 0
    }

    pub fn is_directory(&self) -> bool {
        self.header.flags & NtfsFileFlags::IsDirectory as u16 != 0
    }
}

fn parse_attribute_list_entry(data: &[u8]) -> Option<&NtfsAttributeListEntry> {
    if data.len() < size_of::<NtfsAttributeListEntry>() {
        return None;
    }
    let entry = unsafe { &*(data.as_ptr() as *const NtfsAttributeListEntry) };
    let length = entry.length as usize;
    if length < size_of::<NtfsAttributeListEntry>() || length > data.len() {
        return None;
    }
    Some(entry)
}
