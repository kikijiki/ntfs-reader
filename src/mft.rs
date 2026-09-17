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
    extension_records: Vec<(u64, u64)>,
}

impl Mft {
    pub fn new(volume: Volume) -> NtfsReaderResult<Self> {
        let mut reader = open_volume(&volume.path)?;

        let mft_record =
            Self::get_record_fs(&mut reader, volume.file_record_size, volume.mft_position)?;

        let mut data =
            Self::read_data_fs(&volume, &mut reader, &mft_record, NtfsAttributeType::Data)?
                .ok_or_else(|| NtfsReaderError::MissingMftAttribute("Data".to_string()))?;
        let bitmap =
            Self::read_data_fs(&volume, &mut reader, &mft_record, NtfsAttributeType::Bitmap)?
                .ok_or_else(|| NtfsReaderError::MissingMftAttribute("Bitmap".to_string()))?;

        let max_record = data.len() as u64 / volume.file_record_size;

        // Fixup all records so we are non mutable from now on.
        for number in 0..max_record {
            let start = number * volume.file_record_size;
            let end = start + volume.file_record_size;
            let (start, end) = (start as usize, end as usize);
            let data = &mut data[start..end];
            Self::fixup_record(number, data)?;
        }

        let mut mft = Mft {
            volume,
            data,
            bitmap,
            max_record,
            extension_records: Vec::new(),
        };
        mft.index_extension_records();
        Ok(mft)
    }

    pub fn record_exists(&self, number: u64) -> bool {
        if number >= self.max_record {
            return false;
        }

        let bitmap_idx = number / 8;
        let bitmap_off = (number % 8) as u8;

        if bitmap_idx >= self.bitmap.len() as u64 {
            return false;
        }

        let bit = self.bitmap[bitmap_idx as usize];
        (bit & (1u8 << bitmap_off)) != 0
    }

    pub fn files(&self) -> impl Iterator<Item = NtfsFile<'_>> {
        (FIRST_NORMAL_RECORD..self.max_record)
            .filter(|&n| self.record_exists(n))
            .filter_map(|n| self.get_record(n))
            .filter(|f| f.is_used() && !f.is_extension())
    }

    /// Returns the base MFT record and all live extension records that belong
    /// to the same logical file.
    pub fn file_records<'a>(
        &'a self,
        file: &NtfsFile<'_>,
    ) -> impl Iterator<Item = NtfsFile<'a>> + 'a {
        let base_number = file.base_record_number().unwrap_or(file.number());
        let base_reference = self
            .get_record(base_number)
            .map(|base| base.reference_number());
        let start = self
            .extension_records
            .partition_point(|(base, _)| *base < base_number);
        let end = self
            .extension_records
            .partition_point(|(base, _)| *base <= base_number);

        std::iter::once(base_number)
            .chain(
                self.extension_records[start..end]
                    .iter()
                    .map(|(_, extension)| *extension),
            )
            .filter(move |number| self.record_exists(*number))
            .filter_map(move |number| self.get_record(number))
            .filter(move |record| {
                record.is_used()
                    && (record.number() == base_number
                        || record.base_reference_number() == base_reference)
            })
    }

    #[deprecated(since = "0.4.5", note = "use `files()` iterator instead")]
    pub fn iterate_files<F>(&self, mut f: F)
    where
        F: FnMut(&NtfsFile),
    {
        for file in self.files() {
            f(&file);
        }
    }

    fn get_record_data(&self, number: u64) -> &[u8] {
        let start = number * self.volume.file_record_size;
        let end = start + self.volume.file_record_size;
        &self.data[start as usize..end as usize]
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

    fn index_extension_records(&mut self) {
        self.extension_records = (0..self.max_record)
            .filter(|&number| self.record_exists(number))
            .filter_map(|number| {
                let record = self.get_record(number)?;
                if !record.is_used() {
                    return None;
                }
                record
                    .base_record_number()
                    .map(|base| (base, record.number()))
            })
            .collect();
        self.extension_records.sort_unstable();
    }

    pub fn get_record_fs<R>(
        fs: &mut R,
        file_record_size: u64,
        position: u64,
    ) -> NtfsReaderResult<Vec<u8>>
    where
        R: Seek + Read,
    {
        let mut data = vec![0; file_record_size as usize];
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
                        if let Ok(target_record) =
                            Self::get_record_fs(reader, volume.file_record_size, record_position)
                        {
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
                usize::try_from(size).map_err(|_| NtfsReaderError::AllocationTooLarge { size })?;

            let mut data = Vec::new();
            data.try_reserve(total_size)
                .map_err(|_| NtfsReaderError::AllocationTooLarge { size })?;
            let mut copied = 0u64;

            for run in runs.iter() {
                if copied >= size {
                    break;
                }

                let buf_size = match run {
                    DataRun::Data { lcn, length } => {
                        let buf_size = u64::min(*length, size - copied);
                        let start = data.len();
                        data.resize(start + buf_size as usize, 0u8);

                        reader.seek(SeekFrom::Start(*lcn))?;
                        reader.read_exact(&mut data[start..])?;
                        buf_size
                    }
                    DataRun::Sparse { length } => {
                        let buf_size = u64::min(*length, size - copied);
                        data.resize(data.len() + buf_size as usize, 0);
                        buf_size
                    }
                };
                copied += buf_size;
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::file::NtfsNameKind;
    use crate::file_info::FileInfo;

    const RECORD_SIZE: usize = 1024;
    const ATTRIBUTES_OFFSET: usize = 48;

    #[test]
    fn combines_base_and_extension_records_into_one_file() {
        let attributes = NtfsFileNameFlags::Hidden as u32
            | NtfsFileNameFlags::System as u32
            | NtfsFileNameFlags::Archive as u32
            | NtfsFileNameFlags::SparseFile as u32;
        let file_size = 34_359_738_368u64;

        let mut base = new_record(24, 7, 0);
        let mut offset = ATTRIBUTES_OFFSET;
        offset = add_standard_information(&mut base, offset, attributes);
        offset = add_nonresident_data(&mut base, offset, file_size);
        finish_record(&mut base, offset);

        let base_reference = (7u64 << 48) | 24;
        let mut extension = new_record(25, 3, base_reference);
        let offset = add_file_name(
            &mut extension,
            ATTRIBUTES_OFFSET,
            "large-fragmented.rar",
            attributes,
        );
        finish_record(&mut extension, offset);

        let mut data = vec![0u8; 26 * RECORD_SIZE];
        data[24 * RECORD_SIZE..25 * RECORD_SIZE].copy_from_slice(&base);
        data[25 * RECORD_SIZE..26 * RECORD_SIZE].copy_from_slice(&extension);

        let boot_sector = unsafe { std::mem::zeroed() };
        let volume = Volume {
            path: PathBuf::from(r"\\.\T:"),
            boot_sector,
            cluster_size: 4096,
            volume_size: 0,
            file_record_size: RECORD_SIZE as u64,
            mft_position: 0,
        };
        let mut mft = Mft {
            volume,
            data,
            bitmap: vec![0, 0, 0, 0b0000_0011],
            max_record: 26,
            extension_records: Vec::new(),
        };
        mft.index_extension_records();

        let files: Vec<_> = mft.files().collect();
        assert_eq!(files.len(), 1, "extension record must not be a second file");
        assert_eq!(files[0].number(), 24);
        assert_eq!(mft.file_records(&files[0]).count(), 2);

        let info = FileInfo::new(&mft, &files[0]);
        assert_eq!(info.name, "large-fragmented.rar");
        assert_eq!(info.size, file_size);
        assert_eq!(info.file_attributes, attributes);
    }

    #[test]
    fn all_file_names_separates_hard_links_from_dos_aliases() {
        let attributes = NtfsFileNameFlags::Archive as u32;
        // Different parent: a second real hard link.
        let other_parent = (2u64 << 48) | 100;

        let mut base = new_record(24, 1, 0);
        write_u16(&mut base, 18, 2); // link_count: two real hard links
        let mut offset = ATTRIBUTES_OFFSET;
        offset = add_file_name_ex(
            &mut base,
            offset,
            1,
            ROOT_RECORD,
            NtfsFileNamespace::Win32,
            "longfilename.txt",
            attributes,
        );
        offset = add_file_name_ex(
            &mut base,
            offset,
            2,
            ROOT_RECORD,
            NtfsFileNamespace::Dos,
            "LONGFI~1.TXT",
            attributes,
        );
        offset = add_file_name_ex(
            &mut base,
            offset,
            3,
            other_parent,
            NtfsFileNamespace::Posix,
            "secondlink.txt",
            attributes,
        );
        finish_record(&mut base, offset);

        let mut data = vec![0u8; 25 * RECORD_SIZE];
        data[24 * RECORD_SIZE..25 * RECORD_SIZE].copy_from_slice(&base);

        let boot_sector = unsafe { std::mem::zeroed() };
        let volume = Volume {
            path: PathBuf::from(r"\\.\T:"),
            boot_sector,
            cluster_size: 4096,
            volume_size: 0,
            file_record_size: RECORD_SIZE as u64,
            mft_position: 0,
        };
        let mut mft = Mft {
            volume,
            data,
            bitmap: vec![0, 0, 0, 0b0000_0001],
            max_record: 25,
            extension_records: Vec::new(),
        };
        mft.index_extension_records();

        let files: Vec<_> = mft.files().collect();
        assert_eq!(files.len(), 1);
        let file = &files[0];
        let link_count = file.header.link_count;

        let names = file.all_file_names(&mft);
        assert_eq!(names.len(), 3);

        let links: Vec<_> = names
            .iter()
            .filter(|entry| entry.kind == NtfsNameKind::Link)
            .collect();
        let aliases: Vec<_> = names
            .iter()
            .filter(|entry| entry.kind == NtfsNameKind::DosAlias)
            .collect();

        assert_eq!(links.len(), link_count as usize);
        assert_eq!(aliases.len(), 1);
        assert!(aliases[0].name.is_dos_alias());
        assert_eq!(aliases[0].name.to_string(), "LONGFI~1.TXT");

        let link_parents: Vec<u64> = links.iter().map(|entry| entry.name.parent()).collect();
        assert!(link_parents.contains(&ROOT_RECORD));
        assert!(link_parents.contains(&(other_parent & 0x0000_FFFF_FFFF_FFFF)));
    }

    fn new_record(number: u64, sequence: u16, base_reference: u64) -> Vec<u8> {
        let mut record = vec![0u8; RECORD_SIZE];
        record[0..4].copy_from_slice(FILE_RECORD_SIGNATURE);
        write_u16(&mut record, 4, 42);
        write_u16(&mut record, 6, 1);
        write_u16(&mut record, 16, sequence);
        write_u16(&mut record, 18, 1);
        write_u16(&mut record, 20, ATTRIBUTES_OFFSET as u16);
        write_u16(&mut record, 22, NtfsFileFlags::InUse as u16);
        write_u32(&mut record, 28, RECORD_SIZE as u32);
        write_u64(&mut record, 32, base_reference);
        write_u16(&mut record, 40, number as u16);
        record
    }

    fn add_standard_information(record: &mut [u8], offset: usize, attributes: u32) -> usize {
        let mut value = vec![0u8; size_of::<NtfsStandardInformation>()];
        write_u64(&mut value, 0, EPOCH_DIFFERENCE + 10_000_000);
        write_u64(&mut value, 8, EPOCH_DIFFERENCE + 20_000_000);
        write_u64(&mut value, 24, EPOCH_DIFFERENCE + 30_000_000);
        write_u32(&mut value, 32, attributes);
        add_resident_attribute(
            record,
            offset,
            NtfsAttributeType::StandardInformation,
            0,
            &value,
        )
    }

    fn add_file_name(record: &mut [u8], offset: usize, name: &str, attributes: u32) -> usize {
        add_file_name_ex(
            record,
            offset,
            1,
            ROOT_RECORD,
            NtfsFileNamespace::Win32,
            name,
            attributes,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn add_file_name_ex(
        record: &mut [u8],
        offset: usize,
        id: u16,
        parent: u64,
        namespace: NtfsFileNamespace,
        name: &str,
        attributes: u32,
    ) -> usize {
        let encoded: Vec<u16> = name.encode_utf16().collect();
        let mut value = vec![0u8; size_of::<NtfsFileNameHeader>() + encoded.len() * 2];
        write_u64(&mut value, 0, parent);
        write_u64(&mut value, 40, 34_359_738_368);
        write_u64(&mut value, 48, 34_359_738_368);
        write_u32(&mut value, 56, attributes);
        value[64] = encoded.len() as u8;
        value[65] = namespace as u8;
        for (index, character) in encoded.into_iter().enumerate() {
            write_u16(
                &mut value,
                size_of::<NtfsFileNameHeader>() + index * 2,
                character,
            );
        }
        add_resident_attribute(record, offset, NtfsAttributeType::FileName, id, &value)
    }

    fn add_resident_attribute(
        record: &mut [u8],
        offset: usize,
        attribute_type: NtfsAttributeType,
        id: u16,
        value: &[u8],
    ) -> usize {
        const VALUE_OFFSET: usize = 24;
        let length = align_to_eight(VALUE_OFFSET + value.len());
        write_u32(record, offset, attribute_type as u32);
        write_u32(record, offset + 4, length as u32);
        write_u16(record, offset + 14, id);
        write_u32(record, offset + 16, value.len() as u32);
        write_u16(record, offset + 20, VALUE_OFFSET as u16);
        record[offset + VALUE_OFFSET..offset + VALUE_OFFSET + value.len()].copy_from_slice(value);
        offset + length
    }

    fn add_nonresident_data(record: &mut [u8], offset: usize, size: u64) -> usize {
        let length = size_of::<NtfsNonResidentAttributeHeader>();
        write_u32(record, offset, NtfsAttributeType::Data as u32);
        write_u32(record, offset + 4, length as u32);
        record[offset + 8] = 1;
        write_u16(record, offset + 14, 2);
        write_u64(record, offset + 16, 0);
        write_u64(record, offset + 40, size);
        write_u64(record, offset + 48, size);
        write_u64(record, offset + 56, size);
        offset + length
    }

    fn finish_record(record: &mut [u8], used_size: usize) {
        write_u32(record, 24, used_size as u32);
    }

    fn align_to_eight(value: usize) -> usize {
        (value + 7) & !7
    }

    fn write_u16(data: &mut [u8], offset: usize, value: u16) {
        data[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u32(data: &mut [u8], offset: usize, value: u32) {
        data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u64(data: &mut [u8], offset: usize, value: u64) {
        data[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
}
