// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use std::{
    io::{Read, Seek, SeekFrom},
    time::Instant,
};

use tracing::info;

use crate::{
    aligned_reader::open_volume, api::*, attribute::NtfsAttribute, errors::NtfsReaderResult,
    file::NtfsFile, volume::Volume,
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
            Self::read_data_fs(&volume, &mut reader, &mft_record, NtfsAttributeType::Data);
        let bitmap =
            Self::read_data_fs(&volume, &mut reader, &mft_record, NtfsAttributeType::Bitmap);

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

    pub fn get_record(&self, number: u64) -> Option<NtfsFile> {
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
    ) -> Vec<u8>
    where
        R: Seek + Read,
    {
        let mut data = Vec::<u8>::new();

        let header = unsafe { &*(record.as_ptr() as *const NtfsFileRecordHeader) };
        let mut att_offset = header.attributes_offset as usize;

        info!("Reading DATA attribute");

        loop {
            if att_offset >= header.used_size as usize {
                break;
            }

            let att = NtfsAttribute::new(&record[att_offset..]);
            if att.header.type_id == NtfsAttributeType::End as u32 {
                break;
            }

            if att.header.type_id == attribute_type as u32 {
                if att.header.is_non_resident == 0 {
                    data.copy_from_slice(att.as_resident_data());
                } else {
                    let read_start = Instant::now();

                    let mut buffer = Vec::new();
                    let (size, runs) = att.get_nonresident_data_runs(volume);
                    data.reserve_exact(size);
                    let mut copied = 0usize;

                    for (run_idx, run) in runs.iter().enumerate() {
                        if copied >= size {
                            break;
                        }

                        let run_start = Instant::now();

                        let buf_size = usize::min(run.len(), size - copied);
                        buffer.resize(buf_size, 0u8);

                        let _ = reader.seek(SeekFrom::Start(run.start as u64));
                        let _ = reader.read_exact(&mut buffer);

                        data.append(&mut buffer.clone());
                        copied += buf_size;

                        info!(
                            "- Run {}/{} (size: {}, took {:?})",
                            run_idx + 1,
                            runs.len(),
                            buf_size,
                            Instant::now() - run_start
                        );
                    }

                    info!(
                        "Loaded DATA of size {} in {:?}",
                        copied,
                        Instant::now() - read_start
                    );
                }
            }

            att_offset += att.header.length as usize;
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

    use std::time::Instant;

    use crate::{
        errors::NtfsReaderResult,
        file::NtfsFile,
        file_info::{FileInfo, HashMapCache, VecCache},
        mft::Mft,
        volume::Volume,
    };
    use tracing::info;
    use tracing_subscriber::FmtSubscriber;

    fn init_tracing() {
        let subscriber = FmtSubscriber::builder()
            .with_max_level(tracing::Level::TRACE)
            .without_time()
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
    }

    fn test_iteration<F>(name: &str, mft: &Mft, mut file_info_creator: F) -> NtfsReaderResult<()>
    where
        F: FnMut(&Mft, &NtfsFile) -> FileInfo,
    {
        let mut files = Vec::new();
        files.reserve(mft.max_record as usize);

        info!("======== Testing {} ========", name);
        let mut counter = 0usize;

        mft.iterate_files(|file| {
            files.push(file_info_creator(mft, file));
            counter += 1;
            if counter % 100_000 == 0 {
                info!("- Read {} records", counter);
            }
        });

        info!("Read all {} records", counter);
        Ok(())
    }

    #[test]
    fn iterate_files() -> NtfsReaderResult<()> {
        init_tracing();

        let vol = Volume::new("\\\\.\\C:")?;
        let mft = Mft::new(vol)?;

        // Test without cache
        let start_time = Instant::now();
        test_iteration("No Cache", &mft, |mft: &Mft, file: &NtfsFile| {
            FileInfo::new(mft, file)
        })?;
        let no_cache_iteration_duration = Instant::now() - start_time;
        let no_cache_total_duration = no_cache_iteration_duration;

        //Test with HashMap cache
        let mut hashmap_cache = HashMapCache::default();
        let start_time = Instant::now();
        test_iteration("HashMap Cache", &mft, |mft: &Mft, file: &NtfsFile| {
            FileInfo::with_cache(mft, file, &mut hashmap_cache)
        })?;
        let iteration_end_time = Instant::now();
        let hashmap_iteration_duration = iteration_end_time - start_time;

        let pre_drop_time = Instant::now();
        info!("Dropping cache...");
        drop(hashmap_cache);
        let hashmap_cache_drop_duration = Instant::now() - pre_drop_time;
        let hashmap_cache_total_duration = Instant::now() - start_time;

        //Test with Vec cache
        let mut vec_cache = VecCache::default();
        vec_cache.0.resize(mft.max_record as usize, None);
        let start_time = Instant::now();
        test_iteration("Vec Cache", &mft, |mft: &Mft, file: &NtfsFile| {
            FileInfo::with_cache(mft, file, &mut vec_cache)
        })?;
        let iteration_end_time = Instant::now();
        let vec_iteration_duration = iteration_end_time - start_time;

        let pre_drop_time = Instant::now();
        info!("Dropping cache...");
        drop(vec_cache);
        let vec_cache_drop_duration = Instant::now() - pre_drop_time;
        let vec_cache_total_duration = Instant::now() - start_time;

        info!("========= Timings Summary =========");
        info!(
            "{:<13} {:<10} {:<10} {:<10}",
            "Type", "Iteration", "Drop", "Total"
        );
        info!(
            "{:<13} {:<10.3?} {:<10} {:<10.3?}",
            "No Cache", no_cache_iteration_duration, "0", no_cache_total_duration
        );

        info!(
            "{:<13} {:<10.3?} {:<10.3?} {:<10.3?}",
            "HashMap Cache",
            hashmap_iteration_duration,
            hashmap_cache_drop_duration,
            hashmap_cache_total_duration
        );

        info!(
            "{:<13} {:<10.3?} {:<10.3?} {:<10.3?}",
            "Vec Cache", vec_iteration_duration, vec_cache_drop_duration, vec_cache_total_duration
        );

        Ok(())
    }
}
