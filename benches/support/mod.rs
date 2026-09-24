//! Synthetic MFT builders shared by `mft_load_synthetic.rs` and
//! `file_access_synthetic.rs`. Needs the `internals` feature (see
//! Cargo.toml), which exposes `ntfs_reader::internals::test_records` (otherwise
//! `#[cfg(test)]`-only) so these benches reuse the exact record-building
//! code the crate's own unit tests use, instead of a second copy of it.
#![allow(dead_code)]

use ntfs_reader::internals::test_records::*;
use ntfs_reader::{Mft, NtfsAttributeType, NtfsFileNamespace, Volume};

/// Directories a leaf file sits under; also the number of hops a cold
/// `resolve_path` walks for one.
pub const DIR_DEPTH: usize = 8;

/// One nested directory record per level (`dir0` at the root, `dir1` under
/// `dir0`, ...). Must be the first records placed by [`build_dataset`], so
/// their record numbers (assigned by vector position, starting at
/// [`FIRST_RECORD`]) match what this builds parent references from.
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

/// A synthetic MFT: [`DIR_DEPTH`] nested directories, then `file_count` leaf
/// files under the deepest one. Every file has a Win32 name, standard
/// information and a small resident `$DATA` attribute. Every `stream_every`th
/// file (0 disables it) also gets a named alternate data stream. Every
/// `split_every`th file (0 disables it) has its `$DATA` (and ADS, if any)
/// moved into a second, extension record instead of staying in the base one,
/// so `attributes()`/`data_streams()` have to cross records for a slice of
/// the dataset - the same shape a fragmented file or one with many hard
/// links has on a real volume (see docs/architecture.md's "central rule").
pub fn build_dataset(file_count: usize, stream_every: usize, split_every: usize) -> Mft {
    let (volume, data, bitmap) = raw_parts(dataset_records(file_count, stream_every, split_every));
    build_from_parts(volume, data, bitmap)
}

/// Same dataset as [`build_dataset`], but returns the raw `(volume, data,
/// bitmap)` triple instead of building the `Mft`, so a bench can time
/// [`build_from_parts`] itself on a buffer built once, outside the timed
/// section.
pub fn dataset_raw_parts(
    file_count: usize,
    stream_every: usize,
    split_every: usize,
) -> (Volume, Vec<u8>, Vec<u8>) {
    raw_parts(dataset_records(file_count, stream_every, split_every))
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

/// A single, self-contained non-resident `$DATA` attribute's worth of
/// record bytes, with `run_count` real data runs (4 clusters each, at
/// strictly increasing LCNs), for timing data run decoding alone.
/// Returns the record and the [`test_volume`] its cluster size matches.
///
/// `run_count` is bounded by how many runs fit in one [`RECORD_SIZE`]-byte
/// record: `nonresident_data_runs` only decodes a single,
/// self-contained extent (a value split across extension records is a
/// different code path, the crate's own extension-record walk), so this is
/// a real ceiling on this fixture, not an arbitrary one to raise. Each run
/// costs 3 bytes here (a 1-byte cluster count, a 1-byte offset); the
/// non-resident attribute header ahead of them is 64 bytes.
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
        // Descriptor 0x11: a 1-byte cluster count, a 1-byte (signed)
        // cluster offset. A constant positive offset keeps the running LCN
        // monotonically increasing, so it never goes negative.
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
