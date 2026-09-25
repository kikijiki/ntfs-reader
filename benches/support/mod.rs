//! Synthetic MFT builders shared by `mft_load_synthetic.rs`, `file_access_synthetic.rs` and
//! `deleted_synthetic.rs`. Needs the `internals` feature (see Cargo.toml), which exposes the
//! otherwise `#[cfg(test)]`-only `ntfs_reader::internals::test_records`, so these benches reuse
//! the crate's own unit-test record builders instead of duplicating them.
#![allow(dead_code)]

use ntfs_reader::internals::test_records::*;
use ntfs_reader::{Mft, NtfsAttributeType, NtfsFileNamespace, Volume};

/// Directories a leaf file sits under; also the number of hops a cold
/// `resolve_path` walks for one.
pub const DIR_DEPTH: usize = 8;

/// One nested directory record per level (`dir0` at the root, `dir1` under `dir0`, ...). Must be
/// the first records in [`build_dataset`]'s output: their numbers (vector position, from
/// [`FIRST_RECORD`]) are what the parent references here assume.
fn dir_chain(depth: usize) -> (Vec<Vec<u8>>, u64 /* deepest dir's record number */) {
    let mut records = Vec::with_capacity(depth);
    let mut parent: Option<u64> = None; // None: add_file_name's default (the root)
    let mut number = FIRST_RECORD;

    for level in 0..depth {
        let mut record = new_record(number, 1, 0);
        set_record_flags(&mut record, directory_flags());
        let name = format!("dir{level}");
        let offset = match parent {
            None => add_file_name(&mut record, ATTRIBUTES_OFFSET, &name, 0),
            Some(parent_ref) => add_file_name_ex(
                &mut record,
                ATTRIBUTES_OFFSET,
                1,
                parent_ref,
                NtfsFileNamespace::Win32,
                &name,
                0,
            ),
        };
        finish_record(&mut record, offset);
        records.push(record);
        parent = Some(reference(1, number));
        number += 1;
    }

    (records, number - 1)
}

/// A synthetic MFT: [`DIR_DEPTH`] nested directories, then `file_count` leaf files under the
/// deepest one. Every file has a Win32 name, standard information and a small resident `$DATA`
/// attribute. Every `stream_every`th file (0 disables it) gets a named alternate data stream too.
/// Every `split_every`th file (0 disables it) has its `$DATA` (and ADS) moved into an extension
/// record instead, so `attributes()`/`data_streams()` cross records for part of the dataset: the
/// same shape a fragmented file or one with many hard links has on a real volume.
pub fn build_dataset(file_count: usize, stream_every: usize, split_every: usize) -> Mft {
    let (volume, data, bitmap) = raw_parts(dataset_records(file_count, stream_every, split_every));
    build_from_parts(volume, data, bitmap)
}

/// Same dataset as [`build_dataset`], but returns the raw `(volume, data, bitmap)` triple
/// instead of building the `Mft`, so a bench can time [`build_from_parts`] itself on a buffer
/// built once, outside the timed section.
pub fn dataset_raw_parts(
    file_count: usize,
    stream_every: usize,
    split_every: usize,
) -> (Volume, Vec<u8>, Vec<u8>) {
    raw_parts(dataset_records(file_count, stream_every, split_every))
}

/// [`dataset_raw_parts`] with every file split into a base and extension record, and every
/// `freed_every`th file deleted (in-use flag off, `$BITMAP` bits clear, sequence bumped, as a
/// real delete leaves them). Times the loader against freed records, which the plain datasets
/// do not have.
pub fn dataset_raw_parts_freed(
    file_count: usize,
    freed_every: usize,
) -> (Volume, Vec<u8>, Vec<u8>) {
    let mut records = dataset_records(file_count, 0, 1);
    let first_leaf = DIR_DEPTH;
    let mut freed = Vec::new();
    for (index, pair) in records[first_leaf..].chunks_mut(2).enumerate() {
        if index % freed_every == 0 {
            for (offset, record) in pair.iter_mut().enumerate() {
                // What a delete leaves: flag off, sequence bumped, so the extension record's base
                // reference names the freed base like a real one.
                delete_record(record);
                freed.push(FIRST_RECORD as usize + first_leaf + index * 2 + offset);
            }
        }
    }
    let (volume, data, mut bitmap) = raw_parts(records);
    for number in freed {
        bitmap[number / 8] &= !(1 << (number % 8));
    }
    (volume, data, bitmap)
}

fn dataset_records(file_count: usize, stream_every: usize, split_every: usize) -> Vec<Vec<u8>> {
    let (mut records, parent_number) = dir_chain(DIR_DEPTH);
    let parent = reference(1, parent_number);
    let mut number = FIRST_RECORD + records.len() as u64;

    for index in 0..file_count {
        let mut file = new_record(number, 1, 0);
        let name = format!("file{index}.dat");
        let mut offset = add_file_name_ex(
            &mut file,
            ATTRIBUTES_OFFSET,
            1,
            parent,
            NtfsFileNamespace::Win32,
            &name,
            0,
        );
        offset = add_standard_information(&mut file, offset, 0x20);

        let with_stream = stream_every != 0 && index % stream_every == 0;
        let split = split_every != 0 && index % split_every == 0;

        if split {
            // Base record: name and standard info only; $DATA (and the ADS,
            // if any) live in an extension record.
            finish_record(&mut file, offset);
            records.push(file);

            let extension_number = number + 1;
            let mut extension = new_record(extension_number, 1, reference(1, number));
            let mut ext_offset = add_nonresident_data(&mut extension, ATTRIBUTES_OFFSET, 65536);
            if with_stream {
                ext_offset = add_resident_attribute(
                    &mut extension,
                    ext_offset,
                    NtfsAttributeType::Data,
                    3,
                    "ads",
                    b"alternate stream contents",
                );
            }
            finish_record(&mut extension, ext_offset);
            records.push(extension);
            number += 2;
        } else {
            offset = add_resident_attribute(
                &mut file,
                offset,
                NtfsAttributeType::Data,
                2,
                "",
                b"resident file contents",
            );
            if with_stream {
                offset = add_resident_attribute(
                    &mut file,
                    offset,
                    NtfsAttributeType::Data,
                    3,
                    "ads",
                    b"alternate stream contents",
                );
            }
            finish_record(&mut file, offset);
            records.push(file);
            number += 1;
        }
    }

    records
}

/// Record bytes for a single, self-contained non-resident `$DATA` attribute with `run_count`
/// real data runs (4 clusters each, strictly increasing LCNs), for timing data run decoding
/// alone. Returns the record and the [`test_volume`] its cluster size matches.
///
/// `run_count` is bounded by how many runs fit in one [`RECORD_SIZE`]-byte record:
/// `nonresident_data_runs` only decodes a single self-contained extent (a value split across
/// extension records is a different code path), so this is a real fixture ceiling, not an
/// arbitrary one. Each run costs 3 bytes (1-byte cluster count, 1-byte offset); the non-resident
/// header ahead of them is 64 bytes.
pub fn fragmented_data_run_record(run_count: usize) -> (Vec<u8>, Volume) {
    const CLUSTERS_PER_RUN: u64 = 4;
    const HEADER_BYTES: usize = 64;
    const BYTES_PER_RUN: usize = 3;
    // -1: the trailing terminator byte `add_nonresident_data_runs` writes
    // right after the last run.
    let max_run_count = (RECORD_SIZE - ATTRIBUTES_OFFSET - HEADER_BYTES - 1) / BYTES_PER_RUN;
    assert!(
        run_count <= max_run_count,
        "run_count {run_count} would overflow one {RECORD_SIZE}-byte record \
         (fits at most {max_run_count} runs)",
    );

    let mut runs = Vec::with_capacity(run_count * 3);
    for _ in 0..run_count {
        // Descriptor 0x11: 1-byte cluster count, 1-byte signed offset. A constant positive
        // offset keeps the LCN strictly increasing.
        runs.push(0x11);
        runs.push(CLUSTERS_PER_RUN as u8);
        runs.push(2u8);
    }

    let data_size = run_count as u64 * CLUSTERS_PER_RUN * CLUSTER_SIZE as u64;
    let mut record = new_record(FIRST_RECORD, 1, 0);
    let offset = add_nonresident_data_runs(
        &mut record,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Data,
        "",
        0,
        run_count as u64 - 1,
        data_size,
        &runs,
    );
    finish_record(&mut record, offset);

    (record, test_volume())
}

/// A synthetic volume with [`test_volume`]'s cluster size and a known size of `clusters`
/// clusters, for checking runs and building a bitmap against it.
pub fn sized_volume(clusters: u64) -> Volume {
    let plain = test_volume();
    Volume::synthetic(
        plain.path(),
        plain.cluster_size(),
        clusters * plain.cluster_size(),
        RECORD_SIZE as u64,
        0,
    )
}

/// A synthetic MFT for the deleted-file paths: `depth` nested directories, `file_count` files
/// under the deepest one, and every `freed_every`th file deleted (flag off, `$BITMAP` bit clear,
/// sequence bumped). With `freed_dirs`, the directories are deleted too, so a deleted file's
/// path walks through freed directories.
pub fn deleted_dataset(
    file_count: usize,
    depth: usize,
    freed_every: usize,
    freed_dirs: bool,
) -> Mft {
    let (mut records, parent_number) = dir_chain(depth);
    let parent = reference(1, parent_number);
    let first_file = FIRST_RECORD as usize + records.len();
    let mut freed = Vec::new();
    if freed_dirs {
        for (index, record) in records.iter_mut().enumerate() {
            delete_record(record);
            freed.push(FIRST_RECORD as usize + index);
        }
    }
    for index in 0..file_count {
        let number = (first_file + index) as u64;
        let mut file = new_record(number, 1, 0);
        let mut offset = add_file_name_ex(
            &mut file,
            ATTRIBUTES_OFFSET,
            1,
            parent,
            NtfsFileNamespace::Win32,
            &format!("file{index}.dat"),
            0,
        );
        offset = add_standard_information(&mut file, offset, 0x20);
        offset = add_resident_attribute(
            &mut file,
            offset,
            NtfsAttributeType::Data,
            2,
            "",
            b"resident file contents",
        );
        finish_record(&mut file, offset);
        if index % freed_every == 0 {
            delete_record(&mut file);
            freed.push(number as usize);
        }
        records.push(file);
    }
    let (volume, data, mut bitmap) = raw_parts(records);
    for number in freed {
        bitmap[number / 8] &= !(1 << (number % 8));
    }
    build_from_parts(volume, data, bitmap)
}

/// An `Mft` whose record 24 has one non-resident default stream in `extents` extents of one
/// cluster each, spread over extension records (`PER_RECORD` runs each) with a cluster gap
/// between every two so nothing is contiguous. Each record's run list starts over at cluster 2,
/// like a real extent's absolute first offset, so the volume stays small.
pub fn fragmented_file(extents: usize) -> Mft {
    const PER_RECORD: usize = 200;
    let size = extents as u64 * CLUSTER_SIZE as u64;
    let base_reference = reference(1, FIRST_RECORD);
    let mut records = Vec::new();
    let mut vcn = 0usize;
    while vcn < extents {
        let count = PER_RECORD.min(extents - vcn);
        let mut runs = Vec::with_capacity(count * 3);
        for _ in 0..count {
            // One cluster, then 2 clusters on: descriptor 0x11 (1-byte length, 1-byte offset).
            runs.extend_from_slice(&[0x11, 1, 2]);
        }
        let number = FIRST_RECORD + records.len() as u64;
        let mut record = new_record(number, 1, if vcn == 0 { 0 } else { base_reference });
        let mut offset = ATTRIBUTES_OFFSET;
        if vcn == 0 {
            offset = add_file_name(&mut record, offset, "fragmented.bin", 0);
        }
        offset = add_nonresident_data_runs(
            &mut record,
            offset,
            NtfsAttributeType::Data,
            "",
            vcn as u64,
            (vcn + count - 1) as u64,
            if vcn == 0 { size } else { 0 },
            &runs,
        );
        finish_record(&mut record, offset);
        records.push(record);
        vcn += count;
    }
    let (_, data, bitmap) = raw_parts(records);
    build_from_parts(sized_volume(2 * PER_RECORD as u64 + 16), data, bitmap)
}
