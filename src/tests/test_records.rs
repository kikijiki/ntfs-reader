// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

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

// These tests pin the fixture builders against byte dumps written by hand from the documented
// NTFS layouts, so the builders and the crate's own readers cannot drift apart. Layouts covered:
// the FILE record header, the resident and non-resident attribute headers,
// `$STANDARD_INFORMATION`, `$FILE_NAME` and `$ATTRIBUTE_LIST` values, and the mapping pairs of a
// run list.

#[track_caller]
fn assert_bytes(actual: &[u8], expected: &[u8], what: &str) {
    assert!(
        actual == expected,
        "{what}\n  actual   {actual:02x?}\n  expected {expected:02x?}"
    );
}

#[test]
fn a_new_record_has_the_header_ntfs_writes() {
    let record = new_record(0x1234, 7, reference(3, 30));
    #[rustfmt::skip]
        let header: [u8; 56] = [
            b'F', b'I', b'L', b'E',                         // 0x00 signature
            0x30, 0x00,                                     // 0x04 update sequence array at 0x30
            0x03, 0x00,                                     // 0x06 3 entries: the number and 2 sectors
            0, 0, 0, 0, 0, 0, 0, 0,                         // 0x08 log sequence number
            0x07, 0x00,                                     // 0x10 sequence number
            0x01, 0x00,                                     // 0x12 hard link count
            0x38, 0x00,                                     // 0x14 first attribute at 0x38
            0x01, 0x00,                                     // 0x16 flags: in use
            0, 0, 0, 0,                                     // 0x18 used size (set when finished)
            0x00, 0x04, 0x00, 0x00,                         // 0x1C allocated size 1024
            30, 0, 0, 0, 0, 0, 3, 0,                        // 0x20 base reference 3/30
            0, 0,                                           // 0x28 next attribute id
            0, 0,                                           // 0x2A
            0x34, 0x12, 0, 0,                               // 0x2C record number
            0, 0, 0, 0, 0, 0,                               // 0x30 the update sequence array
            0, 0,                                           // 0x36 padding to the first attribute
        ];
    assert_bytes(&record[..56], &header, "the record header");
    assert!(record[56..].iter().all(|&byte| byte == 0));
    assert_eq!(record.len(), 1024);
}

#[test]
fn finishing_a_record_protects_its_sector_ends_the_way_ntfs_does() {
    let mut record = new_record(24, 1, 0);
    let end = add_end_marker(&mut record, ATTRIBUTES_OFFSET);
    record[510..512].copy_from_slice(&[0xAA, 0xBB]);
    record[1022..1024].copy_from_slice(&[0xCC, 0xDD]);
    finish_record(&mut record, end);
    // The used size is 64: the header, then the 8 byte end marker.
    assert_eq!(&record[0x18..0x1C], &[64, 0, 0, 0]);
    // The array holds the update sequence number (1) and the two saved sector ends, and every
    // sector ends with the number.
    assert_bytes(
        &record[0x30..0x36],
        &[0x01, 0x00, 0xAA, 0xBB, 0xCC, 0xDD],
        "the update sequence array",
    );
    assert_eq!(&record[510..512], &[0x01, 0x00]);
    assert_eq!(&record[1022..1024], &[0x01, 0x00]);
    // The end marker is 0xFFFFFFFF and the 4 bytes NTFS leaves after it.
    assert_bytes(
        &record[56..64],
        &[0xFF, 0xFF, 0xFF, 0xFF, 0x82, 0x79, 0x47, 0x11],
        "the end marker",
    );
}

#[test]
fn finishing_a_record_without_an_end_marker_adds_the_one_ntfs_writes() {
    let mut record = new_record(24, 1, 0);
    let end = add_standard_information(&mut record, ATTRIBUTES_OFFSET, 0x20);
    finish_record(&mut record, end);
    // 56 + 0x60 + 8, and the marker where the attribute ended.
    assert_eq!(&record[0x18..0x1C], &(56u32 + 0x60 + 8).to_le_bytes());
    let marker = &record[end..end + 8];
    assert_bytes(
        marker,
        &[0xFF, 0xFF, 0xFF, 0xFF, 0x82, 0x79, 0x47, 0x11],
        "the end marker",
    );
    // A record that has its marker is not given a second one.
    let mut record = new_record(24, 1, 0);
    let end = add_end_marker(&mut record, ATTRIBUTES_OFFSET);
    finish_record(&mut record, end);
    assert_eq!(&record[0x18..0x1C], &64u32.to_le_bytes());
}

#[test]
fn a_standard_information_attribute_is_the_ntfs_layout() {
    let mut record = new_record(24, 1, 0);
    let end = add_standard_information_at(
        &mut record,
        ATTRIBUTES_OFFSET,
        0x20,
        [
            0x0102030405060708,
            0x1112131415161718,
            0x2122232425262728,
            0x3132333435363738,
        ],
    );
    // Resident, unnamed: 0x18 bytes of header and a 72 byte value, 0x60 in all.
    assert_eq!(end, ATTRIBUTES_OFFSET + 0x60);
    let attribute = &record[ATTRIBUTES_OFFSET..end];
    #[rustfmt::skip]
        let header: [u8; 24] = [
            0x10, 0x00, 0x00, 0x00,                         // type $STANDARD_INFORMATION
            0x60, 0x00, 0x00, 0x00,                         // length
            0x00,                                           // resident
            0x00,                                           // no name
            0x18, 0x00,                                     // name offset
            0x00, 0x00,                                     // flags
            0x00, 0x00,                                     // attribute id 0
            0x48, 0x00, 0x00, 0x00,                         // value length 72
            0x18, 0x00,                                     // value offset
            0x00, 0x00,                                     // indexed flag
        ];
    assert_bytes(&attribute[..24], &header, "the resident attribute header");
    // The value: created, modified, MFT modified, accessed, then the flags at 0x20.
    assert_bytes(
        &attribute[24..24 + 40],
        &[
            0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, //
            0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12, 0x11, //
            0x28, 0x27, 0x26, 0x25, 0x24, 0x23, 0x22, 0x21, //
            0x38, 0x37, 0x36, 0x35, 0x34, 0x33, 0x32, 0x31, //
            0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ],
        "the value",
    );
}

#[test]
fn a_file_name_attribute_is_the_ntfs_layout() {
    let mut record = new_record(24, 1, 0);
    let end = add_file_name_ex(
        &mut record,
        ATTRIBUTES_OFFSET,
        5,
        reference(2, 0x0501),
        NtfsFileNamespace::Win32,
        "ab\u{e9}",
        0x20,
    );
    // A 66 byte value and a 3 unit name: 24 + 72 = 96, and the attribute is 0x60 long.
    assert_eq!(end, ATTRIBUTES_OFFSET + 0x60);
    let attribute = &record[ATTRIBUTES_OFFSET..end];
    assert_bytes(
        &attribute[..24],
        &[
            0x30, 0, 0, 0, // type $FILE_NAME
            0x60, 0, 0, 0, // length
            0, 0, // resident, no name
            0x18, 0, // name offset
            0, 0, // flags
            5, 0, // attribute id
            0x48, 0, 0, 0, // value length 72
            0x18, 0, // value offset
            0, 0, // indexed flag
        ],
        "the attribute header",
    );
    let value = &attribute[24..];
    // The parent reference: record number in the low 48 bits, sequence above.
    assert_bytes(
        &value[..8],
        &[0x01, 0x05, 0, 0, 0, 0, 2, 0],
        "parent reference",
    );
    // The four times are 0x08..0x28, the allocated and real size 0x28 and 0x30 (34359738368
    // is 0x8_0000_0000), the flags 0x38, the length of the name in units 0x40, the namespace
    // 0x41 (1 is Win32) and the name from 0x42.
    assert_bytes(
        &value[0x28..0x30],
        &[0, 0, 0, 0, 8, 0, 0, 0],
        "allocated size",
    );
    assert_bytes(&value[0x30..0x38], &[0, 0, 0, 0, 8, 0, 0, 0], "real size");
    assert_bytes(&value[0x38..0x3C], &[0x20, 0, 0, 0], "flags");
    assert_bytes(
        &value[0x40..0x48],
        &[3, 1, b'a', 0, b'b', 0, 0xE9, 0x00],
        "name length, namespace and name",
    );
}

#[test]
fn a_non_resident_attribute_is_the_ntfs_layout() {
    let mut record = new_record(24, 1, 0);
    // `ads`: 3 units. Two runs: 0x18 clusters at cluster 0x5634, then 5 clusters 16 back.
    let runs = encode_runs(&[(0x18, Some(0x5634)), (5, Some(-16))]);
    let end = add_nonresident_data_runs(
        &mut record,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Data,
        "ads",
        0,
        0x1C,
        0x1C_100,
        &runs,
    );
    // The name follows the 0x40 byte header, the runs come at 0x48 (the name is 6 bytes, padded
    // to 8), and the whole thing is padded to 8: 0x48 + 7 + terminator = 0x50.
    assert_eq!(end, ATTRIBUTES_OFFSET + 0x50);
    let attribute = &record[ATTRIBUTES_OFFSET..end];
    #[rustfmt::skip]
        let expected: [u8; 0x50] = [
            0x80, 0, 0, 0,                                  // 0x00 type $DATA
            0x50, 0, 0, 0,                                  // 0x04 length
            0x01,                                           // 0x08 non-resident
            0x03,                                           // 0x09 name length
            0x40, 0x00,                                     // 0x0A name offset
            0x00, 0x00,                                     // 0x0C flags
            0x01, 0x00,                                     // 0x0E attribute id
            0, 0, 0, 0, 0, 0, 0, 0,                         // 0x10 lowest VCN 0
            0x1C, 0, 0, 0, 0, 0, 0, 0,                      // 0x18 highest VCN 0x1C
            0x48, 0x00,                                     // 0x20 runs at 0x48
            0x00, 0x00,                                     // 0x22 compression unit
            0, 0, 0, 0,                                     // 0x24
            0x00, 0xD0, 0x01, 0, 0, 0, 0, 0,                // 0x28 allocated: 29 clusters, 0x1D000
            0x00, 0xC1, 0x01, 0, 0, 0, 0, 0,                // 0x30 data size 0x1C100
            0x00, 0xC1, 0x01, 0, 0, 0, 0, 0,                // 0x38 initialized size 0x1C100
            b'a', 0, b'd', 0, b's', 0, 0, 0,                // 0x40 the name, then padding
            0x21, 0x18, 0x34, 0x56,                         // 0x48 run 1: 0x18 clusters at 0x5634
            0x11, 0x05, 0xF0,                               // run 2: 5 clusters, 16 back
            0x00,                                           // terminator
        ];
    assert_bytes(attribute, &expected, "the non-resident attribute");
}

// What NTFS leaves of a non-resident attribute when it deletes a file that has an attribute
// list: lowest VCN 0, highest VCN -1, allocated, data and initialized sizes 0, and a run list
// that is only the terminator.
#[test]
fn a_truncated_attribute_is_what_ntfs_leaves() {
    let mut record = new_record(24, 1, 0);
    let end =
        add_truncated_nonresident(&mut record, ATTRIBUTES_OFFSET, NtfsAttributeType::Data, "");
    assert_eq!(end, ATTRIBUTES_OFFSET + 0x48);
    let attribute = &record[ATTRIBUTES_OFFSET..end];
    #[rustfmt::skip]
        let expected: [u8; 0x48] = [
            0x80, 0, 0, 0,                                  // type $DATA
            0x48, 0, 0, 0,                                  // length: header and the terminator, padded
            0x01, 0x00,                                     // non-resident, no name
            0x40, 0x00,                                     // name offset
            0x00, 0x00,                                     // flags
            0x01, 0x00,                                     // attribute id
            0, 0, 0, 0, 0, 0, 0, 0,                         // lowest VCN 0
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // highest VCN -1
            0x40, 0x00,                                     // the run list is right after the header
            0x00, 0x00,
            0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0,                         // allocated size 0
            0, 0, 0, 0, 0, 0, 0, 0,                         // data size 0
            0, 0, 0, 0, 0, 0, 0, 0,                         // initialized size 0
            0x00, 0, 0, 0, 0, 0, 0, 0,                      // the terminator, padded
        ];
    assert_bytes(attribute, &expected, "the truncated attribute");
}

// The example run list of the NTFS documentation, `21 18 34 56 00`, with a run that goes
// backwards and a hole.
#[test]
fn mapping_pairs_are_the_ntfs_encoding() {
    assert_bytes(
        &encode_runs(&[(0x18, Some(0x5634))]),
        &[0x21, 0x18, 0x34, 0x56],
        "the documented example",
    );
    assert_bytes(
        &encode_runs(&[(0x18, Some(0x5634)), (5, Some(-16)), (8, None)]),
        &[0x21, 0x18, 0x34, 0x56, 0x11, 0x05, 0xF0, 0x01, 0x08],
        "a run 16 clusters back and a hole",
    );
    // A delta that needs 3 bytes signed (0x8000 does not fit in 2), and a 2 byte length.
    assert_bytes(
        &encode_runs(&[(0x100, Some(0x8000))]),
        &[0x32, 0x00, 0x01, 0x00, 0x80, 0x00],
        "wide fields",
    );
}

#[test]
fn an_attribute_list_entry_is_the_ntfs_layout() {
    let entry = list_entry(NtfsAttributeType::Data, "ab", 0x10, 0x0102);
    // 26 bytes of header, a 4 byte name: 30, padded to 32.
    #[rustfmt::skip]
        let expected: [u8; 32] = [
            0x80, 0, 0, 0,                                  // 0x00 type $DATA
            0x20, 0x00,                                     // 0x04 entry length
            0x02,                                           // 0x06 name length in units
            0x1A,                                           // 0x07 name offset
            0x10, 0, 0, 0, 0, 0, 0, 0,                      // 0x08 starting VCN
            0x02, 0x01, 0, 0, 0, 0, 1, 0,                   // 0x10 the record holding it: 1/0x102
            0x00, 0x00,                                     // 0x18 attribute id
            b'a', 0, b'b', 0,                               // 0x1A name
            0, 0,
        ];
    assert_bytes(&entry, &expected, "the list entry");
}

// A delete changes only three header fields (measured): the log sequence number, the sequence
// number, and the in-use bit. An extension record keeps its base reference, with the base's
// sequence from while it was live.
#[test]
fn delete_record_changes_the_three_header_fields_ntfs_changes() {
    let mut base = new_record(24, 7, 0);
    let offset = add_standard_information(&mut base, ATTRIBUTES_OFFSET, 0x20);
    let offset = add_file_name(&mut base, offset, "a.txt", 0x20);
    finish_record(&mut base, offset);
    let mut extension = new_record(25, 4, reference(7, 24));
    let offset = add_file_name(&mut extension, ATTRIBUTES_OFFSET, "b", 0x20);
    finish_record(&mut extension, offset);

    for (record, sequence) in [(base, 7u16), (extension, 4)] {
        let mut gone = record.clone();
        delete_record(&mut gone);
        let changed: Vec<usize> = (0..RECORD_SIZE)
            .filter(|&at| gone[at] != record[at])
            .collect();
        // Bytes 0x08..0x10 (LSN), 0x10 (sequence, the low byte only) and 0x16 (flags).
        assert!(
            changed
                .iter()
                .all(|&at| (0x08..0x10).contains(&at) || at == 0x10 || at == 0x16),
            "changed bytes: {changed:x?}"
        );
        assert!(changed.contains(&0x10) && changed.contains(&0x16));
        assert!(
            changed.iter().any(|at| (0x08..0x10).contains(at)),
            "the LSN moved"
        );
        assert_eq!(u16::from_le_bytes([gone[16], gone[17]]), sequence + 1);
        assert_eq!(
            u16::from_le_bytes([gone[22], gone[23]]) & 1,
            0,
            "not in use"
        );
        assert_eq!(gone[32..40], record[32..40], "the base reference");
        assert_eq!(gone[24..28], record[24..28], "the used size");
    }
}

// `encode_runs` is the inverse of the decoder, for the run shapes the property test builds: a
// first run at an LCN, a backwards delta, a sparse run, and multi-byte lengths and deltas.
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
