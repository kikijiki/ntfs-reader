// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! Windows-only: compares this module's struct layouts and `Reason` values against the real
//! `windows::Win32::System::Ioctl`/`Storage::FileSystem` types, field for field. Meaningful only
//! with `windows` in the dependency graph, hence `cfg(windows)`. Runs under plain `cargo test`
//! on Windows, since this module is always built there.

use crate::usn::*;
use std::mem::{offset_of, size_of};
use windows::Win32::Storage::FileSystem::FILE_ID_128;
use windows::Win32::System::Ioctl;

#[test]
fn common_header_matches_windows_layout() {
    assert_eq!(
        size_of::<CommonHeader>(),
        size_of::<Ioctl::USN_RECORD_COMMON_HEADER>()
    );
    assert_eq!(
        offset_of!(CommonHeader, record_length),
        offset_of!(Ioctl::USN_RECORD_COMMON_HEADER, RecordLength)
    );
    assert_eq!(
        offset_of!(CommonHeader, major_version),
        offset_of!(Ioctl::USN_RECORD_COMMON_HEADER, MajorVersion)
    );
    assert_eq!(
        offset_of!(CommonHeader, minor_version),
        offset_of!(Ioctl::USN_RECORD_COMMON_HEADER, MinorVersion)
    );
}

#[test]
fn record_v2_matches_windows_layout() {
    assert_eq!(size_of::<RecordV2>(), size_of::<Ioctl::USN_RECORD_V2>());
    assert_eq!(
        offset_of!(RecordV2, record_length),
        offset_of!(Ioctl::USN_RECORD_V2, RecordLength)
    );
    assert_eq!(
        offset_of!(RecordV2, major_version),
        offset_of!(Ioctl::USN_RECORD_V2, MajorVersion)
    );
    assert_eq!(
        offset_of!(RecordV2, minor_version),
        offset_of!(Ioctl::USN_RECORD_V2, MinorVersion)
    );
    assert_eq!(
        offset_of!(RecordV2, file_reference_number),
        offset_of!(Ioctl::USN_RECORD_V2, FileReferenceNumber)
    );
    assert_eq!(
        offset_of!(RecordV2, parent_file_reference_number),
        offset_of!(Ioctl::USN_RECORD_V2, ParentFileReferenceNumber)
    );
    assert_eq!(
        offset_of!(RecordV2, usn),
        offset_of!(Ioctl::USN_RECORD_V2, Usn)
    );
    assert_eq!(
        offset_of!(RecordV2, time_stamp),
        offset_of!(Ioctl::USN_RECORD_V2, TimeStamp)
    );
    assert_eq!(
        offset_of!(RecordV2, reason),
        offset_of!(Ioctl::USN_RECORD_V2, Reason)
    );
    assert_eq!(
        offset_of!(RecordV2, source_info),
        offset_of!(Ioctl::USN_RECORD_V2, SourceInfo)
    );
    assert_eq!(
        offset_of!(RecordV2, security_id),
        offset_of!(Ioctl::USN_RECORD_V2, SecurityId)
    );
    assert_eq!(
        offset_of!(RecordV2, file_attributes),
        offset_of!(Ioctl::USN_RECORD_V2, FileAttributes)
    );
    assert_eq!(
        offset_of!(RecordV2, file_name_length),
        offset_of!(Ioctl::USN_RECORD_V2, FileNameLength)
    );
    assert_eq!(
        offset_of!(RecordV2, file_name_offset),
        offset_of!(Ioctl::USN_RECORD_V2, FileNameOffset)
    );
    assert_eq!(
        offset_of!(RecordV2, file_name),
        offset_of!(Ioctl::USN_RECORD_V2, FileName)
    );
}

// Reason's constants are written as numbers so `Reason` builds without the `windows` crate;
// this pins each one to the crate's own `USN_REASON_*`, and its `Display` to the name.
#[test]
fn reason_constants_match_the_windows_ones() {
    macro_rules! rows {
            ($($ours:ident => $theirs:ident),* $(,)?) => {
                [$((stringify!($ours), stringify!($theirs), Reason::$ours, Ioctl::$theirs)),*]
            };
        }
    let rows = rows![
        DATA_OVERWRITE => USN_REASON_DATA_OVERWRITE,
        DATA_EXTEND => USN_REASON_DATA_EXTEND,
        DATA_TRUNCATION => USN_REASON_DATA_TRUNCATION,
        NAMED_DATA_OVERWRITE => USN_REASON_NAMED_DATA_OVERWRITE,
        NAMED_DATA_EXTEND => USN_REASON_NAMED_DATA_EXTEND,
        NAMED_DATA_TRUNCATION => USN_REASON_NAMED_DATA_TRUNCATION,
        FILE_CREATE => USN_REASON_FILE_CREATE,
        FILE_DELETE => USN_REASON_FILE_DELETE,
        EA_CHANGE => USN_REASON_EA_CHANGE,
        SECURITY_CHANGE => USN_REASON_SECURITY_CHANGE,
        RENAME_OLD_NAME => USN_REASON_RENAME_OLD_NAME,
        RENAME_NEW_NAME => USN_REASON_RENAME_NEW_NAME,
        INDEXABLE_CHANGE => USN_REASON_INDEXABLE_CHANGE,
        BASIC_INFO_CHANGE => USN_REASON_BASIC_INFO_CHANGE,
        HARD_LINK_CHANGE => USN_REASON_HARD_LINK_CHANGE,
        COMPRESSION_CHANGE => USN_REASON_COMPRESSION_CHANGE,
        ENCRYPTION_CHANGE => USN_REASON_ENCRYPTION_CHANGE,
        OBJECT_ID_CHANGE => USN_REASON_OBJECT_ID_CHANGE,
        REPARSE_POINT_CHANGE => USN_REASON_REPARSE_POINT_CHANGE,
        STREAM_CHANGE => USN_REASON_STREAM_CHANGE,
        TRANSACTED_CHANGE => USN_REASON_TRANSACTED_CHANGE,
        INTEGRITY_CHANGE => USN_REASON_INTEGRITY_CHANGE,
        DESIRED_STORAGE_CLASS_CHANGE => USN_REASON_DESIRED_STORAGE_CLASS_CHANGE,
        CLOSE => USN_REASON_CLOSE,
    ];
    for (name, windows_name, ours, theirs) in rows {
        assert_eq!(ours.bits(), theirs, "{name}");
        assert_eq!(windows_name, format!("USN_REASON_{name}"));
        assert_eq!(ours.to_string(), name, "Display of {windows_name}");
    }
}

#[test]
fn file_id_128_matches_windows_layout() {
    assert_eq!(size_of::<FileId128>(), size_of::<FILE_ID_128>());
    assert_eq!(
        offset_of!(FileId128, identifier),
        offset_of!(FILE_ID_128, Identifier)
    );
}

#[test]
fn record_v3_matches_windows_layout() {
    assert_eq!(size_of::<RecordV3>(), size_of::<Ioctl::USN_RECORD_V3>());
    assert_eq!(
        offset_of!(RecordV3, record_length),
        offset_of!(Ioctl::USN_RECORD_V3, RecordLength)
    );
    assert_eq!(
        offset_of!(RecordV3, major_version),
        offset_of!(Ioctl::USN_RECORD_V3, MajorVersion)
    );
    assert_eq!(
        offset_of!(RecordV3, minor_version),
        offset_of!(Ioctl::USN_RECORD_V3, MinorVersion)
    );
    assert_eq!(
        offset_of!(RecordV3, file_reference_number),
        offset_of!(Ioctl::USN_RECORD_V3, FileReferenceNumber)
    );
    assert_eq!(
        offset_of!(RecordV3, parent_file_reference_number),
        offset_of!(Ioctl::USN_RECORD_V3, ParentFileReferenceNumber)
    );
    assert_eq!(
        offset_of!(RecordV3, usn),
        offset_of!(Ioctl::USN_RECORD_V3, Usn)
    );
    assert_eq!(
        offset_of!(RecordV3, time_stamp),
        offset_of!(Ioctl::USN_RECORD_V3, TimeStamp)
    );
    assert_eq!(
        offset_of!(RecordV3, reason),
        offset_of!(Ioctl::USN_RECORD_V3, Reason)
    );
    assert_eq!(
        offset_of!(RecordV3, source_info),
        offset_of!(Ioctl::USN_RECORD_V3, SourceInfo)
    );
    assert_eq!(
        offset_of!(RecordV3, security_id),
        offset_of!(Ioctl::USN_RECORD_V3, SecurityId)
    );
    assert_eq!(
        offset_of!(RecordV3, file_attributes),
        offset_of!(Ioctl::USN_RECORD_V3, FileAttributes)
    );
    assert_eq!(
        offset_of!(RecordV3, file_name_length),
        offset_of!(Ioctl::USN_RECORD_V3, FileNameLength)
    );
    assert_eq!(
        offset_of!(RecordV3, file_name_offset),
        offset_of!(Ioctl::USN_RECORD_V3, FileNameOffset)
    );
    assert_eq!(
        offset_of!(RecordV3, file_name),
        offset_of!(Ioctl::USN_RECORD_V3, FileName)
    );
}
