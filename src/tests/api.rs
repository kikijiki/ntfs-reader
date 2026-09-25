// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use super::*;

/// FILETIME ticks (100 ns) per second.
const TICKS_PER_SECOND: u64 = 10_000_000;

fn unix_nanos(time: OffsetDateTime) -> i128 {
    time.unix_timestamp_nanos()
}

// Each row: a FILETIME and the Unix time in nanoseconds it stands for,
// worked out by hand from 1601-01-01 = -11_644_473_600 s.
#[test]
fn ntfs_to_unix_time_is_exact() {
    const EPOCH_1601_NANOS: i128 = -11_644_473_600 * 1_000_000_000;
    let cases: [(&str, u64, i128); 5] = [
        ("FILETIME 0 is 1601-01-01", 0, EPOCH_1601_NANOS),
        ("one tick before the Unix epoch", EPOCH_DIFFERENCE - 1, -100),
        ("the Unix epoch", EPOCH_DIFFERENCE, 0),
        (
            // 1960-01-01T00:00:00Z is 315_619_200 s before the epoch.
            "a 1960 time",
            EPOCH_DIFFERENCE - 315_619_200 * TICKS_PER_SECOND,
            -315_619_200 * 1_000_000_000,
        ),
        (
            "a time after the epoch, with a sub-second part",
            EPOCH_DIFFERENCE + 1_700_000_000 * TICKS_PER_SECOND + 1_234_567,
            1_700_000_000 * 1_000_000_000 + 123_456_700,
        ),
    ];
    for (what, ticks, expected) in cases {
        assert_eq!(
            unix_nanos(ntfs_to_unix_time(ticks)),
            expected,
            "{what} ({ticks})"
        );
    }
}

#[test]
fn ntfs_to_unix_time_clamps_to_the_range_of_offset_date_time() {
    let latest = PrimitiveDateTime::MAX.assume_utc();
    // The last tick that fits, and the first that does not.
    let last_tick = (unix_nanos(latest) / 100 + EPOCH_DIFFERENCE as i128) as u64;
    assert_eq!(
        unix_nanos(ntfs_to_unix_time(last_tick)),
        unix_nanos(latest) / 100 * 100,
        "the last representable tick is exact"
    );
    assert_eq!(
        ntfs_to_unix_time(last_tick + 1),
        latest,
        "the first tick beyond the range clamps to the latest time, not the epoch"
    );
    assert!(
        ntfs_to_unix_time(u64::MAX).year() >= 9999,
        "u64::MAX must not fall back to the epoch"
    );
}

// (code units, the WTF-8 bytes of the same text), checked byte for byte so the encoder is not
// compared against itself.
#[cfg(unix)]
const NAMES: &[(&[u16], &[u8])] = &[
    (&[], b""),
    (&[0x61, 0x62, 0x63], b"abc"),
    (&[0x00E9], &[0xC3, 0xA9]),
    (&[0x20AC], &[0xE2, 0x82, 0xAC]),
    (&[0xD83D, 0xDE00], &[0xF0, 0x9F, 0x98, 0x80]),
    (&[0xD800], &[0xED, 0xA0, 0x80]),
    (&[0xDBFF, 0x41], &[0xED, 0xAF, 0xBF, 0x41]),
    (&[0xDC00], &[0xED, 0xB0, 0x80]),
    (&[0xDFFF], &[0xED, 0xBF, 0xBF]),
    // A low surrogate followed by a high one is not a pair.
    (&[0xDC00, 0xD800], &[0xED, 0xB0, 0x80, 0xED, 0xA0, 0x80]),
    (&[0x61, 0xD800, 0x62], &[0x61, 0xED, 0xA0, 0x80, 0x62]),
];

#[cfg(unix)]
#[test]
fn utf16_to_os_string_encodes_wtf8() {
    for (units, bytes) in NAMES {
        assert_eq!(
            utf16_to_os_string(units).as_encoded_bytes(),
            *bytes,
            "{units:04x?}"
        );
    }
}
