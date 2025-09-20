// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use std::io::{Read, Seek, SeekFrom};

use crate::{
    aligned_reader::open_volume,
    api::*,
    attribute::NtfsAttribute,
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
        );

        let mut data =
            Self::read_data_fs(&volume, &mut reader, &mft_record, NtfsAttributeType::Data)
                .ok_or_else(|| NtfsReaderError::MissingMftAttribute("Data".to_string()))?;
        let bitmap =
            Self::read_data_fs(&volume, &mut reader, &mft_record, NtfsAttributeType::Bitmap)
                .ok_or_else(|| NtfsReaderError::MissingMftAttribute("Bitmap".to_string()))?;

        let max_record = (data.len() / volume.file_record_size as usize) as u64;

        // Is this even worth the extra time?
        // let max_bit = bitmap
        //     .iter()
        //     .enumerate()
        //     .filter(|x| *x.1 != 0)
        //     .max_by_key(|x| x.0)
        //     .unwrap_or((0, &0));
        // let max_record_bitmap = max_bit.0 as u64 * 8 + *max_bit.1 as u64;
        // let max_record_mft = (data.len() / volume.file_record_size as usize) as u64;
        // let max_record = u64::min(max_record_bitmap, max_record_mft);

        // Fixup all records so we are non mutable from now on.
        for number in 0..max_record {
            let start = number as usize * volume.file_record_size as usize;
            let end = start + volume.file_record_size as usize;
            let data = &mut data[start..end];
            Self::fixup_record(data);
        }

        Ok(Mft {
            volume,
            data,
            bitmap,
            max_record,
        })
    }

    pub fn record_exists(&self, number: u64) -> bool {
        if number > self.max_record {
            return false;
        }

        let bitmap_idx = (number / 8) as usize;
        let bitmap_off = number % 8;

        if bitmap_idx >= self.bitmap.len() {
            return false;
        }

        let bit = self.bitmap[bitmap_idx];
        bit & (1 << bitmap_off) != 0
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

    pub fn get_record_fs<R>(fs: &mut R, file_record_size: usize, position: u64) -> Vec<u8>
    where
        R: Seek + Read,
    {
        let mut data = vec![0; file_record_size];
        let _ = fs.seek(SeekFrom::Start(position));
        let _ = fs.read_exact(&mut data);

        if NtfsFile::is_valid(&data) {
            Self::fixup_record(&mut data);
            data
        } else {
            Vec::new()
        }
    }

    pub fn read_data_fs<R>(
        volume: &Volume,
        reader: &mut R,
        record: &[u8],
        attribute_type: NtfsAttributeType,
    ) -> Option<Vec<u8>>
    where
        R: Seek + Read,
    {
        let header = unsafe { &*(record.as_ptr() as *const NtfsFileRecordHeader) };
        let mut att_offset = header.attributes_offset as usize;

        // First pass: look for the attribute directly in this record
        loop {
            if att_offset >= header.used_size as usize {
                break;
            }

            let att = NtfsAttribute::new(&record[att_offset..]);
            if att.header.type_id == NtfsAttributeType::End as u32 {
                break;
            }

            if att.header.type_id == attribute_type as u32 {
                return Some(Self::read_attribute_data(reader, &att, volume));
            }

            att_offset += att.header.length as usize;
        }

        // Second pass: if not found, check attribute list entries
        att_offset = header.attributes_offset as usize;
        loop {
            if att_offset >= header.used_size as usize {
                break;
            }

            let att = NtfsAttribute::new(&record[att_offset..]);
            if att.header.type_id == NtfsAttributeType::End as u32 {
                break;
            }

            if att.header.type_id == NtfsAttributeType::AttributeList as u32 {
                let att_list_data = if att.header.is_non_resident != 0 {
                    Self::read_attribute_data(reader, &att, volume)
                } else {
                    let header = unsafe {
                        &*(record[att_offset..].as_ptr() as *const NtfsResidentAttributeHeader)
                    };
                    let value_length = header.value_length;
                    record[att_offset + header.value_offset as usize
                        ..att_offset + header.value_offset as usize + value_length as usize]
                        .to_vec()
                };

                let mut list_offset = 0;

                while list_offset < att_list_data.len() {
                    let entry = unsafe {
                        &*(att_list_data[list_offset..].as_ptr() as *const NtfsAttributeListEntry)
                    };

                    let type_id = entry.type_id;
                    let reference = entry.reference();

                    if type_id == attribute_type as u32 {
                        let record_position =
                            volume.mft_position + (reference * volume.file_record_size);
                        let target_record = Self::get_record_fs(
                            reader,
                            volume.file_record_size as usize,
                            record_position,
                        );

                        if !target_record.is_empty() {
                            // Find the attribute directly in the target record
                            let header = unsafe {
                                &*(target_record.as_ptr() as *const NtfsFileRecordHeader)
                            };
                            let mut att_offset = header.attributes_offset as usize;

                            loop {
                                if att_offset >= header.used_size as usize {
                                    break;
                                }

                                let att = NtfsAttribute::new(&target_record[att_offset..]);
                                if att.header.type_id == NtfsAttributeType::End as u32 {
                                    break;
                                }

                                if att.header.type_id == attribute_type as u32 {
                                    return Some(Self::read_attribute_data(reader, &att, volume));
                                }

                                att_offset += att.header.length as usize;
                            }
                        }
                    }

                    list_offset += entry.length as usize;
                    // Align to 8 bytes
                    list_offset += (8 - (list_offset % 8)) % 8;
                }
            }

            att_offset += att.header.length as usize;
        }

        None
    }

    fn read_attribute_data<R>(reader: &mut R, att: &NtfsAttribute, volume: &Volume) -> Vec<u8>
    where
        R: Seek + Read,
    {
        let mut data = Vec::<u8>::new();

        if att.header.is_non_resident == 0 {
            data.copy_from_slice(att.as_resident_data());
        } else {
            let mut buffer = Vec::new();
            let (size, runs) = att.get_nonresident_data_runs(volume);
            data.reserve_exact(size);
            let mut copied = 0usize;

            for run in runs.iter() {
                if copied >= size {
                    break;
                }

                let buf_size = usize::min(run.len(), size - copied);
                buffer.resize(buf_size, 0u8);

                let _ = reader.seek(SeekFrom::Start(run.start as u64));
                let _ = reader.read_exact(&mut buffer);

                data.append(&mut buffer.clone());
                copied += buf_size;
            }
        }

        data
    }

    fn fixup_record(data: &mut [u8]) {
        let header = unsafe { &*(data.as_ptr() as *const NtfsFileRecordHeader) };

        // Fixup
        let usn_start = header.update_sequence_offset as usize;
        //let usn_end = usn_start + 2;
        let usa_start = usn_start + 2;
        let usa_end = usn_start + header.update_sequence_length as usize * 2;

        //let mut usn = [0u8; 2];
        //usn.copy_from_slice(&data[usn_start..usn_end]);

        let mut sector_off = SECTOR_SIZE - 2;
        for usa_off in (usa_start..usa_end).step_by(2) {
            let mut usa = [0u8; 2];
            usa.clone_from_slice(&data[usa_off..usa_off + 2]);

            //let dst = &data[sector_off..sector_off + 2];
            //if dst != usn {
            //    return false;
            //}

            data[sector_off..sector_off + 2].copy_from_slice(&usa);
            sector_off += SECTOR_SIZE;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        errors::NtfsReaderResult, mft::Mft, test_utils::TEST_VOLUME_LETTER, volume::Volume,
    };

    #[test]
    fn test_mft_creation() -> NtfsReaderResult<()> {
        let vol = Volume::new(format!("\\\\.\\{}:", TEST_VOLUME_LETTER))?;
        let mft = Mft::new(vol)?;

        assert!(mft.max_record > 0);
        assert!(!mft.data.is_empty());
        assert!(!mft.bitmap.is_empty());

        Ok(())
    }

    #[test]
    fn test_record_exists() -> NtfsReaderResult<()> {
        let vol = Volume::new(format!("\\\\.\\{}:", TEST_VOLUME_LETTER))?;
        let mft = Mft::new(vol)?;

        // MFT record (0) should always exist
        assert!(mft.record_exists(0));

        // Test out of bounds
        assert!(!mft.record_exists(u64::MAX));
        assert!(!mft.record_exists(mft.max_record + 1));

        Ok(())
    }

    #[test]
    fn test_get_record() -> NtfsReaderResult<()> {
        let vol = Volume::new(format!("\\\\.\\{}:", TEST_VOLUME_LETTER))?;
        let mft = Mft::new(vol)?;

        // MFT record (0) should be retrievable
        let record = mft.get_record(0);
        assert!(record.is_some());

        // Invalid record number should return None
        let invalid = mft.get_record(mft.max_record + 1);
        assert!(invalid.is_none());

        Ok(())
    }

    #[test]
    fn test_iterate_files() -> NtfsReaderResult<()> {
        let vol = Volume::new(format!("\\\\.\\{}:", TEST_VOLUME_LETTER))?;
        let mft = Mft::new(vol)?;

        let mut count = 0;
        mft.iterate_files(|_file| {
            count += 1;
        });

        assert!(count > 0, "Should iterate over at least some files");

        Ok(())
    }
}
