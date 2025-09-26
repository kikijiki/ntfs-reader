// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use std::mem::size_of;

use crate::{
    api::*,
    errors::{NtfsReaderError, NtfsReaderResult},
    volume::Volume,
};

#[derive(Debug, Clone)]
pub enum DataRun {
    Data { lcn: u64, length: u64 },
    Sparse { length: u64 },
}

pub struct NtfsAttribute<'a> {
    data: &'a [u8],
    pub header: &'a NtfsAttributeHeader,
    length: usize,
}

impl<'a> NtfsAttribute<'a> {
    pub fn new(data: &'a [u8]) -> Option<Self> {
        if data.len() < size_of::<NtfsAttributeHeader>() {
            return None;
        }

        let header = unsafe { &*(data.as_ptr() as *const NtfsAttributeHeader) };
        let length = header.length as usize;
        if length == 0 || length > data.len() {
            return None;
        }

        Some(Self {
            data,
            header,
            length,
        })
    }

    pub fn len(&self) -> usize {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn data(&self) -> &'a [u8] {
        &self.data[..self.length]
    }

    pub fn resident_header(&self) -> Option<&'a NtfsResidentAttributeHeader> {
        if self.header.is_non_resident != 0 {
            return None;
        }
        if self.length < size_of::<NtfsResidentAttributeHeader>() {
            return None;
        }
        Some(unsafe { &*(self.data.as_ptr() as *const NtfsResidentAttributeHeader) })
    }

    pub fn nonresident_header(&self) -> Option<&'a NtfsNonResidentAttributeHeader> {
        if self.header.is_non_resident == 0 {
            return None;
        }
        if self.length < size_of::<NtfsNonResidentAttributeHeader>() {
            return None;
        }
        Some(unsafe { &*(self.data.as_ptr() as *const NtfsNonResidentAttributeHeader) })
    }

    pub fn get_resident(&self) -> Option<&'a [u8]> {
        let header = self.resident_header()?;
        let start = header.value_offset as usize;
        let value_length = header.value_length as usize;
        let end = start.checked_add(value_length)?;
        if end > self.data().len() {
            return None;
        }
        Some(&self.data()[start..end])
    }

    pub fn as_standard_info(&self) -> Option<&'a NtfsStandardInformation> {
        if self.header.type_id != NtfsAttributeType::StandardInformation as u32 {
            return None;
        }
        let slice = self.get_resident()?;
        if slice.len() < size_of::<NtfsStandardInformation>() {
            return None;
        }
        Some(unsafe { &*(slice.as_ptr() as *const NtfsStandardInformation) })
    }

    pub fn as_name(&self) -> Option<NtfsFileName> {
        if self.header.type_id != NtfsAttributeType::FileName as u32 {
            return None;
        }
        let slice = self.get_resident()?;
        if slice.len() < size_of::<NtfsFileNameHeader>() {
            return None;
        }

        let header = unsafe { *(slice.as_ptr() as *const NtfsFileNameHeader) };
        let name_bytes = (header.name_length as usize).checked_mul(2)?;
        let header_size = size_of::<NtfsFileNameHeader>();
        let end = header_size.checked_add(name_bytes)?;
        if end > slice.len() {
            return None;
        }

        let char_count = header.name_length as usize;
        if char_count > 255 {
            return None;
        }

        let mut data = [0u16; 255];
        if char_count > 0 {
            let bytes = &slice[header_size..end];
            for (i, slot) in data.iter_mut().take(char_count).enumerate() {
                let byte_index = i * 2;
                *slot = u16::from_le_bytes([bytes[byte_index], bytes[byte_index + 1]]);
            }
        }

        Some(NtfsFileName { header, data })
    }

    pub fn as_resident_data(&self) -> Option<&'a [u8]> {
        if self.header.type_id != NtfsAttributeType::Data as u32 {
            return None;
        }
        self.get_resident()
    }

    pub fn get_nonresident_data_runs(
        &self,
        volume: &Volume,
    ) -> NtfsReaderResult<(u64, Vec<DataRun>)> {
        let header_nonres = self
            .nonresident_header()
            .ok_or(NtfsReaderError::InvalidDataRun {
                details: "attribute is resident",
            })?;

        let mut out = Vec::new();

        let total_size = header_nonres.data_size;
        if total_size == 0 {
            return Ok((total_size, out));
        }

        let start = header_nonres.data_runs_offset as usize;
        if start > self.length {
            return Err(NtfsReaderError::InvalidDataRun {
                details: "data runs offset outside attribute",
            });
        }
        let runs_data = &self.data()[start..];

        let cluster_size = volume.cluster_size;
        const BUF_SIZE: usize = 8;

        let mut cursor = 0usize;
        let mut prev_lcn = 0i128;
        let mut total_run_length = 0u64;
        loop {
            if cursor >= runs_data.len() {
                return Err(NtfsReaderError::InvalidDataRun {
                    details: "unterminated data run sequence",
                });
            }
            if runs_data[cursor] == 0 {
                break;
            }

            let descriptor = runs_data[cursor];
            let cluster_count_b = (descriptor & 0x0f) as usize;
            let cluster_offset_b = ((descriptor & 0xf0) >> 4) as usize;

            if cluster_count_b == 0 || cluster_count_b > BUF_SIZE {
                return Err(NtfsReaderError::InvalidDataRun {
                    details: "invalid cluster count field",
                });
            }
            if cluster_offset_b > BUF_SIZE {
                return Err(NtfsReaderError::InvalidDataRun {
                    details: "invalid cluster offset field",
                });
            }

            cursor += 1;

            if cursor + cluster_count_b > runs_data.len() {
                return Err(NtfsReaderError::InvalidDataRun {
                    details: "unexpected end of run-length data",
                });
            }
            let mut count_buf = [0u8; BUF_SIZE];
            count_buf[..cluster_count_b]
                .copy_from_slice(&runs_data[cursor..cursor + cluster_count_b]);
            let cluster_count = u64::from_le_bytes(count_buf);
            if cluster_count == 0 {
                return Err(NtfsReaderError::InvalidDataRun {
                    details: "cluster count is zero",
                });
            }
            cursor += cluster_count_b;

            let run_length_bytes =
                cluster_count
                    .checked_mul(cluster_size)
                    .ok_or(NtfsReaderError::InvalidDataRun {
                        details: "run length overflow",
                    })?;
            total_run_length = total_run_length.checked_add(run_length_bytes).ok_or(
                NtfsReaderError::InvalidDataRun {
                    details: "total run length overflow",
                },
            )?;

            let lcn = if cluster_offset_b == 0 {
                None
            } else {
                if cursor + cluster_offset_b > runs_data.len() {
                    return Err(NtfsReaderError::InvalidDataRun {
                        details: "unexpected end of run-offset data",
                    });
                }
                let mut offset_buf = [0u8; BUF_SIZE];
                offset_buf[..cluster_offset_b]
                    .copy_from_slice(&runs_data[cursor..cursor + cluster_offset_b]);
                let raw = i64::from_le_bytes(offset_buf);
                let empty_bits = (BUF_SIZE - cluster_offset_b) * 8;
                let cluster_offset = (raw << empty_bits) >> empty_bits;
                cursor += cluster_offset_b;

                let delta = (cluster_offset as i128)
                    .checked_mul(cluster_size as i128)
                    .ok_or(NtfsReaderError::InvalidDataRun {
                        details: "relative offset overflow",
                    })?;
                let start = prev_lcn
                    .checked_add(delta)
                    .ok_or(NtfsReaderError::InvalidDataRun {
                        details: "relative offset overflow",
                    })?;
                if start < 0 {
                    return Err(NtfsReaderError::InvalidDataRun {
                        details: "relative offset underflow",
                    });
                }
                prev_lcn = start;
                Some(start as u64)
            };

            match lcn {
                Some(start) => out.push(DataRun::Data {
                    lcn: start,
                    length: run_length_bytes,
                }),
                None => out.push(DataRun::Sparse {
                    length: run_length_bytes,
                }),
            }
        }

        if total_size > 0 && out.is_empty() {
            return Err(NtfsReaderError::InvalidDataRun {
                details: "attribute has size but no runs",
            });
        }

        if total_run_length < total_size {
            return Err(NtfsReaderError::InvalidDataRun {
                details: "data runs shorter than declared size",
            });
        }

        Ok((total_size, out))
    }
}
