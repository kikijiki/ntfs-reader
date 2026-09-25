// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use super::*;

// A variant that wraps another error must show that error's message, and one with fields
// must show each of them; the fixed messages of the other variants are not worth a row.
#[test]
fn display_includes_the_wrapped_error_and_every_field() {
    let cases: Vec<(NtfsReaderError, &[&str])> = vec![
        (
            NtfsReaderError::Io(std::io::Error::other("disk exploded")),
            &["disk exploded"],
        ),
        (
            NtfsReaderError::MissingMftAttribute { attribute: "Data" },
            &["Data"],
        ),
        (
            NtfsReaderError::MftRecordFixupFailed { number: 42 },
            &["42"],
        ),
        (
            NtfsReaderError::InvalidMftRecord { position: 4096 },
            &["4096"],
        ),
        (
            NtfsReaderError::InvalidDataRun {
                details: "bad header",
            },
            &["bad header"],
        ),
        (
            NtfsReaderError::AllocationTooLarge { size: 9001 },
            &["9001"],
        ),
        (
            NtfsReaderError::InvalidClusterBitmap {
                details: "too short",
            },
            &["too short"],
        ),
        (
            NtfsReaderError::InvalidBootSector { field: "oem_id" },
            &["oem_id"],
        ),
        (
            NtfsReaderError::InvalidUsnRecord {
                details: "too short",
            },
            &["too short"],
        ),
        (
            NtfsReaderError::ReadBufferTooSmall {
                size: 3071,
                min: 4096,
            },
            &["3071", "4096"],
        ),
    ];

    for (error, expected) in cases {
        let message = error.to_string();
        for part in expected {
            assert!(
                message.contains(part),
                "Display for {error:?} was {message:?}, expected it to contain {part:?}",
            );
        }
    }
}

#[test]
fn a_permission_denied_io_error_becomes_access_denied() {
    let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
    assert!(matches!(
        NtfsReaderError::from(denied),
        NtfsReaderError::AccessDenied
    ));

    let other = std::io::Error::from(std::io::ErrorKind::NotFound);
    assert!(matches!(
        NtfsReaderError::from(other),
        NtfsReaderError::Io(_)
    ));
}
