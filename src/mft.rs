// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use std::io::{Read, Seek, SeekFrom};
use std::mem::size_of;

use crate::{
    aligned_reader::open_volume,
    api::*,
    attribute::{DataRun, NtfsAttribute},
    errors::{NtfsReaderError, NtfsReaderResult},
    file::NtfsFile,
    volume::Volume,
};

pub struct Mft {
    pub volume: Volume,
    pub data: Vec<u8>,
    pub bitmap: Vec<u8>,
    pub max_record: u64,
}

impl Mft {
    pub fn new(volume: Volume) -> NtfsReaderResult<Self> {
        let mut reader = open_volume(&volume.path)?;

        let mft_record = Self::get_record_fs(
            &mut reader,
            volume.file_record_size as usize,
            volume.mft_position,
        )?;

        let mut data =
            Self::read_data_fs(&volume, &mut reader, &mft_record, NtfsAttributeType::Data)?
                .ok_or_else(|| NtfsReaderError::MissingMftAttribute("Data".to_string()))?;
        let bitmap =
            Self::read_data_fs(&volume, &mut reader, &mft_record, NtfsAttributeType::Bitmap)?
                .ok_or_else(|| NtfsReaderError::MissingMftAttribute("Bitmap".to_string()))?;

        let max_record = (data.len() / volume.file_record_size as usize) as u64;

        // Fixup all records so we are non mutable from now on.
        for number in 0..max_record {
            let start = number as usize * volume.file_record_size as usize;
            let end = start + volume.file_record_size as usize;
            let data = &mut data[start..end];
            Self::fixup_record(number, data)?;
        }

        Ok(Mft {
            volume,
            data,
            bitmap,
            max_record,
        })
    }

    pub fn record_exists(&self, number: u64) -> bool {
        if number >= self.max_record {
            return false;
        }

        let bitmap_idx = (number / 8) as usize;
        let bitmap_off = (number % 8) as u8;

        if bitmap_idx >= self.bitmap.len() {
            return false;
        }

        let bit = self.bitmap[bitmap_idx];
        (bit & (1u8 << bitmap_off)) != 0
    }

    pub fn iterate_files<F>(&self, mut f: F)
    where
        F: FnMut(&NtfsFile),
    {
        for number in FIRST_NORMAL_RECORD..self.max_record {
            if self.record_exists(number) {
                if let Some(file) = self.get_record(number) {
                    if file.is_used() {
                        f(&file);
                    }
                }
            }
        }
    }

    pub fn get_record_data(&self, number: u64) -> &[u8] {
        let start = number as usize * self.volume.file_record_size as usize;
        let end = start + self.volume.file_record_size as usize;
        &self.data[start..end]
    }

    pub fn get_record(&self, number: u64) -> Option<NtfsFile<'_>> {
        if number >= self.max_record {
            return None;
        }
        let data = self.get_record_data(number);

        if NtfsFile::is_valid(data) {
            return Some(NtfsFile::new(number, data));
        }

        None
    }

    pub fn get_record_fs<R>(
        fs: &mut R,
        file_record_size: usize,
        position: u64,
    ) -> NtfsReaderResult<Vec<u8>>
    where
        R: Seek + Read,
    {
        let mut data = vec![0; file_record_size];
        fs.seek(SeekFrom::Start(position))?;
        fs.read_exact(&mut data)?;

        if !NtfsFile::is_valid(&data) {
            return Err(NtfsReaderError::InvalidMftRecord { position });
        }
        Self::fixup_record(0, &mut data)?;
        Ok(data)
    }

    pub fn read_data_fs<R>(
        volume: &Volume,
        reader: &mut R,
        record: &[u8],
        attribute_type: NtfsAttributeType,
    ) -> NtfsReaderResult<Option<Vec<u8>>>
    where
        R: Seek + Read,
    {
        let header = unsafe { &*(record.as_ptr() as *const NtfsFileRecordHeader) };
        let mut att_offset = header.attributes_offset as usize;
        let used = usize::min(header.used_size as usize, record.len());

        // First pass: look for the attribute directly in this record
        while att_offset < used {
            let slice = &record[att_offset..used];
            let attr = match NtfsAttribute::new(slice) {
                Some(attr) => attr,
                None => break,
            };

            if attr.header.type_id == NtfsAttributeType::End as u32 {
                break;
            }

            if attr.header.type_id == attribute_type as u32 {
                return Ok(Some(Self::read_attribute_data(reader, &attr, volume)?));
            }

            let attr_len = attr.len();
            if attr_len == 0 {
                break;
            }
            att_offset = match att_offset.checked_add(attr_len) {
                Some(next) if next <= used => next,
                _ => break,
            };
        }

        // Second pass: if not found, check attribute list entries
        att_offset = header.attributes_offset as usize;
        while att_offset < used {
            let slice = &record[att_offset..used];
            let attr = match NtfsAttribute::new(slice) {
                Some(attr) => attr,
                None => break,
            };

            if attr.header.type_id == NtfsAttributeType::End as u32 {
                break;
            }

            if attr.header.type_id == NtfsAttributeType::AttributeList as u32 {
                let att_list_data = if attr.header.is_non_resident != 0 {
                    Self::read_attribute_data(reader, &attr, volume)?
                } else {
                    match attr.as_resident_data() {
                        Some(data) => data.to_vec(),
                        None => break,
                    }
                };

                let mut list_offset = 0usize;

                while list_offset < att_list_data.len() {
                    let entry_slice = &att_list_data[list_offset..];
                    let entry = match parse_attribute_list_entry(entry_slice) {
                        Some(entry) => entry,
                        None => break,
                    };

                    let type_id = entry.type_id;
                    let reference = entry.reference();
                    let entry_len = entry.length as usize;

                    if type_id == attribute_type as u32 {
                        let record_position =
                            volume.mft_position + (reference * volume.file_record_size);
                        if let Ok(target_record) = Self::get_record_fs(
                            reader,
                            volume.file_record_size as usize,
                            record_position,
                        ) {
                            let target_header = unsafe {
                                &*(target_record.as_ptr() as *const NtfsFileRecordHeader)
                            };
                            let mut target_offset = target_header.attributes_offset as usize;
                            let target_used =
                                usize::min(target_header.used_size as usize, target_record.len());

                            while target_offset < target_used {
                                let target_slice = &target_record[target_offset..target_used];
                                let target_attr = match NtfsAttribute::new(target_slice) {
                                    Some(attr) => attr,
                                    None => break,
                                };

                                if target_attr.header.type_id == NtfsAttributeType::End as u32 {
                                    break;
                                }

                                if target_attr.header.type_id == attribute_type as u32 {
                                    return Ok(Some(Self::read_attribute_data(
                                        reader,
                                        &target_attr,
                                        volume,
                                    )?));
                                }

                                let len = target_attr.len();
                                if len == 0 {
                                    break;
                                }
                                target_offset = match target_offset.checked_add(len) {
                                    Some(next) if next <= target_used => next,
                                    _ => break,
                                };
                            }
                        }
                    }

                    if entry_len == 0 {
                        break;
                    }
                    list_offset = match list_offset.checked_add(entry_len) {
                        Some(next) if next <= att_list_data.len() => next,
                        _ => break,
                    };
                    let align = (8 - (list_offset % 8)) % 8;
                    list_offset = match list_offset.checked_add(align) {
                        Some(next) if next <= att_list_data.len() => next,
                        _ => break,
                    };
                }
            }

            let attr_len = attr.len();
            if attr_len == 0 {
                break;
            }
            att_offset = match att_offset.checked_add(attr_len) {
                Some(next) if next <= used => next,
                _ => break,
            };
        }

        Ok(None)
    }

    fn read_attribute_data<R>(
        reader: &mut R,
        att: &NtfsAttribute,
        volume: &Volume,
    ) -> NtfsReaderResult<Vec<u8>>
    where
        R: Seek + Read,
    {
        if att.header.is_non_resident == 0 {
            let data = att
                .as_resident_data()
                .ok_or(NtfsReaderError::InvalidDataRun {
                    details: "resident attribute missing value",
                })?;
            Ok(data.to_vec())
        } else {
            let (size, runs) = att.get_nonresident_data_runs(volume)?;
            let total_size =
                usize::try_from(size).map_err(|_| NtfsReaderError::InvalidDataRun {
                    details: "attribute size exceeds addressable memory",
                })?;

            let mut data = Vec::with_capacity(total_size);
            let mut buffer = Vec::new();
            let mut copied = 0usize;

            for run in runs.iter() {
                if copied >= total_size {
                    break;
                }

                match run {
                    DataRun::Data { lcn, length } => {
                        let run_len = usize::try_from(*length).map_err(|_| {
                            NtfsReaderError::InvalidDataRun {
                                details: "run length exceeds addressable memory",
                            }
                        })?;
                        let buf_size = usize::min(run_len, total_size - copied);
                        buffer.resize(buf_size, 0u8);

                        reader.seek(SeekFrom::Start(*lcn))?;
                        reader.read_exact(&mut buffer)?;

                        data.extend_from_slice(&buffer);
                        copied += buf_size;
                    }
                    DataRun::Sparse { length } => {
                        let run_len = usize::try_from(*length).map_err(|_| {
                            NtfsReaderError::InvalidDataRun {
                                details: "run length exceeds addressable memory",
                            }
                        })?;
                        let buf_size = usize::min(run_len, total_size - copied);
                        data.resize(data.len() + buf_size, 0);
                        copied += buf_size;
                    }
                }
            }

            Ok(data)
        }
    }

    fn fixup_record(record_number: u64, data: &mut [u8]) -> NtfsReaderResult<()> {
        if data.len() < core::mem::size_of::<NtfsFileRecordHeader>() {
            return Err(NtfsReaderError::CorruptMftRecord {
                number: record_number,
            });
        }
        let header =
            unsafe { core::ptr::read_unaligned(data.as_ptr() as *const NtfsFileRecordHeader) };

        let usn_start = header.update_sequence_offset as usize;
        if usn_start + 2 > data.len() {
            return Err(NtfsReaderError::CorruptMftRecord {
                number: record_number,
            });
        }
        let usa_start = usn_start + 2;
        let usa_end =
            usn_start.saturating_add((header.update_sequence_length as usize).saturating_mul(2));
        if usa_end > data.len() {
            return Err(NtfsReaderError::CorruptMftRecord {
                number: record_number,
            });
        }

        let usn0 = data[usn_start];
        let usn1 = data[usn_start + 1];

        let mut sector_off = SECTOR_SIZE - 2;
        for usa_off in (usa_start..usa_end).step_by(2) {
            if sector_off + 2 > data.len() {
                break;
            }

            let mut usa = [0u8; 2];
            usa.copy_from_slice(&data[usa_off..usa_off + 2]);

            let d0 = data[sector_off];
            let d1 = data[sector_off + 1];
            if d0 != usn0 || d1 != usn1 {
                return Err(NtfsReaderError::CorruptMftRecord {
                    number: record_number,
                });
            }

            data[sector_off..sector_off + 2].copy_from_slice(&usa);
            sector_off += SECTOR_SIZE;
        }
        Ok(())
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
