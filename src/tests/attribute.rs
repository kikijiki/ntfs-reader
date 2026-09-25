// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use super::*;

/// 64 bytes holding a `$DATA` attribute header that claims `length`.
fn attribute_claiming(length: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; 64];
    bytes[0..4].copy_from_slice(&(NtfsAttributeType::Data as u32).to_le_bytes());
    bytes[4..8].copy_from_slice(&length.to_le_bytes());
    bytes
}

// NTFS pads every attribute to 8 bytes and the common header alone is 16.
// A length outside that (or past the bytes given) is corrupt, not an
// attribute to step over a few bytes at a time.
#[test]
fn an_attribute_needs_a_plausible_length() {
    let cases = [
        (0, false),
        (1, false),
        (4, false),
        (8, false),
        (12, false),
        (16, true),
        (20, false),
        (24, true),
        (64, true),
        (72, false),
        (u32::MAX, false),
    ];
    for (length, plausible) in cases {
        let bytes = attribute_claiming(length);
        assert_eq!(
            NtfsAttribute::new(&bytes).is_some(),
            plausible,
            "length {length}"
        );
    }
    assert!(NtfsAttribute::new(&attribute_claiming(16)[..15]).is_none());
}
