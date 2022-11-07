// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use std::ops::Range;

use crate::{api::*, volume::Volume};

pub struct NtfsAttribute<'a> {
    pub data: &'a [u8],
    pub header: &'a NtfsAttributeHeader,
    pub header_res: &'a NtfsResidentAttributeHeader,
    pub header_nonres: &'a NtfsNonResidentAttributeHeader,
}

impl<'a> NtfsAttribute<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        unsafe {
            NtfsAttribute {
                data,
                header: &*(data.as_ptr() as *const NtfsAttributeHeader),
                header_res: &*(data.as_ptr() as *const NtfsResidentAttributeHeader),
                header_nonres: &*(data.as_ptr() as *const NtfsNonResidentAttributeHeader),
            }
        }
    }

    pub fn get_resident(&self) -> &'a [u8] {
        assert!(self.header.is_non_resident == 0);
        let start = self.header_res.value_offset as usize;
        let end = start + self.header_res.value_length as usize;
        &self.data[start..end]
    }

    pub fn as_standard_info(&self) -> &'a NtfsStandardInformation {
        assert!(self.header.type_id == NtfsAttributeType::StandardInformation as u32);
        let slice = self.get_resident();
        unsafe { &*(slice.as_ptr() as *const NtfsStandardInformation) }
    }

    pub fn as_name(&self) -> &'a NtfsFileName {
        assert!(self.header.type_id == NtfsAttributeType::FileName as u32);
        let slice = self.get_resident();
        unsafe { &*(slice.as_ptr() as *const NtfsFileName) }
    }

    pub fn as_resident_data(&self) -> &'a [u8] {
        assert!(self.header.type_id == NtfsAttributeType::Data as u32);
        self.get_resident()
    }

    pub fn get_nonresident_data_runs(&self, volume: &Volume) -> (usize, Vec<Range<usize>>) {
        let mut out = Vec::new();

        let total_size = self.header_nonres.data_size as usize;
        if total_size == 0 {
            return (total_size, out);
        }

        let runs_data = {
            let start = self.header_nonres.data_runs_offset as usize;
            let end = start + self.header.length as usize;
            &self.data[start..end]
        };

        let cluster_size = volume.cluster_size as usize;
        const BUF_SIZE: usize = 8;

        let mut cursor = 0usize;
        let mut prev_run = 0usize;
        loop {
            if runs_data[cursor] == 0 {
                break;
            }

            // How many bytes to read for each value.
            let cluster_count_b = (runs_data[cursor] & 0x0f) as usize;
            let cluster_offset_b = ((runs_data[cursor] & 0xf0) >> 4) as usize;
            assert!(cluster_count_b > 0 && cluster_count_b <= BUF_SIZE);
            assert!(cluster_offset_b > 0 && cluster_offset_b <= BUF_SIZE);

            cursor += 1;

            // Read cluster_count
            let mut buf = [0u8; BUF_SIZE];
            buf[..cluster_count_b].copy_from_slice(&runs_data[cursor..cursor + cluster_count_b]);
            let cluster_count = u64::from_le_bytes(buf) as usize;
            cursor += cluster_count_b;

            // Read cluster_offset (and fix sign bits)
            let mut buf = [0u8; BUF_SIZE];
            buf[..cluster_offset_b].copy_from_slice(&runs_data[cursor..cursor + cluster_offset_b]);
            let cluster_offset = i64::from_le_bytes(buf);
            let empty_bits = (BUF_SIZE - cluster_offset_b) * 8;
            let cluster_offset = (cluster_offset << empty_bits) >> empty_bits;
            cursor += cluster_offset_b;

            let start = if cluster_offset >= 0 {
                prev_run + (cluster_offset as usize) * cluster_size
            } else {
                prev_run - (cluster_offset.wrapping_neg() as usize) * cluster_size
            };

            let end = start + cluster_count * cluster_size;
            prev_run = start;

            out.push(start..end);
        }

        (total_size, out)
    }
}
