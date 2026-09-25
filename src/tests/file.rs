// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use super::*;
use crate::file_info::FileInfo;
use crate::mft::test_records::*;

const REPARSE: u32 = NtfsFileNameFlags::ReparsePoint as u32;

fn attribute_types(record: &[u8]) -> Vec<Option<NtfsAttributeType>> {
    Record::new(FIRST_NORMAL_RECORD, record)
        .expect("valid record")
        .attributes()
        .take(10)
        .map(|attribute| attribute.attribute_type())
        .collect()
}

fn best_name(mft: &Mft, number: u64) -> Option<String> {
    let file = mft.record(number).expect("record");
    file.best_name().map(|name| name.to_string())
}

// A junction or symlink has only reparse-point names; it still needs one.
#[test]
fn best_name_includes_reparse_point_names() {
    let mut junction = new_record(24, 1, 0);
    set_record_flags(&mut junction, directory_flags());
    let offset = add_file_name(&mut junction, ATTRIBUTES_OFFSET, "junction", REPARSE);
    finish_record(&mut junction, offset);

    let mut link = new_record(25, 1, 0);
    let mut offset = ATTRIBUTES_OFFSET;
    for (id, namespace, name) in [
        (1, NtfsFileNamespace::Posix, "posix-link.txt"),
        (2, NtfsFileNamespace::Win32, "file-link.txt"),
    ] {
        offset = add_file_name_ex(&mut link, offset, id, ROOT_RECORD, namespace, name, REPARSE);
    }
    finish_record(&mut link, offset);

    let mft = mft_with(vec![junction, link]);
    assert_eq!(best_name(&mft, 24).as_deref(), Some("junction"));
    assert_eq!(best_name(&mft, 25).as_deref(), Some("file-link.txt"));

    for file in mft.files() {
        let best = file.best_name().expect("best name").to_string();
        assert!(
            file.hard_links().any(|link| link.to_string() == best),
            "best_name {best} is not one of the hard links",
        );
        assert_eq!(FileInfo::new(&file).name, best);
    }
}

// `best_name` falls back to the first Posix name; a Win32 name wins over an earlier Posix one.
#[test]
fn best_name_falls_back_to_first_posix_name() {
    let mut posix_only = new_record(24, 1, 0);
    let mut offset = ATTRIBUTES_OFFSET;
    for (id, name) in [(1, "posix-a"), (2, "posix-b")] {
        offset = add_file_name_ex(
            &mut posix_only,
            offset,
            id,
            ROOT_RECORD,
            NtfsFileNamespace::Posix,
            name,
            0,
        );
    }
    finish_record(&mut posix_only, offset);

    let mut mixed = new_record(25, 1, 0);
    let mut offset = ATTRIBUTES_OFFSET;
    for (id, namespace, name) in [
        (1, NtfsFileNamespace::Posix, "posix"),
        (2, NtfsFileNamespace::Win32, "win32"),
    ] {
        offset = add_file_name_ex(&mut mixed, offset, id, ROOT_RECORD, namespace, name, 0);
    }
    finish_record(&mut mixed, offset);

    let mft = mft_with(vec![posix_only, mixed]);
    assert_eq!(best_name(&mft, 24).as_deref(), Some("posix-a"));
    assert_eq!(best_name(&mft, 25).as_deref(), Some("win32"));
}

// An attribute claiming an implausible length (zero, shorter than the header, or not a multiple
// of 8) ends the walk instead of stepping byte by byte, which would turn the remaining bytes into
// phantom attributes; a zero length must not loop.
#[test]
fn record_attributes_stop_at_an_implausible_attribute_length() {
    for length in [0u32, 1, 4, 8, 12, 20, 33] {
        let mut record = new_record(24, 1, 0);
        let offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, "a.txt", 0);
        // Something that would parse as more attributes if the length
        // were skipped a few bytes at a time.
        for step in 0..8 {
            write_u32(
                &mut record,
                offset + step * 4,
                NtfsAttributeType::Data as u32,
            );
        }
        write_u32(&mut record, offset + 4, length);
        finish_record(&mut record, offset + 64);

        assert_eq!(
            attribute_types(&record),
            [Some(NtfsAttributeType::FileName)],
            "attribute length {length}"
        );
    }
}

// Nothing past used_size is read, even without an End marker.
#[test]
fn record_attributes_stop_at_used_size_without_end_marker() {
    let mut record = new_record(24, 1, 0);
    let used = add_file_name(&mut record, ATTRIBUTES_OFFSET, "a.txt", 0);
    add_resident_attribute(&mut record, used, NtfsAttributeType::Data, 2, "", b"x");
    finish_record(&mut record, used);

    assert_eq!(
        attribute_types(&record),
        [Some(NtfsAttributeType::FileName)]
    );
}

// An attribute that crosses used_size is not returned.
#[test]
fn record_attributes_skip_attribute_crossing_used_size() {
    let mut record = new_record(24, 1, 0);
    let offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, "a.txt", 0);
    let end = add_resident_attribute(
        &mut record,
        offset,
        NtfsAttributeType::Data,
        2,
        "",
        &[7; 80],
    );
    assert!(end - offset > 32);
    finish_record(&mut record, offset + 32);

    assert_eq!(
        attribute_types(&record),
        [Some(NtfsAttributeType::FileName)]
    );
}

// The End marker stops the walk even when more bytes follow inside used_size.
#[test]
fn record_attributes_stop_at_end_marker() {
    let mut record = new_record(24, 1, 0);
    let mut offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, "a.txt", 0);
    offset = add_end_marker(&mut record, offset);
    offset = add_resident_attribute(&mut record, offset, NtfsAttributeType::Data, 2, "", b"x");
    finish_record(&mut record, offset);

    assert_eq!(
        attribute_types(&record),
        [Some(NtfsAttributeType::FileName)]
    );
}

// Split non-resident streams: only the lowest-VCN-0 extent of each stream carries the size; later
// extents in an extension record must not add streams or override it.
#[test]
fn data_streams_count_split_nonresident_streams_once() {
    let mut base = new_record(24, 1, 0);
    let mut offset = add_file_name(&mut base, ATTRIBUTES_OFFSET, "split.bin", 0);
    offset = add_nonresident_attribute(&mut base, offset, NtfsAttributeType::Data, 2, "", 0, 5000);
    offset =
        add_nonresident_attribute(&mut base, offset, NtfsAttributeType::Data, 3, "ads", 0, 700);
    finish_record(&mut base, offset);

    let mut extension = new_record(25, 1, reference(1, 24));
    let mut offset = ATTRIBUTES_OFFSET;
    offset = add_nonresident_attribute(
        &mut extension,
        offset,
        NtfsAttributeType::Data,
        4,
        "",
        8,
        123,
    );
    offset = add_nonresident_attribute(
        &mut extension,
        offset,
        NtfsAttributeType::Data,
        5,
        "ads",
        8,
        999,
    );
    finish_record(&mut extension, offset);

    let mft = mft_with(vec![base, extension]);
    let file = mft.files().next().expect("one file");
    assert_eq!(file.records().count(), 2);

    let streams: Vec<_> = file
        .data_streams()
        .map(|stream| (stream.name, stream.size))
        .collect();
    assert_eq!(streams, [(None, 5000), (Some("ads".into()), 700)]);
    assert_eq!(FileInfo::new(&file).size, 5000);
}

// Found by the structure-aware mft_load target: a `$DATA` attribute whose name lies outside the
// attribute is corrupt. It is not the default stream, so data_streams() must not report it as one
// (`name: None`); FileInfo::size and resident_data() already key on the attribute's name length
// and leave it out.
#[test]
fn data_streams_skip_a_named_stream_with_an_unreadable_name() {
    let mut record = new_record(24, 1, 0);
    let mut offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, "f.txt", 0);
    let stream = offset;
    offset = add_resident_attribute(
        &mut record,
        offset,
        NtfsAttributeType::Data,
        2,
        "ads",
        &[7; 59],
    );
    write_u16(&mut record, stream + 10, 0xC3C3);
    finish_record(&mut record, offset);

    let mft = mft_with(vec![record]);
    let file = mft.files().next().expect("one file");

    let streams: Vec<_> = file.data_streams().collect();
    assert_eq!(streams, [], "a stream with an unreadable name was reported");
    assert_eq!(FileInfo::new(&file).size, 0);
    assert!(file.resident_data().is_none());
}

// A stream name is a raw UTF-16 sequence like a file name, so an alternate data stream can carry
// an unpaired surrogate. data_streams() must report it exactly, or a caller cannot open
// `path:stream` with it.
#[test]
fn data_streams_report_a_stream_name_with_an_unpaired_surrogate_losslessly() {
    let name: Vec<u16> = "ads-"
        .encode_utf16()
        .chain([0xD800])
        .chain("-x".encode_utf16())
        .collect();
    let mut record = new_record(24, 1, 0);
    let mut offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, "f.txt", 0);
    offset = add_resident_attribute_raw(
        &mut record,
        offset,
        NtfsAttributeType::Data,
        2,
        &name,
        &[7; 5],
    );
    finish_record(&mut record, offset);

    let mft = mft_with(vec![record]);
    let file = mft.files().next().expect("one file");

    let streams: Vec<_> = file.data_streams().collect();
    assert_eq!(streams.len(), 1, "{streams:?}");
    let expected_bytes: &[u8] = b"ads-\xED\xA0\x80-x";
    assert_os_str_is(
        streams[0].name.as_deref().expect("a named stream"),
        &name,
        expected_bytes,
    );
    assert_eq!(streams[0].size, 5);

    let attribute_name = file
        .attributes()
        .find(|attribute| attribute.attribute_type() == Some(NtfsAttributeType::Data))
        .and_then(|attribute| attribute.name());
    assert_os_str_is(
        attribute_name.as_deref().expect("a named attribute"),
        &name,
        expected_bytes,
    );
}

#[test]
fn a_name_is_in_the_root_when_its_parent_is_the_root_directory() {
    let record_named_under = |number: u64, parent: u64| {
        let mut record = new_record(number, 1, 0);
        let offset = add_file_name_ex(
            &mut record,
            ATTRIBUTES_OFFSET,
            1,
            parent,
            NtfsFileNamespace::Win32,
            "f",
            0,
        );
        finish_record(&mut record, offset);
        record
    };
    let (root, elsewhere, stale_root) = (
        FIRST_NORMAL_RECORD,
        FIRST_NORMAL_RECORD + 1,
        FIRST_NORMAL_RECORD + 2,
    );
    let mft = mft_with(vec![
        // The parent as a live file stores it: record number plus sequence number.
        record_named_under(root, reference(ROOT_SEQUENCE, crate::ROOT_RECORD)),
        record_named_under(elsewhere, reference(1, FIRST_NORMAL_RECORD + 40)),
        // Record number only: the sequence number is not compared.
        record_named_under(stale_root, reference(9, crate::ROOT_RECORD)),
    ]);
    let in_root = |number| {
        let names: Vec<_> = mft.record(number).expect("record").names().collect();
        assert_eq!(names.len(), 1, "record {number}");
        names[0].is_in_root()
    };
    assert!(in_root(root));
    assert!(!in_root(elsewhere));
    assert!(in_root(stale_root));
}
