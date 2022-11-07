// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use std::{path::PathBuf, time::SystemTime};

use chrono::{DateTime, Utc};
use time::OffsetDateTime;

use crate::{
    api::{ntfs_to_unix_time, NtfsAttributeType, ROOT_RECORD},
    file::NtfsFile,
    mft::{Mft, MftCache},
};

#[derive(Default)]
pub struct FileInformation {
    pub name: String,
    pub path: PathBuf,
    pub is_directory: bool,
    pub size: u64,
    pub created: DateTime<Utc>,
    pub accessed: DateTime<Utc>,
    pub modified: DateTime<Utc>,
}

impl FileInformation {
    pub fn new(mft: &Mft, file: &NtfsFile, cache: Option<&mut MftCache>) -> Self {
        let mut info = FileInformation::default();

        let mut accessed = None;
        let mut created = None;
        let mut modified = None;

        file.attributes(|att| {
            if att.header.type_id == NtfsAttributeType::StandardInformation as u32 {
                let stdinfo = att.as_standard_info();

                accessed = Some(ntfs_to_unix_time(stdinfo.access_time));
                created = Some(ntfs_to_unix_time(stdinfo.creation_time));
                modified = Some(ntfs_to_unix_time(stdinfo.modification_time));
            }

            if att.header.type_id == NtfsAttributeType::Data as u32 {
                if att.header.is_non_resident == 0 {
                    info.size = att.header_res.value_length as u64;
                } else {
                    info.size = att.header_nonres.data_size;
                }
            }
        });

        info.created = SystemTime::from(created.unwrap_or(OffsetDateTime::UNIX_EPOCH)).into();
        info.accessed = SystemTime::from(accessed.unwrap_or(OffsetDateTime::UNIX_EPOCH)).into();
        info.modified = SystemTime::from(modified.unwrap_or(OffsetDateTime::UNIX_EPOCH)).into();

        info.is_directory = file.is_directory();

        info.compute_path(mft, file, cache);

        info
    }

    fn compute_path(&mut self, mft: &Mft, file: &NtfsFile, cache: Option<&mut MftCache>) {
        let mut next_parent;

        if let Some(name) = file.get_best_file_name() {
            self.name = name.to_string();
            next_parent = name.parent();
        } else {
            return;
        }

        let mut components = Vec::new();
        let mut cached_path = None;
        loop {
            if next_parent == ROOT_RECORD {
                break;
            }

            // Cache hit?
            if let Some(cache) = cache.as_ref() {
                if let Some(cur_path) = cache.get(&next_parent) {
                    cached_path = Some(cur_path);
                    break;
                }
            }

            let cur_file = mft.get_record(next_parent);
            if cur_file.is_none() {
                return;
            }
            let cur_file = cur_file.unwrap();

            if let Some(cur_name_att) = cur_file.get_best_file_name() {
                let cur_name = cur_name_att.to_string();
                components.push((cur_file.number(), PathBuf::from(cur_name)));
                next_parent = cur_name_att.parent();
            } else {
                return;
            }
        }

        let mut path = PathBuf::from(cached_path.unwrap_or(&mft.volume.path));

        if let Some(cache) = cache {
            for (number, comp) in components.iter().rev() {
                path.push(comp);
                cache.insert(*number, path.clone());
            }

            path.push(&self.name);
            cache.insert(file.number, path.clone());
        } else {
            for (_, comp) in components.iter().rev() {
                path.push(comp);
            }

            path.push(&self.name);
        }

        self.path = path;
    }
}
