// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

// Property test of `parse_usn_records`: the input describes a buffer of USN records (version 2
// or 3, a name, an optional fault per record, a possible cut inside the last), laid out in the
// byte format `usn.rs` parses. A fault-free buffer must parse to exactly the described records;
// each fault has a known outcome (clean stop or failure), checked too. See `crate::property` for
// running it longer and replaying a failing seed.

use std::ffi::OsString;

use arbitrary::Arbitrary;

use crate::property::{list, property, show_on_replay};
use crate::usn::parse_usn_records;

const MAX_RECORDS: usize = 8;
const MAX_NAME_UNITS: usize = 12;
const V2_LAYOUT: usize = 64;
const V3_LAYOUT: usize = 80;
/// Where `FileName` starts in a V2 and a V3 record.
const V2_NAME_OFFSET: usize = 60;
const V3_NAME_OFFSET: usize = 76;

#[derive(Debug, Arbitrary)]
struct Buffer {
    /// The `USN` the buffer starts with; the parser skips it.
    next_usn: u64,
    #[arbitrary(with = list::<RecordSpec, MAX_RECORDS>)]
    records: Vec<RecordSpec>,
    /// Zero bytes after the last record, inside the returned length.
    padding: u8,
    /// Bytes of the last record left out of the returned length (1 up to its
    /// length), or none.
    cut: Option<u8>,
}

#[derive(Debug, Arbitrary)]
struct RecordSpec {
    version: Version,
    usn: i64,
    reason: u32,
    /// The fields beside `file_attributes`, which the parser does not return: filled with
    /// values of their own, so a misplaced offset reads the wrong ones.
    source_info: u32,
    security_id: u32,
    file_attributes: u32,
    /// The 64-bit references of a V2 record. A V3 record uses them as the low half of its 128-bit
    /// ids and `*_high` as the high half.
    file_id: u64,
    parent_id: u64,
    file_id_high: u64,
    parent_id_high: u64,
    time_stamp: TimeStamp,
    #[arbitrary(with = list::<u8, MAX_NAME_UNITS>)]
    name: Vec<u8>,
    fault: Option<Fault>,
}

/// The record's time as 100ns intervals since 1601. Raw values are mostly far past year 9999,
/// so the other variants put most cases near the epoch and the last representable instant,
/// where the clamping rules apply.
#[derive(Debug, Arbitrary)]
enum TimeStamp {
    /// Any value, negative included.
    Raw(i64),
    /// Seconds from the Unix epoch, either side.
    AroundEpoch(i32),
    /// Seconds from the last instant `time` can represent, either side.
    AroundEnd(i16),
}

impl TimeStamp {
    fn ticks(&self) -> i64 {
        match *self {
            TimeStamp::Raw(ticks) => ticks,
            TimeStamp::AroundEpoch(seconds) => {
                EPOCH_DIFFERENCE as i64 + seconds as i64 * TICKS_PER_SECOND
            }
            TimeStamp::AroundEnd(seconds) => {
                (MAX_UNIX_NANOS / 100) as i64
                    + EPOCH_DIFFERENCE as i64
                    + seconds as i64 * TICKS_PER_SECOND
            }
        }
    }
}

#[derive(Debug, Arbitrary)]
enum Version {
    V2,
    V3,
    /// A major version the parser does not know: skipped, not an error.
    Unknown(u8),
}

/// What is wrong with a record. Each stops the parse.
#[derive(Debug, Arbitrary)]
enum Fault {
    /// `RecordLength` zero: the parser stops, without error.
    LengthZero,
    /// `RecordLength` beyond the returned bytes.
    LengthPastEnd,
    /// `RecordLength` not a multiple of 8.
    LengthUnaligned,
    /// `RecordLength` smaller than the record's fixed part.
    LengthShort,
    /// The name runs past `RecordLength`.
    NameOutside,
}

fn name_units(name: &[u8]) -> Vec<u16> {
    let mut units = Vec::new();
    for &choice in name {
        match choice % 8 {
            0..=2 => units.push(b'a' as u16 + (choice % 8) as u16),
            3 => units.push(b' ' as u16),
            4 => units.push(0xD800),
            5 => units.extend_from_slice(&[0xD83D, 0xDE00]),
            6 => units.push(0x00E9),
            _ => units.push(0x4E2D),
        }
    }
    units
}

/// The name as an `OsString`: `from_wide` on Windows.
#[cfg(windows)]
fn os_string(units: &[u16]) -> OsString {
    use std::os::windows::ffi::OsStringExt;
    OsString::from_wide(units)
}

/// The name as an `OsString`, encoded here by hand (WTF-8: ordinary UTF-8 plus the three-byte
/// form for an unpaired surrogate) so the check shares no conversion with the parser. Lossless.
#[cfg(not(windows))]
fn os_string(units: &[u16]) -> OsString {
    let mut bytes = Vec::new();
    let mut index = 0;
    while index < units.len() {
        let unit = units[index] as u32;
        let scalar = match unit {
            0xD800..=0xDBFF => match units.get(index + 1) {
                Some(&low) if (0xDC00..=0xDFFF).contains(&low) => {
                    index += 1;
                    0x10000 + ((unit - 0xD800) << 10) + (low as u32 - 0xDC00)
                }
                _ => unit,
            },
            _ => unit,
        };
        index += 1;
        match scalar {
            0..=0x7F => bytes.push(scalar as u8),
            0x80..=0x7FF => {
                bytes.push(0xC0 | (scalar >> 6) as u8);
                bytes.push(0x80 | (scalar & 0x3F) as u8);
            }
            0x800..=0xFFFF => {
                bytes.push(0xE0 | (scalar >> 12) as u8);
                bytes.push(0x80 | ((scalar >> 6) & 0x3F) as u8);
                bytes.push(0x80 | (scalar & 0x3F) as u8);
            }
            _ => {
                bytes.push(0xF0 | (scalar >> 18) as u8);
                bytes.push(0x80 | ((scalar >> 12) & 0x3F) as u8);
                bytes.push(0x80 | ((scalar >> 6) & 0x3F) as u8);
                bytes.push(0x80 | (scalar & 0x3F) as u8);
            }
        }
    }
    // SAFETY: `bytes` is well-formed WTF-8, the encoding `OsString` uses.
    unsafe { OsString::from_encoded_bytes_unchecked(bytes) }
}

/// What the parser should return for a record, in plain integers so the check shares no code
/// with the parser.
#[derive(Debug, PartialEq, Eq)]
struct Expected {
    usn: i64,
    reason: u32,
    file_attributes: u32,
    name: OsString,
    file_id: u128,
    parent_id: u128,
    /// Nanoseconds since the Unix epoch.
    unix_nanos: i128,
}

/// 100ns intervals between 1601 and 1970.
const EPOCH_DIFFERENCE: i128 = 116_444_736_000_000_000;
const TICKS_PER_SECOND: i64 = 10_000_000;

/// The first and last instants `time` can represent (-9999-01-01T00:00:00Z and
/// 9999-12-31T23:59:59.999999999Z) in Unix nanoseconds; a record time outside them becomes the
/// nearer one.
const MIN_UNIX_NANOS: i128 = -377_705_116_800 * 1_000_000_000;
const MAX_UNIX_NANOS: i128 = 253_402_300_799_999_999_999;

/// The record bytes, and the record the parser should return for them.
fn build(spec: &RecordSpec) -> (Vec<u8>, Option<Expected>) {
    let units = name_units(&spec.name);
    let (major, layout, name_offset) = match spec.version {
        Version::V2 => (2, V2_LAYOUT, V2_NAME_OFFSET),
        Version::V3 => (3, V3_LAYOUT, V3_NAME_OFFSET),
        Version::Unknown(major) => (
            // 0 to 3 would be a real or missing version; pick another.
            4 + major as u16 % 200,
            V2_LAYOUT,
            V2_NAME_OFFSET,
        ),
    };
    let name_length = units.len() * 2;
    let length = (name_offset + name_length).next_multiple_of(8).max(layout);

    let mut bytes = vec![0u8; length];
    bytes[0..4].copy_from_slice(&(length as u32).to_le_bytes());
    bytes[4..6].copy_from_slice(&major.to_le_bytes());
    // The fields after the reference numbers: V2 has two 8-byte references
    // (usn at 24), V3 two 16-byte ones (usn at 40).
    let usn_at = if major == 3 { 40 } else { 24 };
    if major == 3 {
        bytes[8..16].copy_from_slice(&spec.file_id.to_le_bytes());
        bytes[16..24].copy_from_slice(&spec.file_id_high.to_le_bytes());
        bytes[24..32].copy_from_slice(&spec.parent_id.to_le_bytes());
        bytes[32..40].copy_from_slice(&spec.parent_id_high.to_le_bytes());
    } else {
        bytes[8..16].copy_from_slice(&spec.file_id.to_le_bytes());
        bytes[16..24].copy_from_slice(&spec.parent_id.to_le_bytes());
    }
    bytes[usn_at..usn_at + 8].copy_from_slice(&spec.usn.to_le_bytes());
    bytes[usn_at + 8..usn_at + 16].copy_from_slice(&spec.time_stamp.ticks().to_le_bytes());
    bytes[usn_at + 16..usn_at + 20].copy_from_slice(&spec.reason.to_le_bytes());
    bytes[usn_at + 20..usn_at + 24].copy_from_slice(&spec.source_info.to_le_bytes());
    bytes[usn_at + 24..usn_at + 28].copy_from_slice(&spec.security_id.to_le_bytes());
    bytes[usn_at + 28..usn_at + 32].copy_from_slice(&spec.file_attributes.to_le_bytes());
    bytes[name_offset - 4..name_offset - 2].copy_from_slice(&(name_length as u16).to_le_bytes());
    bytes[name_offset - 2..name_offset].copy_from_slice(&(name_offset as u16).to_le_bytes());
    for (index, unit) in units.iter().enumerate() {
        let at = name_offset + index * 2;
        bytes[at..at + 2].copy_from_slice(&unit.to_le_bytes());
    }

    let wide = |low: u64, high: u64| low as u128 | (high as u128) << 64;
    let expected = matches!(spec.version, Version::V2 | Version::V3).then(|| Expected {
        usn: spec.usn,
        reason: spec.reason,
        file_attributes: spec.file_attributes,
        name: os_string(&units),
        file_id: if major == 3 {
            wide(spec.file_id, spec.file_id_high)
        } else {
            spec.file_id as u128
        },
        parent_id: if major == 3 {
            wide(spec.parent_id, spec.parent_id_high)
        } else {
            spec.parent_id as u128
        },
        // The exact rule of `ntfs_to_unix_time`: a time before 1970 is the real date, one
        // outside `time`'s range clamps to its nearest limit. A negative `TimeStamp` reads as 0
        // (1601-01-01, not the epoch); the kernel never writes one.
        unix_nanos: ((spec.time_stamp.ticks().max(0) as i128 - EPOCH_DIFFERENCE) * 100)
            .clamp(MIN_UNIX_NANOS, MAX_UNIX_NANOS),
    });
    (bytes, expected)
}

/// Applies the fault to the record's header. `returned` is how many bytes the
/// buffer has from this record on, which `LengthPastEnd` must exceed.
fn apply(fault: &Fault, bytes: &mut [u8], version: &Version, returned: usize) {
    let length = bytes.len();
    let name_offset = match version {
        Version::V3 => V3_NAME_OFFSET,
        _ => V2_NAME_OFFSET,
    };
    let record_length = match fault {
        Fault::LengthZero => 0,
        Fault::LengthPastEnd => returned + 8,
        Fault::LengthUnaligned => length + 1,
        Fault::LengthShort => match version {
            Version::V3 => V3_LAYOUT - 8,
            _ => V2_LAYOUT - 8,
        },
        Fault::NameOutside => {
            // A name that starts at the end of the record and is not empty.
            bytes[name_offset - 4..name_offset - 2].copy_from_slice(&8u16.to_le_bytes());
            bytes[name_offset - 2..name_offset].copy_from_slice(&(length as u16).to_le_bytes());
            length
        }
    };
    bytes[0..4].copy_from_slice(&(record_length as u32).to_le_bytes());
}

/// The faults that mean something for a record of this version: an unknown
/// version has no fixed part or name to get wrong.
fn effective_fault(spec: &RecordSpec) -> Option<&Fault> {
    spec.fault.as_ref().filter(|fault| {
        !(matches!(spec.version, Version::Unknown(_))
            && matches!(fault, Fault::LengthShort | Fault::NameOutside))
    })
}

#[test]
fn described_usn_records_parse_to_what_the_description_says() {
    property(|u| {
        let input = Buffer::arbitrary(u)?;
        show_on_replay(&input);
        check_buffer(&input);
        Ok(())
    });
}

fn check_buffer(input: &Buffer) {
    let mut buffer = input.next_usn.to_le_bytes().to_vec();
    let mut expected = Vec::new();
    let mut outcome_is_error = false;
    let mut stopped = false;
    let mut last_start = buffer.len();
    let mut last_expected_len = 0;

    let records = &input.records;
    let built: Vec<(Vec<u8>, Option<Expected>)> = records.iter().map(build).collect();
    let total: usize = built.iter().map(|(bytes, _)| bytes.len()).sum();

    for (spec, (mut bytes, record)) in records.iter().zip(built) {
        last_start = buffer.len();
        last_expected_len = expected.len();
        if let Some(fault) = effective_fault(spec) {
            let returned = total + input.padding as usize - (buffer.len() - 8);
            apply(fault, &mut bytes, &spec.version, returned);
            if !stopped {
                match fault {
                    Fault::LengthZero => {}
                    _ => outcome_is_error = true,
                }
                stopped = true;
            }
        } else if !stopped {
            expected.extend(record);
        }
        buffer.extend(bytes);
    }
    buffer.extend(std::iter::repeat_n(0u8, input.padding as usize));

    // A cut inside the last record: fewer than 8 bytes of it left is a clean
    // stop, 8 or more is a record longer than what was returned. Only when no
    // earlier fault already decided the outcome.
    let mut returned = buffer.len();
    if let (Some(cut), false) = (input.cut, records.is_empty()) {
        if input.padding == 0 && !stopped {
            let record_length = buffer.len() - last_start;
            let left = record_length - (cut as usize % record_length).max(1);
            returned = last_start + left;
            expected.truncate(last_expected_len);
            if left >= 8 {
                outcome_is_error = true;
            }
        }
    }

    let result = parse_usn_records(&buffer[..returned]);
    match (result, outcome_is_error) {
        (Ok(records), false) => {
            let parsed: Vec<Expected> = records
                .into_iter()
                .map(|r| Expected {
                    usn: r.usn,
                    reason: r.reason.bits(),
                    file_attributes: r.file_attributes,
                    name: r.name,
                    file_id: r.file_id.as_u128(),
                    parent_id: r.parent_id.as_u128(),
                    unix_nanos: r.timestamp.unix_timestamp_nanos(),
                })
                .collect();
            assert_eq!(parsed, expected, "parsed records differ");
        }
        (Err(_), true) => {}
        (Ok(records), true) => panic!("expected an error, parsed {records:?}"),
        (Err(error), false) => panic!("expected {expected:?}, got the error {error}"),
    }
}
