// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use crate::{api::*, attribute::NtfsAttribute, mft::Mft};

pub struct NtfsFile<'a> {
    pub number: u64,
    pub header: &'a NtfsFileRecordHeader,
    pub data: &'a [u8],
}

impl<'a> NtfsFile<'a> {
    pub fn new(number: u64, data: &'a [u8]) -> Self {
        unsafe {
            let header = &*(data.as_ptr() as *const NtfsFileRecordHeader);
            NtfsFile {
                number,
                header,
                data,
            }
        }
    }

    pub fn number(&self) -> u64 {
        self.number
    }

    pub fn is_valid(data: &[u8]) -> bool {
        let header = unsafe { &*(data.as_ptr() as *const NtfsFileRecordHeader) };
        if &header.signature != FILE_RECORD_SIGNATURE {
            return false;
        }

        if header.update_sequence_length == 0 {
            return false;
        }

        let usa_end =
            header.update_sequence_offset as usize + header.update_sequence_length as usize * 2;

        let usa_num = header.update_sequence_length as usize - 1;
        let sector_num = data.len() / SECTOR_SIZE;

        if usa_end > data.len() || usa_num > sector_num {
            return false;
        }

        true
    }

    pub fn attributes<F>(&self, mut f: F)
    where
        F: FnMut(&NtfsAttribute),
    {
        let mut offset = self.header.attributes_offset as usize;
        loop {
            if offset >= self.header.used_size as usize {
                break;
            }

            let att_type = u32::from_le_bytes(self.data[offset..offset + 4].try_into().unwrap());
            if att_type == NtfsAttributeType::End as u32 {
                break;
            }

            let att = NtfsAttribute::new(&self.data[offset..]);
            f(&att);

            offset += att.header.length as usize;
        }
    }

    pub fn get_attribute(&self, attribute_type: NtfsAttributeType) -> Option<NtfsAttribute> {
        let mut offset = self.header.attributes_offset as usize;

        loop {
            if offset >= self.header.used_size as usize {
                break;
            }
            let att = NtfsAttribute::new(&self.data[offset..]);
            if att.header.type_id == NtfsAttributeType::End as u32 {
                break;
            }
            if att.header.type_id == attribute_type as u32 {
                return Some(NtfsAttribute::new(&self.data[offset..]));
            }

            offset += att.header.length as usize;
        }
        None
    }

    pub fn get_best_file_name(&self, mft: &Mft) -> Option<NtfsFileName> {
        let mut offset = self.header.attributes_offset as usize;
        let mut best = None;

        loop {
            if offset >= self.header.used_size as usize {
                break;
            }
            let att = NtfsAttribute::new(&self.data[offset..]);
            if att.header.type_id == NtfsAttributeType::End as u32 {
                break;
            }

            if att.header.type_id == NtfsAttributeType::FileName as u32 {
                let name = att.as_name();

                // Ignore junctions
                if !name.is_reparse_point() {
                    if name.header.namespace == NtfsFileNamespace::Win32 as u8
                        || name.header.namespace == NtfsFileNamespace::Win32AndDos as u8
                    {
                        return Some(name.clone());
                    } else {
                        best = Some(name.clone());
                    }
                }
            }

            if att.header.type_id == NtfsAttributeType::AttributeList as u32 {
                let header = unsafe {
                    &*(self.data[offset..].as_ptr() as *const NtfsResidentAttributeHeader)
                };

                let att_data = &self.data[offset + header.value_offset as usize..];

                let mut att_offset = 0;
                while att_offset < header.value_length as usize {
                    let entry = unsafe {
                        &*(att_data[att_offset..].as_ptr() as *const NtfsAttributeListEntry)
                    };
                    if entry.type_id == NtfsAttributeType::FileName as u32 {
                        let rec = mft.get_record(entry.reference())?;
                        let att = rec.get_attribute(NtfsAttributeType::FileName)?;
                        let name = att.as_name();

                        // Ignore junctions
                        if !name.is_reparse_point() {
                            if name.header.namespace == NtfsFileNamespace::Win32 as u8
                                || name.header.namespace == NtfsFileNamespace::Win32AndDos as u8
                            {
                                return Some(name.clone());
                            } else {
                                best = Some(name.clone());
                                break;
                            }
                        }
                    }

                    att_offset += entry.length as usize;
                    // Make sure the offset is aligned to 8 bytes
                    att_offset += (8 - (att_offset % 8)) % 8;
                }
            }

            offset += att.header.length as usize;
        }

        best
    }

    // This cannot read nonresident data!
    pub fn read_data(&self) -> Option<&[u8]> {
        if let Some(att) = self.get_attribute(NtfsAttributeType::Data) {
            assert!(att.header.is_non_resident != 0);
            return Some(att.as_resident_data());
        }
        None
    }

    pub fn is_used(&self) -> bool {
        return self.header.flags & NtfsFileFlags::InUse as u16 != 0;
    }

    pub fn is_directory(&self) -> bool {
        return self.header.flags & NtfsFileFlags::IsDirectory as u16 != 0;
    }
}
