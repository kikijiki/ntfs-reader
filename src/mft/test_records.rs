// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! Synthetic MFT records for unit tests and the property test, and (under the
//! `internals` feature) for `benches/*_synthetic.rs`. This is a child of
//! `mft` so it can build an `Mft` without a volume. Not every caller uses
//! every helper.
#![allow(dead_code)]

use std::path::PathBuf;

use super::Mft;
use crate::api::*;
use crate::volume::Volume;

/// Size of one synthetic MFT record.
pub const RECORD_SIZE: usize = 1024;
/// Cluster size of [`test_volume`].
pub const CLUSTER_SIZE: usize = 4096;
/// Offset of the update sequence array in a record built by [`new_record`]:
/// right after the header of an NTFS 3.1 record (which keeps its record number
/// at offset 44).
pub const UPDATE_SEQUENCE_OFFSET: usize = 48;
/// Where the first attribute of a record built by [`new_record`] starts: after
/// the update sequence array (a sequence number plus one saved value for each
/// of the record's two sectors, 6 bytes), aligned to 8.
pub const ATTRIBUTES_OFFSET: usize = 56;
/// Path of [`test_volume`].
pub const VOLUME_PATH: &str = r"\\.\T:";
/// The update sequence number [`finish_record`] protects records with.
pub const UPDATE_SEQUENCE_NUMBER: u16 = 1;

// The on-disk sizes of three structures, written out instead of taken from the crate's structs
// with `size_of`, so a struct of the wrong size cannot make the fixtures agree with it.
/// A `$STANDARD_INFORMATION` value of NTFS 3.x (the crate's struct reads the first 36 bytes).
const STANDARD_INFORMATION_SIZE: usize = 72;
/// The fixed part of a `$FILE_NAME` value, before the name.
const FILE_NAME_HEADER: usize = 66;
/// The header of a non-resident attribute, before its name and its data runs.
const NONRESIDENT_HEADER: usize = 64;

/// Record number [`mft_with`] places its first record at (records after
/// that are numbered by vector position). `crate::api::FIRST_NORMAL_RECORD`
/// is `pub(crate)`, so this copies its value out for external callers
/// (`benches/*_synthetic.rs`) that need to predict record numbers while
/// building the input vector, e.g. to link a directory chain's parents.
pub const FIRST_RECORD: u64 = FIRST_NORMAL_RECORD;

/// Sequence number of the synthetic root record `mft_with` places at
/// [`ROOT_RECORD`]. Names built by [`add_file_name`] (which default their
/// parent to the root) carry this sequence, so `resolve_path`'s root check
/// (comparing the full reference against the root record's own
/// `reference()`) finds a real, matching root instead of failing.
pub const ROOT_SEQUENCE: u16 = 1;

/// Asserts that `text` holds exactly the UTF-16 code units `units`, looking at
/// the platform's own representation instead of converting through the crate:
/// the units themselves on Windows, `wtf8` (the same text as WTF-8 bytes, which
/// is what the crate writes for a lone surrogate elsewhere) on other platforms.
#[track_caller]
pub fn assert_os_str_is(text: &std::ffi::OsStr, units: &[u16], wtf8: &[u8]) {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let _ = wtf8;
        assert_eq!(text.encode_wide().collect::<Vec<u16>>(), units);
    }
    #[cfg(not(windows))]
    {
        let _ = units;
        assert_eq!(text.as_encoded_bytes(), wtf8);
    }
}

/// Reference with a sequence number, as stored in a `$FILE_NAME` parent field.
pub fn reference(sequence: u16, number: u64) -> u64 {
    ((sequence as u64) << 48) | number
}

/// A minimal, valid root directory record (record [`ROOT_RECORD`], sequence
/// [`ROOT_SEQUENCE`]), for [`mft_with`] to place at its fixed position.
pub fn root_record() -> Vec<u8> {
    let mut root = new_record(ROOT_RECORD, ROOT_SEQUENCE, 0);
    set_record_flags(&mut root, directory_flags());
    let offset = add_end_marker(&mut root, ATTRIBUTES_OFFSET);
    finish_record(&mut root, offset);
    root
}

/// A volume with 4 KiB clusters, 1 KiB records, the `$MFT` at byte 0 and an
/// unknown size (0: not used to bound anything).
pub fn test_volume() -> Volume {
    Volume::synthetic(
        PathBuf::from(VOLUME_PATH),
        CLUSTER_SIZE as u64,
        0,
        RECORD_SIZE as u64,
        0,
    )
}

/// Raw `(volume, data, bitmap)` inputs to `Mft::from_parts`: the same layout
/// [`mft_with`] builds (records placed consecutively from [`FIRST_RECORD`],
/// plus a real root record at [`ROOT_RECORD`], see [`root_record`]), without
/// applying fixups or indexing extension records. Lets a bench build the
/// input once and time [`build_from_parts`] (fixup + indexing) alone,
/// repeatedly, on a fresh clone of `data`/`bitmap`.
pub fn raw_parts(records: Vec<Vec<u8>>) -> (Volume, Vec<u8>, Vec<u8>) {
    let first = FIRST_NORMAL_RECORD as usize;
    let record_count = first + records.len();
    let mut data = vec![0u8; first * RECORD_SIZE];
    let mut bitmap = vec![0u8; record_count.div_ceil(8)];

    let root_number = ROOT_RECORD as usize;
    data[root_number * RECORD_SIZE..(root_number + 1) * RECORD_SIZE]
        .copy_from_slice(&root_record());
    bitmap[root_number / 8] |= 1 << (root_number % 8);

    for (index, record) in records.into_iter().enumerate() {
        let number = first + index;
        bitmap[number / 8] |= 1 << (number % 8);
        data.extend(record);
    }

    (test_volume(), data, bitmap)
}

/// Applies fixups and indexes extension records: the same work `Mft::new`
/// does after reading a volume's `$MFT`. `Mft::from_parts` itself is
/// crate-private, so this is the entry point a bench uses to time it (Card
/// 008 made this step about 25x faster; nothing measured that until now).
pub fn build_from_parts(volume: Volume, data: Vec<u8>, bitmap: Vec<u8>) -> Mft {
    Mft::from_parts(volume, data, bitmap).expect("synthetic MFT")
}

/// Places `records` at consecutive numbers starting at [`FIRST_RECORD`],
/// plus a real root record at [`ROOT_RECORD`] (see [`root_record`]) so names
/// that resolve up to the root (the default for [`add_file_name`]) find a
/// real, matching record instead of an empty/invalid one.
pub fn mft_with(records: Vec<Vec<u8>>) -> Mft {
    let (volume, data, bitmap) = raw_parts(records);
    build_from_parts(volume, data, bitmap)
}

/// Places each record at its given number, unlike [`mft_with`] which always
/// starts at [`FIRST_NORMAL_RECORD`]. Needed for records below that (for
/// example the root directory at record 5).
pub fn mft_with_at(records: Vec<(u64, Vec<u8>)>) -> Mft {
    let record_count = records
        .iter()
        .map(|(number, _)| number + 1)
        .max()
        .unwrap_or(0);
    let mut data = vec![0u8; record_count as usize * RECORD_SIZE];
    let mut bitmap = vec![0u8; (record_count as usize).div_ceil(8)];
    for (number, record) in records {
        bitmap[number as usize / 8] |= 1 << (number % 8);
        let start = number as usize * RECORD_SIZE;
        data[start..start + RECORD_SIZE].copy_from_slice(&record);
    }

    Mft::from_parts(test_volume(), data, bitmap).expect("synthetic MFT")
}

/// A record header in the NTFS 3.1 layout: signature, a well-formed update
/// sequence array (offset [`UPDATE_SEQUENCE_OFFSET`], one entry per sector
/// plus the sequence number), the record number at offset 44, in use, no
/// attributes. Add attributes and call [`finish_record`], which sets the used
/// size and applies the update sequence protection.
pub fn new_record(number: u64, sequence: u16, base_reference: u64) -> Vec<u8> {
    let mut record = vec![0u8; RECORD_SIZE];
    record[0..4].copy_from_slice(FILE_RECORD_SIGNATURE);
    write_u16(&mut record, 4, UPDATE_SEQUENCE_OFFSET as u16);
    write_u16(&mut record, 6, (RECORD_SIZE / SECTOR_SIZE + 1) as u16);
    write_u16(&mut record, 16, sequence);
    write_u16(&mut record, 18, 1);
    write_u16(&mut record, 20, ATTRIBUTES_OFFSET as u16);
    write_u16(&mut record, 22, NtfsFileFlags::InUse as u16);
    write_u32(&mut record, 28, RECORD_SIZE as u32);
    write_u64(&mut record, 32, base_reference);
    write_u32(&mut record, 44, number as u32);
    record
}

/// Overwrites the record header flags (`NtfsFileFlags`).
pub fn set_record_flags(record: &mut [u8], flags: u16) {
    write_u16(record, 22, flags);
}

pub fn directory_flags() -> u16 {
    NtfsFileFlags::InUse as u16 | NtfsFileFlags::IsDirectory as u16
}

/// A `$STANDARD_INFORMATION` with four distinct times: created 1 s, modified
/// 2 s, MFT modified 4 s and accessed 3 s after the Unix epoch.
pub fn add_standard_information(record: &mut [u8], offset: usize, attributes: u32) -> usize {
    add_standard_information_at(
        record,
        offset,
        attributes,
        [10_000_000, 20_000_000, 40_000_000, 30_000_000].map(|ticks| EPOCH_DIFFERENCE + ticks),
    )
}

/// A `$STANDARD_INFORMATION` with the given FILETIMEs, in the on-disk order:
/// created, modified, MFT modified, accessed.
pub fn add_standard_information_at(
    record: &mut [u8],
    offset: usize,
    attributes: u32,
    [created, modified, mft_modified, accessed]: [u64; 4],
) -> usize {
    let mut value = vec![0u8; STANDARD_INFORMATION_SIZE];
    write_u64(&mut value, 0, created);
    write_u64(&mut value, 8, modified);
    write_u64(&mut value, 16, mft_modified);
    write_u64(&mut value, 24, accessed);
    write_u32(&mut value, 32, attributes);
    add_resident_attribute(
        record,
        offset,
        NtfsAttributeType::StandardInformation,
        0,
        "",
        &value,
    )
}

/// Defaults the parent to the real root record [`mft_with`] places at
/// [`ROOT_RECORD`] with sequence [`ROOT_SEQUENCE`] - not the bare record
/// number (which would mean sequence 0, not matching that root).
pub fn add_file_name(record: &mut [u8], offset: usize, name: &str, attributes: u32) -> usize {
    add_file_name_ex(
        record,
        offset,
        1,
        reference(ROOT_SEQUENCE, ROOT_RECORD),
        NtfsFileNamespace::Win32,
        name,
        attributes,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn add_file_name_ex(
    record: &mut [u8],
    offset: usize,
    id: u16,
    parent: u64,
    namespace: NtfsFileNamespace,
    name: &str,
    attributes: u32,
) -> usize {
    let encoded: Vec<u16> = name.encode_utf16().collect();
    add_file_name_raw(record, offset, id, parent, namespace, &encoded, attributes)
}

/// Like [`add_file_name_ex`], but takes the name as raw UTF-16 code units
/// instead of `&str`. `str::encode_utf16` can only ever produce well-formed
/// UTF-16 (a lone surrogate cannot appear in a valid `str`), but NTFS does
/// not require that: an unpaired surrogate is a real, accepted Windows
/// filename, and this is how a test builds one.
#[allow(clippy::too_many_arguments)]
pub fn add_file_name_raw(
    record: &mut [u8],
    offset: usize,
    id: u16,
    parent: u64,
    namespace: NtfsFileNamespace,
    name: &[u16],
    attributes: u32,
) -> usize {
    let mut value = vec![0u8; FILE_NAME_HEADER + name.len() * 2];
    write_u64(&mut value, 0, parent);
    write_u64(&mut value, 40, 34_359_738_368);
    write_u64(&mut value, 48, 34_359_738_368);
    write_u32(&mut value, 56, attributes);
    value[64] = name.len() as u8;
    value[65] = namespace as u8;
    for (index, &character) in name.iter().enumerate() {
        write_u16(&mut value, FILE_NAME_HEADER + index * 2, character);
    }
    add_resident_attribute(record, offset, NtfsAttributeType::FileName, id, "", &value)
}

pub fn add_resident_attribute(
    record: &mut [u8],
    offset: usize,
    attribute_type: NtfsAttributeType,
    id: u16,
    name: &str,
    value: &[u8],
) -> usize {
    let name: Vec<u16> = name.encode_utf16().collect();
    add_resident_attribute_raw(record, offset, attribute_type, id, &name, value)
}

/// Like [`add_resident_attribute`], with the attribute name as raw UTF-16
/// code units, so a test can give it an unpaired surrogate.
pub fn add_resident_attribute_raw(
    record: &mut [u8],
    offset: usize,
    attribute_type: NtfsAttributeType,
    id: u16,
    name: &[u16],
    value: &[u8],
) -> usize {
    const NAME_OFFSET: usize = 24;
    let value_offset = align_to_eight(NAME_OFFSET + name.len() * 2);
    let length = align_to_eight(value_offset + value.len());
    write_u32(record, offset, attribute_type as u32);
    write_u32(record, offset + 4, length as u32);
    record[offset + 9] = name.len() as u8;
    write_u16(record, offset + 10, NAME_OFFSET as u16);
    write_u16(record, offset + 14, id);
    write_u32(record, offset + 16, value.len() as u32);
    write_u16(record, offset + 20, value_offset as u16);
    for (index, &unit) in name.iter().enumerate() {
        write_u16(record, offset + NAME_OFFSET + index * 2, unit);
    }
    record[offset + value_offset..offset + value_offset + value.len()].copy_from_slice(value);
    offset + length
}

/// A non-resident attribute extent without data runs. Only the extent with
/// `lowest_vcn == 0` carries a meaningful `size`.
#[allow(clippy::too_many_arguments)]
pub fn add_nonresident_attribute(
    record: &mut [u8],
    offset: usize,
    attribute_type: NtfsAttributeType,
    id: u16,
    name: &str,
    lowest_vcn: i64,
    size: u64,
) -> usize {
    let header = NONRESIDENT_HEADER;
    let name: Vec<u16> = name.encode_utf16().collect();
    let length = align_to_eight(header + name.len() * 2);
    write_u32(record, offset, attribute_type as u32);
    write_u32(record, offset + 4, length as u32);
    record[offset + 8] = 1;
    record[offset + 9] = name.len() as u8;
    write_u16(record, offset + 10, header as u16);
    write_u16(record, offset + 14, id);
    write_u64(record, offset + 16, lowest_vcn as u64);
    write_u16(record, offset + 32, length as u16);
    write_u64(record, offset + 40, size);
    write_u64(record, offset + 48, size);
    write_u64(record, offset + 56, size);
    for (index, unit) in name.into_iter().enumerate() {
        write_u16(record, offset + header + index * 2, unit);
    }
    offset + length
}

pub fn add_nonresident_data(record: &mut [u8], offset: usize, size: u64) -> usize {
    add_nonresident_attribute(record, offset, NtfsAttributeType::Data, 2, "", 0, size)
}

/// Writes the `End` marker (with a non-zero length, as found on disk) and
/// returns the offset after it.
pub fn add_end_marker(record: &mut [u8], offset: usize) -> usize {
    write_u32(record, offset, NtfsAttributeType::End as u32);
    write_u32(record, offset + 4, 16);
    offset + 16
}

/// Sets the record's used size and applies the update sequence protection
/// (see [`protect_record`]). Call it last: the record's sector ends move into
/// the update sequence array, so bytes written over them afterwards are lost
/// to the fixup.
pub fn finish_record(record: &mut [u8], used_size: usize) {
    write_u32(record, 24, used_size as u32);
    protect_record(record, UPDATE_SEQUENCE_NUMBER);
}

pub fn align_to_eight(value: usize) -> usize {
    (value + 7) & !7
}

pub fn write_u16(data: &mut [u8], offset: usize, value: u16) {
    data[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

pub fn write_u32(data: &mut [u8], offset: usize, value: u32) {
    data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

pub fn write_u64(data: &mut [u8], offset: usize, value: u64) {
    data[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

/// Applies NTFS update sequence protection to a record, the inverse of
/// `fixup_record`: sector ends move into the array, `usn` takes their place.
/// A record that is already protected (the array's own number sits at every
/// sector end) is restored first, so this can be called again to change the
/// number.
pub fn protect_record(record: &mut [u8], usn: u16) {
    let sectors = record.len() / SECTOR_SIZE;
    let previous = [
        record[UPDATE_SEQUENCE_OFFSET],
        record[UPDATE_SEQUENCE_OFFSET + 1],
    ];
    for sector in 0..sectors {
        let end = (sector + 1) * SECTOR_SIZE - 2;
        if record[end..end + 2] == previous {
            let slot = UPDATE_SEQUENCE_OFFSET + 2 + sector * 2;
            record.copy_within(slot..slot + 2, end);
        }
    }

    write_u16(record, 4, UPDATE_SEQUENCE_OFFSET as u16);
    write_u16(record, 6, (sectors + 1) as u16);
    write_u16(record, UPDATE_SEQUENCE_OFFSET, usn);
    for sector in 0..sectors {
        let end = (sector + 1) * SECTOR_SIZE - 2;
        let slot = UPDATE_SEQUENCE_OFFSET + 2 + sector * 2;
        record.copy_within(end..end + 2, slot);
        write_u16(record, end, usn);
    }
}

/// One `$ATTRIBUTE_LIST` entry pointing at `record` (sequence 1).
pub fn list_entry(
    attribute_type: NtfsAttributeType,
    name: &str,
    starting_vcn: u64,
    record: u64,
) -> Vec<u8> {
    const NAME_OFFSET: usize = 26;
    let name: Vec<u16> = name.encode_utf16().collect();
    let length = align_to_eight(NAME_OFFSET + name.len() * 2);
    let mut entry = vec![0u8; length];
    write_u32(&mut entry, 0, attribute_type as u32);
    write_u16(&mut entry, 4, length as u16);
    entry[6] = name.len() as u8;
    entry[7] = NAME_OFFSET as u8;
    write_u64(&mut entry, 8, starting_vcn);
    write_u64(&mut entry, 16, (1u64 << 48) | record);
    for (index, unit) in name.into_iter().enumerate() {
        write_u16(&mut entry, NAME_OFFSET + index * 2, unit);
    }
    entry
}

/// A non-resident attribute extent covering `lowest_vcn..=highest_vcn` with
/// the given mapping pairs (the terminator is added). Unlike
/// [`add_nonresident_attribute`], which writes a single implicit extent from
/// a size alone, this builds the explicit data runs bytes, for tests that
/// exercise `$MFT`'s own `$DATA` extent mapping.
#[allow(clippy::too_many_arguments)]
pub fn add_nonresident_data_runs(
    record: &mut [u8],
    offset: usize,
    attribute_type: NtfsAttributeType,
    name: &str,
    lowest_vcn: u64,
    highest_vcn: u64,
    data_size: u64,
    runs: &[u8],
) -> usize {
    let name: Vec<u16> = name.encode_utf16().collect();
    add_nonresident_data_runs_raw(
        record,
        offset,
        attribute_type,
        &name,
        lowest_vcn,
        highest_vcn,
        data_size,
        runs,
    )
}

/// Like [`add_nonresident_data_runs`], with the attribute name as raw UTF-16
/// code units, so a name can hold an unpaired surrogate.
#[allow(clippy::too_many_arguments)]
pub fn add_nonresident_data_runs_raw(
    record: &mut [u8],
    offset: usize,
    attribute_type: NtfsAttributeType,
    name: &[u16],
    lowest_vcn: u64,
    highest_vcn: u64,
    data_size: u64,
    runs: &[u8],
) -> usize {
    const HEADER: usize = NONRESIDENT_HEADER;
    let runs_offset = align_to_eight(HEADER + name.len() * 2);
    let length = align_to_eight(runs_offset + runs.len() + 1);
    write_u32(record, offset, attribute_type as u32);
    write_u32(record, offset + 4, length as u32);
    record[offset + 8] = 1;
    record[offset + 9] = name.len() as u8;
    write_u16(record, offset + 10, HEADER as u16);
    write_u16(record, offset + 14, 1);
    write_u64(record, offset + 16, lowest_vcn);
    write_u64(record, offset + 24, highest_vcn);
    write_u16(record, offset + 32, runs_offset as u16);
    write_u64(
        record,
        offset + 40,
        data_size.next_multiple_of(CLUSTER_SIZE as u64),
    );
    write_u64(record, offset + 48, data_size);
    write_u64(record, offset + 56, data_size);
    for (index, &unit) in name.iter().enumerate() {
        write_u16(record, offset + HEADER + index * 2, unit);
    }
    record[offset + runs_offset..offset + runs_offset + runs.len()].copy_from_slice(runs);
    record[offset + runs_offset + runs.len()] = 0;
    offset + length
}

/// Mapping pairs (without the terminator) for a list of runs: each is a
/// length in clusters and either the LCN delta from the previous run or
/// `None` for a sparse run. A zero length encodes with a zero-width length
/// field, which the decoder rejects, so the property test can build that too.
pub fn encode_runs(runs: &[(u64, Option<i64>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for &(length, delta) in runs {
        let length_size = 8 - length.leading_zeros() as usize / 8;
        let delta_size = delta.map_or(0, |delta| {
            (1..=8usize)
                .find(|size| {
                    let shift = 64 - 8 * size;
                    (delta << shift) >> shift == delta
                })
                .expect("8 bytes always fit")
        });
        out.push((delta_size << 4 | length_size) as u8);
        out.extend_from_slice(&length.to_le_bytes()[..length_size]);
        if let Some(delta) = delta {
            out.extend_from_slice(&delta.to_le_bytes()[..delta_size]);
        }
    }
    out
}

/// Adds a minimal unnamed, non-resident `$DATA` attribute standing in for
/// `$MFT`'s own VCN-0 extent: one run at LCN 0, covering `clusters`
/// clusters. `read_data_fs` needs this to locate any extension record by
/// number at all; tests that aren't specifically about the shape of that
/// mapping use this so record numbers translate to the same byte position
/// their bytes are laid out at (`mft_position` is 0 in [`test_volume`]).
/// Tests exercising non-contiguous runs build their own instead.
pub fn add_identity_mft_data(record: &mut [u8], offset: usize, clusters: u8) -> usize {
    add_nonresident_data_runs(
        record,
        offset,
        NtfsAttributeType::Data,
        "",
        0,
        clusters as u64 - 1,
        clusters as u64 * CLUSTER_SIZE as u64,
        &[0x11, clusters, 0x00],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attribute::NtfsAttribute;
    use crate::data_run::DataRun;

    // The sizes written out above are the ones of the structs the crate reads the fixtures with.
    #[test]
    fn the_fixture_sizes_agree_with_the_crates_structs() {
        assert_eq!(size_of::<NtfsFileNameHeader>(), FILE_NAME_HEADER);
        assert_eq!(
            size_of::<NtfsNonResidentAttributeHeader>(),
            NONRESIDENT_HEADER
        );
        // The struct is the leading part of the value: the times and the attribute flags.
        assert!(size_of::<NtfsStandardInformation>() <= STANDARD_INFORMATION_SIZE);
    }

    // `encode_runs` is the inverse of the decoder, for the run shapes the
    // property test builds: a first run at an LCN, a backwards
    // delta, a sparse run, and multi-byte lengths and deltas.
    #[test]
    fn encode_runs_round_trips_through_the_decoder() {
        let cluster = CLUSTER_SIZE as u64;
        let runs = [
            (3, Some(10)),
            (300, None),
            (1, Some(-4)),
            (70_000, Some(200)),
            (2, Some(-100)),
        ];
        let mut record = new_record(FIRST_NORMAL_RECORD, 1, 0);
        let offset = add_nonresident_data_runs(
            &mut record,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::Data,
            "",
            0,
            0,
            0,
            &encode_runs(&runs),
        );
        let attribute = NtfsAttribute::new(&record[ATTRIBUTES_OFFSET..offset]).expect("attribute");
        let decoded = attribute
            .nonresident_extent_runs(&test_volume())
            .expect("runs");

        let mut lcn = 0i64;
        let expected: Vec<(u64, Option<u64>)> = runs
            .iter()
            .map(|&(length, delta)| {
                let start = delta.map(|delta| {
                    lcn += delta;
                    lcn as u64 * cluster
                });
                (length * cluster, start)
            })
            .collect();
        let decoded: Vec<(u64, Option<u64>)> = decoded
            .into_iter()
            .map(|run| match run {
                DataRun::Data { offset, length } => (length, Some(offset)),
                DataRun::Sparse { length } => (length, None),
            })
            .collect();
        assert_eq!(decoded, expected);
    }
}
