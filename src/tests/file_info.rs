// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use super::*;
use crate::api::{EPOCH_DIFFERENCE, FIRST_NORMAL_RECORD};
use crate::mft::test_records::*;

// FileInfo::new must aggregate across the whole logical file
// even when handed an extension record directly, not just the base.
#[test]
fn file_info_on_an_extension_record_matches_the_base() {
    let base_number = FIRST_NORMAL_RECORD;
    let extension_number = FIRST_NORMAL_RECORD + 1;

    let mut base = new_record(base_number, 1, 0);
    let mut offset = add_file_name(&mut base, ATTRIBUTES_OFFSET, "split.bin", 0);
    offset = add_standard_information(&mut base, offset, 0x20);
    finish_record(&mut base, offset);

    let mut extension = new_record(extension_number, 1, reference(1, base_number));
    let offset = add_nonresident_data(&mut extension, ATTRIBUTES_OFFSET, 4096);
    finish_record(&mut extension, offset);

    let mft = mft_with(vec![base, extension]);
    let base_file = mft.record(base_number).expect("base record");
    let extension_file = mft.record(extension_number).expect("extension record");
    assert!(extension_file.is_extension());

    let from_base = FileInfo::new(&base_file);
    let from_extension = FileInfo::new(&extension_file);

    assert_eq!(from_extension.name, from_base.name);
    assert_eq!(from_extension.size, from_base.size);
    assert_eq!(from_extension.size, 4096);
    assert_eq!(from_extension.file_attributes, from_base.file_attributes);
}

// The directory flag lives only in the base record; FileInfo on an extension record must still
// report it.
#[test]
fn file_info_on_an_extension_record_of_a_directory_says_directory() {
    let base_number = FIRST_NORMAL_RECORD;
    let extension_number = FIRST_NORMAL_RECORD + 1;

    let mut base = new_record(base_number, 1, 0);
    set_record_flags(&mut base, directory_flags());
    let offset = add_standard_information(&mut base, ATTRIBUTES_OFFSET, 0x10);
    finish_record(&mut base, offset);

    // The directory's only name spilled into an extension record.
    let mut extension = new_record(extension_number, 1, reference(1, base_number));
    let offset = add_file_name(&mut extension, ATTRIBUTES_OFFSET, "spilled-dir", 0x10);
    finish_record(&mut extension, offset);

    let mft = mft_with(vec![base, extension]);
    let base_file = mft.record(base_number).expect("base record");
    let extension_file = mft.record(extension_number).expect("extension record");
    assert!(extension_file.is_extension());
    assert!(
        !extension_file.is_directory(),
        "the extension record carries no directory flag"
    );

    assert!(FileInfo::new(&base_file).is_directory);
    let from_extension = FileInfo::new(&extension_file);
    assert_eq!(from_extension.name, "spilled-dir");
    assert!(from_extension.is_directory);
}

/// FILETIME ticks (100 ns) `secs` seconds and `ticks` ticks after the Unix epoch.
fn filetime(secs: i64, ticks: u64) -> u64 {
    ((EPOCH_DIFFERENCE as i64 + secs * 10_000_000) as u64) + ticks
}

// T2: pins which stored time maps to which field. Four distinct times per file, each with a
// sub-second part, checked as Unix nanoseconds worked out by hand, through FileInfo and through
// the standard information accessors. One file has times before 1970.
#[test]
fn each_standard_information_time_reaches_its_own_field() {
    let modern = [
        filetime(1_000_000_000, 1_234_567),
        filetime(1_100_000_000, 2_345_678),
        filetime(1_300_000_000, 4_567_890),
        filetime(1_200_000_000, 3_456_789),
    ];
    // created, modified, MFT modified, accessed
    let modern_nanos: [i128; 4] = [
        1_000_000_000_123_456_700,
        1_100_000_000_234_567_800,
        1_300_000_000_456_789_000,
        1_200_000_000_345_678_900,
    ];
    // 1960-01-01T00:00:00Z is 315_619_200 s before the epoch; each time a
    // second and a tick earlier than the one before.
    let old = [
        filetime(-315_619_200, 0),
        filetime(-315_619_201, 5),
        filetime(-315_619_203, 7),
        filetime(-315_619_202, 9),
    ];
    let old_nanos: [i128; 4] = [
        -315_619_200_000_000_000,
        -315_619_200_999_999_500,
        -315_619_202_999_999_300,
        -315_619_201_999_999_100,
    ];

    let mut records = Vec::new();
    for (index, times) in [modern, old].into_iter().enumerate() {
        let mut record = new_record(FIRST_NORMAL_RECORD + index as u64, 1, 0);
        let mut offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, "t.txt", 0);
        offset = add_standard_information_at(&mut record, offset, 0x20, times);
        finish_record(&mut record, offset);
        records.push(record);
    }
    let mut without = new_record(FIRST_NORMAL_RECORD + 2, 1, 0);
    let offset = add_file_name(&mut without, ATTRIBUTES_OFFSET, "none.txt", 0);
    finish_record(&mut without, offset);
    records.push(without);
    let mft = mft_with(records);

    for (index, expected) in [modern_nanos, old_nanos].into_iter().enumerate() {
        let file = mft
            .record(FIRST_NORMAL_RECORD + index as u64)
            .expect("record");
        let info = FileInfo::new(&file);
        let nanos = |time: Option<OffsetDateTime>| time.map(OffsetDateTime::unix_timestamp_nanos);
        assert_eq!(nanos(info.created), Some(expected[0]), "created");
        assert_eq!(nanos(info.modified), Some(expected[1]), "modified");
        assert_eq!(nanos(info.accessed), Some(expected[3]), "accessed");
        assert_eq!(info.file_attributes, 0x20);

        let standard = file.standard_information().expect("standard information");
        assert_eq!(standard.created().unix_timestamp_nanos(), expected[0]);
        assert_eq!(standard.modified().unix_timestamp_nanos(), expected[1]);
        assert_eq!(standard.mft_modified().unix_timestamp_nanos(), expected[2]);
        assert_eq!(standard.accessed().unix_timestamp_nanos(), expected[3]);
        assert_eq!(standard.file_attributes(), 0x20);
    }

    let file = mft.record(FIRST_NORMAL_RECORD + 2).expect("record");
    let info = FileInfo::new(&file);
    assert_eq!(
        (info.created, info.modified, info.accessed),
        (None, None, None)
    );
    assert_eq!(info.file_attributes, 0);
    assert!(file.standard_information().is_none());
}
