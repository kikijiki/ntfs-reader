// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! Synthetic MFT records for unit tests and the property test, and (under
//! the `internals` feature) for `benches/*_synthetic.rs`. A child of `mft`
//! so it can build an `Mft` without a volume. Not every caller uses every
//! helper.
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
/// right after the header of an NTFS 3.1 record (record number at offset 44).
pub const UPDATE_SEQUENCE_OFFSET: usize = 48;
/// Where the first attribute of a record built by [`new_record`] starts:
/// after the update sequence array (a sequence number plus one saved value
/// per sector, 6 bytes for the record's two sectors), aligned to 8.
pub const ATTRIBUTES_OFFSET: usize = 56;
/// Path of [`test_volume`].
pub const VOLUME_PATH: &str = r"\\.\T:";
/// The update sequence number [`finish_record`] protects records with.
pub const UPDATE_SEQUENCE_NUMBER: u16 = 1;

// On-disk sizes of three structures, written out instead of taken from the
// crate's structs with `size_of`, so a wrongly sized struct cannot make the
// fixtures agree with it.
/// A `$STANDARD_INFORMATION` value of NTFS 3.x (the crate's struct reads the first 36 bytes).
const STANDARD_INFORMATION_SIZE: usize = 72;
/// The fixed part of a `$FILE_NAME` value, before the name.
const FILE_NAME_HEADER: usize = 66;
/// The header of a non-resident attribute, before its name and its data runs.
const NONRESIDENT_HEADER: usize = 64;

/// Record number [`mft_with`] places its first record at (later records are
/// numbered by vector position). Copies out `crate::api::FIRST_NORMAL_RECORD`
/// (`pub(crate)`) for external callers (`benches/*_synthetic.rs`) that need
/// to predict record numbers, e.g. to link a directory chain's parents.
pub const FIRST_RECORD: u64 = FIRST_NORMAL_RECORD;

/// Sequence number of the synthetic root record `mft_with` places at
/// [`ROOT_RECORD`]. Names built by [`add_file_name`] default to this
/// sequence, so `resolve_path`'s root check (full reference against the
/// root's own `reference()`) finds a real, matching root.
pub const ROOT_SEQUENCE: u16 = 1;

/// Asserts that `text` holds exactly the UTF-16 code units `units`, checking
/// the platform's own representation rather than converting through the
/// crate: the units themselves on Windows, `wtf8` (what the crate writes
/// for a lone surrogate elsewhere) on other platforms.
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
/// [`ROOT_SEQUENCE`]) for [`mft_with`] to place at its fixed position.
pub fn root_record() -> Vec<u8> {
    let mut root = new_record(ROOT_RECORD, ROOT_SEQUENCE, 0);
    set_record_flags(&mut root, directory_flags());
    let offset = add_end_marker(&mut root, ATTRIBUTES_OFFSET);
    finish_record(&mut root, offset);
    root
}

/// A volume with 4 KiB clusters, 1 KiB records, `$MFT` at byte 0, and an
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

/// Raw `(volume, data, bitmap)` inputs to `Mft::from_parts`: the layout
/// [`mft_with`] builds (records from [`FIRST_RECORD`], plus a root record
/// at [`ROOT_RECORD`], see [`root_record`]), without fixups or extension
/// record indexing. Lets a bench build the input once and repeatedly time
/// [`build_from_parts`] alone, on a fresh clone of `data`/`bitmap`.
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

/// Applies fixups and indexes extension records: the work `Mft::new` does
/// after reading a volume's `$MFT`. `Mft::from_parts` is crate-private, so
/// this is the entry point a bench uses to time it (Card 008 made this step
/// about 25x faster; nothing measured that until now).
pub fn build_from_parts(volume: Volume, data: Vec<u8>, bitmap: Vec<u8>) -> Mft {
    Mft::from_parts(volume, data, bitmap).expect("synthetic MFT")
}

/// Places `records` at consecutive numbers from [`FIRST_RECORD`], plus a
/// root record at [`ROOT_RECORD`] (see [`root_record`]) so names resolving
/// up to the root (the default for [`add_file_name`]) find a real record.
pub fn mft_with(records: Vec<Vec<u8>>) -> Mft {
    let (volume, data, bitmap) = raw_parts(records);
    build_from_parts(volume, data, bitmap)
}

/// Places each record at its given number, unlike [`mft_with`], which
/// always starts at [`FIRST_NORMAL_RECORD`]. Needed below that (e.g. the
/// root directory at record 5).
pub fn mft_with_at(records: Vec<(u64, Vec<u8>)>) -> Mft {
    mft_with_at_freed(records, &[])
}

/// Like [`mft_with`], with the `$BITMAP` bit of each number in `freed`
/// clear, as a delete leaves it. The record's own in-use flag is separate:
/// see [`mark_freed`].
pub fn mft_with_freed(records: Vec<Vec<u8>>, freed: &[u64]) -> Mft {
    let (volume, data, mut bitmap) = raw_parts(records);
    clear_bitmap_bits(&mut bitmap, freed);
    build_from_parts(volume, data, bitmap)
}

/// Like [`mft_with_at`], with the `$BITMAP` bit of each record number in
/// `freed` clear.
pub fn mft_with_at_freed(records: Vec<(u64, Vec<u8>)>, freed: &[u64]) -> Mft {
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

    clear_bitmap_bits(&mut bitmap, freed);

    Mft::from_parts(test_volume(), data, bitmap).expect("synthetic MFT")
}

fn clear_bitmap_bits(bitmap: &mut [u8], numbers: &[u64]) {
    for &number in numbers {
        bitmap[number as usize / 8] &= !(1 << (number % 8));
    }
}

/// Clears the in-use flag, as a delete does: the record keeps everything
/// else, directory bit included. Freeing also bumps the sequence number
/// (see [`new_record`]'s `sequence`) and clears the `$BITMAP` bit (see
/// [`mft_with_freed`]); the caller does those.
pub fn mark_freed(record: &mut [u8]) {
    let flags = u16::from_le_bytes([record[22], record[23]]);
    write_u16(record, 22, flags & !(NtfsFileFlags::InUse as u16));
}

/// What a delete does to a record's own bytes, as measured on NTFS 3.1: the
/// log sequence number (offset 8) moves, the sequence number (offset 16)
/// goes up by one, and the in-use flag goes off. Nothing else changes: not
/// the used size, hard link count, attributes, or an extension record's
/// base reference (kept as it was while live). The `$BITMAP` bit is the
/// volume's business, see [`mft_with_freed`]. Unlike [`mark_freed`], which
/// only clears the flag, this is the whole change: a deleted-file fixture
/// uses this one.
pub fn delete_record(record: &mut [u8]) {
    let lsn = u64::from_le_bytes(record[8..16].try_into().unwrap());
    write_u64(record, 8, lsn.wrapping_add(0x1000));
    let sequence = u16::from_le_bytes([record[16], record[17]]);
    write_u16(record, 16, sequence.wrapping_add(1));
    mark_freed(record);
}

/// A record header in the NTFS 3.1 layout: signature, a well-formed update
/// sequence array (offset [`UPDATE_SEQUENCE_OFFSET`], one entry per sector
/// plus the sequence number), record number at offset 44, in use, no
/// attributes. Add attributes, then call [`finish_record`] to set the used
/// size and apply the update sequence protection.
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

/// A `$STANDARD_INFORMATION` with four distinct times: created 1s, modified
/// 2s, MFT modified 4s, accessed 3s after the Unix epoch.
pub fn add_standard_information(record: &mut [u8], offset: usize, attributes: u32) -> usize {
    add_standard_information_at(
        record,
        offset,
        attributes,
        [10_000_000, 20_000_000, 40_000_000, 30_000_000].map(|ticks| EPOCH_DIFFERENCE + ticks),
    )
}

/// A `$STANDARD_INFORMATION` with the given FILETIMEs, in on-disk order:
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
/// [`ROOT_RECORD`] with sequence [`ROOT_SEQUENCE`], not the bare record
/// number (sequence 0, which would not match that root).
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
/// instead of `&str`. `str::encode_utf16` only ever produces well-formed
/// UTF-16 (a lone surrogate cannot appear in a valid `str`), but NTFS
/// accepts an unpaired surrogate as a real filename; this is how a test
/// builds one.
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
/// `lowest_vcn == 0` has a meaningful `size`.
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

/// A non-resident attribute as NTFS leaves it when deleting a file with an
/// `$ATTRIBUTE_LIST`: `lowest_vcn` 0, `highest_vcn` -1, allocated, data and
/// initialized sizes 0, and a run list that is only the terminator byte.
pub fn add_truncated_nonresident(
    record: &mut [u8],
    offset: usize,
    attribute_type: NtfsAttributeType,
    name: &str,
) -> usize {
    add_nonresident_data_runs(record, offset, attribute_type, name, 0, u64::MAX, 0, &[])
}

pub fn add_nonresident_data(record: &mut [u8], offset: usize, size: u64) -> usize {
    add_nonresident_attribute(record, offset, NtfsAttributeType::Data, 2, "", 0, size)
}

/// Writes the `End` marker as it is on disk (`0xFFFFFFFF`, then the 4 bytes
/// NTFS leaves after it, `0x11477982`) and returns the offset after it: 8
/// bytes, so a record with no attributes has a used size of 64.
pub fn add_end_marker(record: &mut [u8], offset: usize) -> usize {
    write_u32(record, offset, NtfsAttributeType::End as u32);
    write_u32(record, offset + 4, 0x1147_7982);
    offset + 8
}

/// Sets the record's used size and applies the update sequence protection
/// (see [`protect_record`]). Call it last: the record's sector ends move
/// into the update sequence array, so later writes over them are lost to
/// the fixup.
///
/// NTFS ends every record's attributes with the end marker, so a record
/// whose attributes were added without [`add_end_marker`] gets one here
/// (when it fits), its used size then including it, as on disk.
pub fn finish_record(record: &mut [u8], used_size: usize) {
    let marker = NtfsAttributeType::End as u32;
    let has_marker = used_size >= ATTRIBUTES_OFFSET + 8
        && record[used_size - 8..used_size - 4] == marker.to_le_bytes();
    let used_size = if has_marker || used_size + 8 > RECORD_SIZE {
        used_size
    } else {
        add_end_marker(record, used_size)
    };
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
/// `fixup_record`: sector ends move into the array, `usn` takes their
/// place. A record already protected (the array's number sits at every
/// sector end) is restored first, so this can be called again to change it.
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
    list_entry_to(attribute_type, name, starting_vcn, reference(1, record))
}

/// One `$ATTRIBUTE_LIST` entry pointing at record `target` (a reference:
/// NTFS writes the sequence the record had when the entry was made, one
/// less than it has once the file is deleted).
pub fn list_entry_to(
    attribute_type: NtfsAttributeType,
    name: &str,
    starting_vcn: u64,
    target: u64,
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
    write_u64(&mut entry, 16, target);
    for (index, unit) in name.into_iter().enumerate() {
        write_u16(&mut entry, NAME_OFFSET + index * 2, unit);
    }
    entry
}

/// A non-resident attribute extent covering `lowest_vcn..=highest_vcn` with
/// the given mapping pairs (terminator added). Unlike
/// [`add_nonresident_attribute`], which writes a single implicit extent
/// from a size alone, this builds explicit data runs bytes, for tests
/// exercising `$MFT`'s own `$DATA` extent mapping.
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
/// field, which the decoder rejects, so the property test can build that
/// too.
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
/// number at all; tests not specifically about that mapping's shape use it
/// so record numbers translate to the byte position their bytes are laid
/// out at (`mft_position` is 0 in [`test_volume`]). Tests exercising
/// non-contiguous runs build their own instead.
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
#[path = "tests/test_records.rs"]
mod tests;
