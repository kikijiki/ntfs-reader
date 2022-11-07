// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use std::path::{Path, PathBuf};

use binread::BinReaderExt;

use crate::{aligned_reader::open_volume, api::*};

#[derive(Clone)]
pub struct Volume {
    pub path: PathBuf,
    pub boot_sector: BootSector,
    pub cluster_size: u64,
    pub volume_size: u64,
    pub file_record_size: u64,
    pub mft_position: u64,
}

impl Volume {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let mut reader = open_volume(path.as_ref())?;
        let boot_sector = reader.read_le::<BootSector>()?;

        let cluster_size = boot_sector.sectors_per_cluster as u64 * boot_sector.sector_size as u64;
        let volume_size = boot_sector.total_sectors as u64 * boot_sector.sector_size as u64;
        let file_record_size = {
            if boot_sector.file_record_size_info > 0 {
                boot_sector.file_record_size_info as u64
            } else {
                1u64 << (-boot_sector.file_record_size_info) as u64
            }
        };
        let mft_position = boot_sector.mft_lcn * cluster_size;

        Ok(Volume {
            path: path.as_ref().into(),
            boot_sector,
            cluster_size,
            volume_size,
            file_record_size,
            mft_position,
        })
    }
}
