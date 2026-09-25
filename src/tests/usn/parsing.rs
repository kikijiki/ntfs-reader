// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use crate::mft::test_records::assert_os_str_is;
use crate::usn::*;

// Byte offsets in `USN_RECORD_V2`, written out here rather than taken from `RecordV2`, so a
// wrong struct cannot make the tests agree with it. Name starts at 60, size 64.
const V2_FILE_REFERENCE: usize = 8;
const V2_PARENT_REFERENCE: usize = 16;
const V2_USN: usize = 24;
const V2_TIME_STAMP: usize = 32;
const V2_REASON: usize = 40;
const V2_NAME_LENGTH: usize = 56;
const V2_NAME_OFFSET: usize = 58;
const V2_NAME: usize = 60;

fn put(bytes: &mut [u8], at: usize, value: &[u8]) {
    bytes[at..at + value.len()].copy_from_slice(value);
}

/// A 64-byte V2 record with the header fields the parser reads set and everything else zero.
fn v2_record(record_len: u32, major_version: u16, usn: i64, file_ref: u64) -> Vec<u8> {
    let mut record = vec![0u8; 64];
    put(&mut record, 0, &record_len.to_le_bytes());
    put(&mut record, 4, &major_version.to_le_bytes());
    put(&mut record, V2_FILE_REFERENCE, &file_ref.to_le_bytes());
    put(&mut record, V2_USN, &usn.to_le_bytes());
    put(&mut record, V2_NAME_OFFSET, &(V2_NAME as u16).to_le_bytes());
    record
}

/// A V2 record whose name is `name`, laid out the way the kernel does.
fn v2_with_name(name: &[u16]) -> Vec<u8> {
    let length = (V2_NAME + name.len() * 2).next_multiple_of(8);
    let mut record = v2_record(length as u32, 2, 1, 1);
    record.resize(length, 0);
    put(
        &mut record,
        V2_NAME_LENGTH,
        &((name.len() * 2) as u16).to_le_bytes(),
    );
    for (index, unit) in name.iter().enumerate() {
        put(&mut record, V2_NAME + index * 2, &unit.to_le_bytes());
    }
    record
}

/// The bytes an FSCTL returns: the next USN, then `records`.
fn buffer_of(records: &[Vec<u8>]) -> Vec<u8> {
    let mut buffer = vec![0u8; 8];
    for record in records {
        buffer.extend_from_slice(record);
    }
    buffer
}

#[test]
fn a_v2_record_parses_to_its_fields() {
    // 2023-11-14T22:13:20Z, 1_700_000_000 s after the epoch, as a FILETIME.
    let ticks = 116_444_736_000_000_000u64 + 1_700_000_000 * 10_000_000;
    let name: Vec<u16> = "a.txt".encode_utf16().collect();
    let mut record = v2_with_name(&name);
    put(
        &mut record,
        V2_FILE_REFERENCE,
        &0x0007_0000_0000_0123u64.to_le_bytes(),
    );
    put(
        &mut record,
        V2_PARENT_REFERENCE,
        &0x0001_0000_0000_0005u64.to_le_bytes(),
    );
    put(&mut record, V2_USN, &100i64.to_le_bytes());
    put(&mut record, V2_TIME_STAMP, &ticks.to_le_bytes());
    put(&mut record, V2_REASON, &0x8000_0100u32.to_le_bytes());

    let records = parse_usn_records(&buffer_of(&[record])).expect("valid record");

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].usn, 100);
    assert_eq!(
        records[0].timestamp,
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    );
    assert_eq!(records[0].file_id, FileId::from(0x0007_0000_0000_0123u64));
    assert_eq!(records[0].parent_id, FileId::from(0x0001_0000_0000_0005u64));
    assert_eq!(records[0].reason, Reason::FILE_CREATE | Reason::CLOSE);
    assert_eq!(records[0].name, "a.txt");
}

// The record's own attributes reach the caller from both V2 and V3: after `FILE_DELETE` they
// are the only sign of whether a directory or a file went away. Neighbouring fields hold other
// values, so a misplaced offset reads the wrong one.
#[test]
fn a_record_carries_its_file_attributes() {
    // FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED.
    const ATTRIBUTES: u32 = 0x0000_2010;

    let mut v2 = v2_record(64, 2, 1, 1);
    put(&mut v2, 44, &0x1111u32.to_le_bytes()); // SourceInfo
    put(&mut v2, 48, &0x2222u32.to_le_bytes()); // SecurityId
    put(&mut v2, 52, &ATTRIBUTES.to_le_bytes());

    let mut v3 = vec![0u8; 80];
    put(&mut v3, 0, &80u32.to_le_bytes());
    put(&mut v3, 4, &3u16.to_le_bytes());
    put(&mut v3, 60, &0x1111u32.to_le_bytes()); // SourceInfo
    put(&mut v3, 64, &0x2222u32.to_le_bytes()); // SecurityId
    put(&mut v3, 68, &ATTRIBUTES.to_le_bytes());

    let records = parse_usn_records(&buffer_of(&[v2, v3])).expect("valid records");

    assert_eq!(records.len(), 2);
    assert_eq!(records[0].file_attributes, ATTRIBUTES, "V2");
    assert_eq!(records[1].file_attributes, ATTRIBUTES, "V3");
}

// The same file compares equal whether a V2 record (64-bit reference) or a V3 record (128-bit
// id, the reference zero-extended, little-endian) named it.
#[test]
fn a_v3_record_id_equals_the_v2_id_of_the_same_file() {
    let reference = 0x0007_0000_0000_0123u64;
    let v2 = v2_record(64, 2, 1, reference);

    let mut v3 = vec![0u8; 80];
    v3[0..4].copy_from_slice(&80u32.to_le_bytes());
    v3[4..6].copy_from_slice(&3u16.to_le_bytes());
    v3[8..16].copy_from_slice(&reference.to_le_bytes()); // low half of FileReferenceNumber
    v3[24..32].copy_from_slice(&5u64.to_le_bytes()); // parent

    let from_v2 = parse_usn_records(&buffer_of(&[v2])).expect("v2");
    let from_v3 = parse_usn_records(&buffer_of(&[v3])).expect("v3");

    assert_eq!(from_v2[0].file_id, from_v3[0].file_id);
    assert_eq!(from_v3[0].file_id, FileId::from(u128::from(reference)));
    assert_eq!(from_v3[0].parent_id, FileId::from(5u64));
}

#[test]
fn a_v3_id_keeps_all_128_bits() {
    let mut v3 = vec![0u8; 80];
    v3[0..4].copy_from_slice(&80u32.to_le_bytes());
    v3[4..6].copy_from_slice(&3u16.to_le_bytes());
    let id: [u8; 16] = std::array::from_fn(|i| i as u8 + 1);
    v3[8..24].copy_from_slice(&id);

    let records = parse_usn_records(&buffer_of(&[v3])).expect("v3");

    assert_eq!(records[0].file_id.as_u128(), u128::from_le_bytes(id));
    assert_eq!(records[0].file_id.as_reference(), None);
}

// A USN record's name is a raw UTF-16 sequence, not guaranteed valid, and the parsed name must
// be exactly that, unpaired surrogate included, or it cannot be matched against a name read
// from the MFT.
#[test]
fn a_name_with_an_unpaired_surrogate_is_kept() {
    // "bad", a lone high surrogate, "name".
    let units = [0x62, 0x61, 0x64, 0xD800, 0x6E, 0x61, 0x6D, 0x65];
    let buffer = buffer_of(&[v2_with_name(&units)]);

    let records = parse_usn_records(&buffer).expect("valid record");

    assert_eq!(records.len(), 1);
    // Not a U+FFFD substitute: the lone surrogate is WTF-8's three bytes ED A0 80.
    assert_os_str_is(&records[0].name, &units, b"bad\xED\xA0\x80name");
}

#[test]
fn a_name_length_not_bounded_by_the_record_length_is_rejected() {
    // A record whose real content has a 2-char name, but FileNameLength lies and claims 100.
    // Bytes following it in the buffer (whatever they are) must not be read as part of the name.
    let mut record = v2_with_name(&"ab".encode_utf16().collect::<Vec<_>>());
    put(&mut record, V2_NAME_LENGTH, &200u16.to_le_bytes());
    let mut buffer = buffer_of(&[record]);
    buffer.resize(512, 0xAA);

    assert!(
        parse_usn_records(&buffer).is_err(),
        "a FileNameLength beyond the record's RecordLength must be rejected"
    );
}

// `RecordLength` is validated (bounds, multiple of 8, minimum size for the version) via
// `read_unaligned` before any field is trusted, so a corrupt one is rejected, not misread.
#[test]
fn a_record_length_that_is_not_a_multiple_of_8_is_rejected() {
    // Two correctly laid out 64-byte V2 records back to back, but the first's header lies about
    // its RecordLength (61). Once a record's own length is corrupt there is no reliable way to
    // find the next record, so the whole read is rejected.
    let mut record1 = v2_record(64, 2, 100, 100);
    record1[0..4].copy_from_slice(&61u32.to_le_bytes());
    let record2 = v2_record(64, 2, 200, 200);

    assert!(
        parse_usn_records(&buffer_of(&[record1, record2])).is_err(),
        "a RecordLength that is not a multiple of 8 must be rejected"
    );
}

#[test]
fn a_record_length_past_the_returned_bytes_is_rejected() {
    let record = v2_record(64, 2, 1, 1);
    let buffer = buffer_of(&[record]);

    assert!(parse_usn_records(&buffer[..buffer.len() - 8]).is_err());
}

#[test]
fn a_record_shorter_than_its_version_is_rejected() {
    let mut record = vec![0u8; 16];
    record[0..4].copy_from_slice(&16u32.to_le_bytes());
    record[4..6].copy_from_slice(&2u16.to_le_bytes());

    assert!(parse_usn_records(&buffer_of(&[record])).is_err());
}

#[test]
fn a_zero_record_length_ends_the_parse_without_error() {
    let first = v2_record(64, 2, 1, 1);
    let records = parse_usn_records(&buffer_of(&[first, vec![0u8; 64]])).expect("stops");

    assert_eq!(records.len(), 1);
}

#[test]
fn an_unknown_major_version_is_skipped() {
    let unknown = v2_record(64, 9, 1, 1);
    let known = v2_record(64, 2, 2, 2);

    let records = parse_usn_records(&buffer_of(&[unknown, known])).expect("skips");

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].usn, 2);
}

#[test]
fn a_buffer_with_only_the_leading_usn_has_no_records() {
    assert!(parse_usn_records(&[0u8; 8]).expect("empty").is_empty());
    assert!(parse_usn_records(&[]).expect("empty").is_empty());
}

#[test]
fn reason_contains_needs_all_flags_and_intersects_any() {
    let reason = Reason::FILE_CREATE | Reason::CLOSE;

    assert!(reason.contains(Reason::FILE_CREATE));
    assert!(reason.contains(Reason::FILE_CREATE | Reason::CLOSE));
    assert!(!reason.contains(Reason::FILE_CREATE | Reason::FILE_DELETE));
    assert!(reason.intersects(Reason::FILE_CREATE | Reason::FILE_DELETE));
    assert!(!reason.intersects(Reason::FILE_DELETE));
    assert!(reason.contains(Reason::EMPTY));
    assert!(!reason.intersects(Reason::EMPTY));
}

// The flags are listed in alphabetical order, not bit order.
#[test]
fn reason_display_lists_set_flags_by_name() {
    let reason = Reason::FILE_CREATE | Reason::CLOSE;

    assert_eq!(reason.to_string(), "CLOSE FILE_CREATE");
}

// Each row: a `TimeStamp` and the time it must read as, worked out by hand from
// 1601-01-01 = -11_644_473_600 s (FILETIME 0) and 1970-01-01 = FILETIME 116_444_736_000_000_000.
#[test]
fn a_timestamp_is_the_real_date_and_clamps_only_out_of_range() {
    const EPOCH: i64 = 116_444_736_000_000_000;
    // 9999-12-31T23:59:59.9999999Z, the last 100 ns tick `OffsetDateTime` holds.
    const LAST_TICK: i64 = 2_650_467_743_999_999_999;
    let cases: [(&str, i64, i128); 8] = [
        (
            "FILETIME 0 is 1601-01-01",
            0,
            -11_644_473_600 * 1_000_000_000,
        ),
        (
            "1960-01-01 plus 1234567 ticks, before the epoch",
            EPOCH - 315_619_200 * 10_000_000 + 1_234_567,
            -315_619_200 * 1_000_000_000 + 123_456_700,
        ),
        ("one tick before the epoch", EPOCH - 1, -100),
        ("the epoch", EPOCH, 0),
        (
            "the last tick of 9999",
            LAST_TICK,
            253_402_300_799_999_999_900,
        ),
        (
            "one tick past 9999 clamps to the last instant",
            LAST_TICK + 1,
            253_402_300_799_999_999_999,
        ),
        (
            "the largest value clamps to the last instant",
            i64::MAX,
            253_402_300_799_999_999_999,
        ),
        (
            "a negative value reads as 1601-01-01",
            -1,
            -11_644_473_600 * 1_000_000_000,
        ),
    ];

    for (what, ticks, unix_nanos) in cases {
        let mut record = v2_record(64, 2, 1, 1);
        put(&mut record, V2_TIME_STAMP, &ticks.to_le_bytes());

        let records = parse_usn_records(&buffer_of(&[record])).expect("valid record");

        assert_eq!(
            records[0].timestamp,
            OffsetDateTime::from_unix_timestamp_nanos(unix_nanos).unwrap(),
            "{what}"
        );
    }
}
