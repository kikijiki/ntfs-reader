// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use std::path::PathBuf;

use crate::file::NtfsDataStream;
use crate::file_info::FileInfo;
use crate::mft::test_records::*;
use crate::mft::*;
use crate::path::{DefaultPathCache, PathCache};

#[test]
fn combines_base_and_extension_records_into_one_file() {
    let attributes = NtfsFileNameFlags::Hidden as u32
        | NtfsFileNameFlags::System as u32
        | NtfsFileNameFlags::Archive as u32
        | NtfsFileNameFlags::SparseFile as u32;
    let file_size = 34_359_738_368u64;

    let mut base = new_record(24, 7, 0);
    let mut offset = ATTRIBUTES_OFFSET;
    offset = add_standard_information(&mut base, offset, attributes);
    offset = add_nonresident_data(&mut base, offset, file_size);
    finish_record(&mut base, offset);

    let base_reference = (7u64 << 48) | 24;
    let mut extension = new_record(25, 3, base_reference);
    let offset = add_file_name(
        &mut extension,
        ATTRIBUTES_OFFSET,
        "large-fragmented.rar",
        attributes,
    );
    finish_record(&mut extension, offset);

    let mft = mft_with(vec![base, extension]);

    let files: Vec<_> = mft.files().collect();
    assert_eq!(files.len(), 1, "extension record must not be a second file");
    assert_eq!(files[0].number(), 24);
    assert_eq!(files[0].records().count(), 2);

    let info = FileInfo::new(&files[0]);
    assert_eq!(info.name, "large-fragmented.rar");
    assert_eq!(info.size, file_size);
    assert_eq!(info.file_attributes, attributes);
}

#[test]
fn hard_links_exclude_dos_aliases() {
    let attributes = NtfsFileNameFlags::Archive as u32;
    // Different parent: a second real hard link.
    let other_parent = (2u64 << 48) | 100;

    let mut base = new_record(24, 1, 0);
    // Windows counts the DOS alias in link_count too.
    write_u16(&mut base, 18, 3);
    let mut offset = ATTRIBUTES_OFFSET;
    for (id, parent, namespace, name) in [
        (1, ROOT_RECORD, NtfsFileNamespace::Win32, "longfilename.txt"),
        (2, ROOT_RECORD, NtfsFileNamespace::Dos, "LONGFI~1.TXT"),
        (3, other_parent, NtfsFileNamespace::Posix, "secondlink.txt"),
    ] {
        offset = add_file_name_ex(&mut base, offset, id, parent, namespace, name, attributes);
    }
    finish_record(&mut base, offset);

    let mft = mft_with(vec![base]);
    let file = mft.files().next().expect("one file");

    assert_eq!(file.names().count(), 3);

    let links: Vec<_> = file
        .hard_links()
        .map(|name| (name.to_string(), name.parent_number()))
        .collect();
    assert_eq!(
        links,
        [
            ("longfilename.txt".to_string(), ROOT_RECORD),
            ("secondlink.txt".to_string(), 100),
        ]
    );

    let best = file.best_name().expect("best name");
    assert_eq!(best.to_string(), "longfilename.txt");
}

#[test]
fn data_streams_include_alternate_streams() {
    let mut base = new_record(24, 1, 0);
    let mut offset = ATTRIBUTES_OFFSET;
    offset = add_file_name(&mut base, offset, "file.txt", 0);
    offset = add_resident_attribute(&mut base, offset, NtfsAttributeType::Data, 2, "", b"hello");
    offset = add_resident_attribute(
        &mut base,
        offset,
        NtfsAttributeType::Data,
        3,
        "Zone.Identifier",
        b"[ZoneTransfer]",
    );
    finish_record(&mut base, offset);

    let mft = mft_with(vec![base]);
    let file = mft.files().next().expect("one file");

    let streams: Vec<_> = file.data_streams().collect();
    assert_eq!(
        streams,
        [
            NtfsDataStream {
                name: None,
                size: 5,
                data_lost: false
            },
            NtfsDataStream {
                name: Some("Zone.Identifier".into()),
                size: 14,
                data_lost: false
            },
        ]
    );
    assert_eq!(file.resident_data(), Some(&b"hello"[..]));
    assert_eq!(FileInfo::new(&file).size, 5);
}

#[test]
fn resolves_a_path_for_every_hard_link() {
    let mut directory = new_record(24, 1, 0);
    write_u16(
        &mut directory,
        22,
        NtfsFileFlags::InUse as u16 | NtfsFileFlags::IsDirectory as u16,
    );
    let offset = add_file_name(&mut directory, ATTRIBUTES_OFFSET, "dir", 0);
    finish_record(&mut directory, offset);

    let mut file = new_record(25, 1, 0);
    let mut offset = ATTRIBUTES_OFFSET;
    offset = add_file_name_ex(
        &mut file,
        offset,
        1,
        (1u64 << 48) | 24,
        NtfsFileNamespace::Win32AndDos,
        "a.txt",
        0,
    );
    offset = add_file_name_ex(
        &mut file,
        offset,
        2,
        (ROOT_SEQUENCE as u64) << 48 | ROOT_RECORD,
        NtfsFileNamespace::Win32AndDos,
        "b.txt",
        0,
    );
    finish_record(&mut file, offset);

    let mft = mft_with(vec![directory, file]);
    let file = mft.record(25).expect("file record");

    let mut cache = DefaultPathCache::new();
    let paths: Vec<_> = file
        .hard_links()
        .map(|link| mft.resolve_path(&link, &mut cache))
        .collect();
    // Uses `.join()`, not a literal `\`-joined path: resolve_path builds the result with
    // `PathBuf::push`, whose separator is platform-specific; a hard-coded Windows path would
    // only pass on Windows.
    assert_eq!(
        paths,
        [
            Some(PathBuf::from(r"\\.\T:").join("dir").join("a.txt")),
            Some(PathBuf::from(r"\\.\T:").join("b.txt")),
        ]
    );
    // The cache is keyed by the directory's full reference (sequence 1, record 24).
    assert_eq!(
        cache.get((1u64 << 48) | 24),
        crate::path::CachedPath::Resolved(PathBuf::from(r"\\.\T:").join("dir").as_path())
    );
}

#[test]
fn read_data_fs_follows_resident_attribute_list() {
    let entry = |type_id: NtfsAttributeType, record: u64| {
        let mut entry = vec![0u8; 32];
        write_u32(&mut entry, 0, type_id as u32);
        write_u16(&mut entry, 4, 32);
        entry[7] = 26;
        write_u64(&mut entry, 16, (1u64 << 48) | record);
        entry
    };

    let mut base = new_record(0, 1, 0);
    let list = [
        entry(NtfsAttributeType::StandardInformation, 0),
        entry(NtfsAttributeType::Bitmap, 1),
    ]
    .concat();
    let mut offset = add_resident_attribute(
        &mut base,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::AttributeList,
        0,
        "",
        &list,
    );
    // $MFT's own $DATA, needed to locate record 1 through the list.
    offset = add_identity_mft_data(&mut base, offset, 1);
    finish_record(&mut base, offset);

    let mut extension = new_record(1, 1, 1u64 << 48);
    let offset = add_resident_attribute(
        &mut extension,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Bitmap,
        1,
        "",
        &[0xAB, 0xCD],
    );
    finish_record(&mut extension, offset);

    let volume = test_volume();
    let mut reader = std::io::Cursor::new([base.clone(), extension].concat());
    let read = |attribute_type, reader: &mut std::io::Cursor<Vec<u8>>| {
        Mft::read_data_fs(&volume, reader, &base, attribute_type).expect("read")
    };

    assert_eq!(
        read(NtfsAttributeType::Bitmap, &mut reader),
        Some(vec![0xAB, 0xCD])
    );
    // The list entry for StandardInformation points back at record 0 itself, which has no such
    // attribute: genuinely absent. Data, by contrast, is $MFT's own bootstrap attribute above,
    // not looked up via the list.
    assert_eq!(
        read(NtfsAttributeType::StandardInformation, &mut reader),
        None
    );
}

// `$MFT`'s attribute list can itself be non-resident (it outgrows the record on a fragmented
// volume): read through its own runs, from its own cluster, and its entry still reaches the
// extension record.
#[test]
fn read_data_fs_reads_a_non_resident_attribute_list() {
    let list = list_entry(NtfsAttributeType::Bitmap, "", 0, 1);
    let mut base = new_record(0, 1, 0);
    // The list is in cluster 1, after the four records the identity `$DATA` maps.
    let mut offset = add_nonresident_data_runs(
        &mut base,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::AttributeList,
        "",
        0,
        0,
        list.len() as u64,
        &encode_runs(&[(1, Some(1))]),
    );
    offset = add_identity_mft_data(&mut base, offset, 1);
    finish_record(&mut base, offset);

    let mut extension = new_record(1, 1, 1u64 << 48);
    let offset = add_resident_attribute(
        &mut extension,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Bitmap,
        1,
        "",
        &[0xAB, 0xCD],
    );
    finish_record(&mut extension, offset);

    let mut image = vec![0u8; 2 * CLUSTER_SIZE];
    image[..RECORD_SIZE].copy_from_slice(&base);
    image[RECORD_SIZE..2 * RECORD_SIZE].copy_from_slice(&extension);
    image[CLUSTER_SIZE..CLUSTER_SIZE + list.len()].copy_from_slice(&list);

    let data = Mft::read_data_fs(
        &test_volume(),
        &mut std::io::Cursor::new(image),
        &base,
        NtfsAttributeType::Bitmap,
    )
    .expect("read");
    assert_eq!(data, Some(vec![0xAB, 0xCD]));
}

// Record 0's `$DATA` has two runs with a physical gap between them; the extension record the
// list points at (record 4) falls in the second run. A decoy sits where a naive
// `n * file_record_size` formula would read (byte 4096), so a wrong formula fails loudly instead
// of passing by luck.
#[test]
fn read_data_fs_locates_extension_record_through_fragmented_mft_data() {
    let list = list_entry(NtfsAttributeType::Bitmap, "", 0, 4);
    let mut base = new_record(0, 1, 0);
    let mut offset = add_resident_attribute(
        &mut base,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::AttributeList,
        0,
        "",
        &list,
    );
    // $MFT's own $DATA, VCN-0 extent: two runs, physically apart (a gap between cluster 1 and
    // cluster 5). Records 0-3 map through the first run (bytes 0..4096); records 4-7 through
    // the second (bytes 4096..8192, at cluster 5).
    offset = add_nonresident_data_runs(
        &mut base,
        offset,
        NtfsAttributeType::Data,
        "",
        0,
        1,
        2 * CLUSTER_SIZE as u64,
        &[0x11, 0x01, 0x01, 0x11, 0x01, 0x04],
    );
    finish_record(&mut base, offset);

    let mut image = vec![0u8; 6 * CLUSTER_SIZE];

    // Decoy at the naive `mft_position + 4 * file_record_size` position (byte 4096): a valid,
    // correctly-based record 4 but with a different value, so reading it instead of the real
    // one is visible below.
    let mut decoy = new_record(999, 1, 1u64 << 48);
    let decoy_offset = add_resident_attribute(
        &mut decoy,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Bitmap,
        1,
        "",
        &[0x99, 0x99],
    );
    finish_record(&mut decoy, decoy_offset);
    image[4096..4096 + RECORD_SIZE].copy_from_slice(&decoy);

    // The real record 4, in the second run (cluster 5, byte 20480), at the start of its
    // coverage (within-run offset 0).
    let mut extension = new_record(4, 1, 1u64 << 48);
    let extension_offset = add_resident_attribute(
        &mut extension,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Bitmap,
        1,
        "",
        &[0xAB, 0xCD],
    );
    finish_record(&mut extension, extension_offset);
    image[20480..20480 + RECORD_SIZE].copy_from_slice(&extension);

    let mut reader = std::io::Cursor::new(image);
    let data = Mft::read_data_fs(
        &test_volume(),
        &mut reader,
        &base,
        NtfsAttributeType::Bitmap,
    )
    .expect("read");
    assert_eq!(data, Some(vec![0xAB, 0xCD]));
}

// The real `$MFT` shape: record 0 holds the VCN-0 extent and the list, whose entries also
// point back at record 0.
#[test]
fn read_data_fs_joins_base_extent_with_extension_extent() {
    // Record 3, not 1: MFT records pack four to a cluster (RECORD_SIZE 1024 in CLUSTER_SIZE
    // 4096), so any non-zero extension record shares its cluster with VCN 0's own content -
    // here, the last record-sized slot.
    let list = [
        list_entry(NtfsAttributeType::AttributeList, "", 0, 0),
        list_entry(NtfsAttributeType::Data, "", 0, 0),
        list_entry(NtfsAttributeType::Data, "", 1, 3),
    ]
    .concat();
    let mut base = new_record(0, 1, 0);
    let mut offset = add_resident_attribute(
        &mut base,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::AttributeList,
        0,
        "",
        &list,
    );
    offset = add_nonresident_data_runs(
        &mut base,
        offset,
        NtfsAttributeType::Data,
        "",
        0,
        0,
        2 * CLUSTER_SIZE as u64,
        &[0x11, 0x01, 0x01],
    );
    finish_record(&mut base, offset);

    let mut extension = new_record(3, 1, 1u64 << 48);
    let offset = add_nonresident_data_runs(
        &mut extension,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Data,
        "",
        1,
        1,
        0,
        &[0x11, 0x01, 0x02],
    );
    finish_record(&mut extension, offset);

    // VCN 0 is at cluster 1 (bytes 4096..8192); record 3 (byte 4096 + 3 * RECORD_SIZE = 7168)
    // sits in the last record slot, so only the first three slots are content-checked below.
    // VCN 1 is at cluster 2, untouched by record placement.
    let mut image = vec![0u8; 4 * CLUSTER_SIZE];
    image[4096..7168].fill(0xAA);
    image[7168..7168 + RECORD_SIZE].copy_from_slice(&extension);
    image[8192..12288].fill(0xBB);

    let mut reader = std::io::Cursor::new(image);
    let data = Mft::read_data_fs(&test_volume(), &mut reader, &base, NtfsAttributeType::Data)
        .expect("read")
        .expect("present");
    assert_eq!(data.len(), 2 * CLUSTER_SIZE);
    assert!(data[..3 * RECORD_SIZE].iter().all(|&b| b == 0xAA));
    assert!(data[CLUSTER_SIZE..].iter().all(|&b| b == 0xBB));
}

// A named `$DATA` before the unnamed one.
#[test]
fn read_data_fs_skips_named_attribute() {
    let mut base = new_record(0, 1, 0);
    let mut offset = add_resident_attribute(
        &mut base,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Data,
        1,
        "stream",
        b"wrong",
    );
    offset = add_resident_attribute(&mut base, offset, NtfsAttributeType::Data, 2, "", b"right");
    finish_record(&mut base, offset);

    let mut reader = std::io::Cursor::new(base.clone());
    let data = Mft::read_data_fs(&test_volume(), &mut reader, &base, NtfsAttributeType::Data)
        .expect("read");
    assert_eq!(data.as_deref(), Some(&b"right"[..]));
}

// A named `$DATA` reached through the list is skipped.
#[test]
fn read_data_fs_skips_named_attribute_in_list() {
    let mut base = new_record(0, 1, 0);
    let list = list_entry(NtfsAttributeType::Bitmap, "stream", 0, 1);
    let mut offset = add_resident_attribute(
        &mut base,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::AttributeList,
        0,
        "",
        &list,
    );
    // $MFT's own $DATA, needed to locate record 1 through the list.
    offset = add_identity_mft_data(&mut base, offset, 1);
    finish_record(&mut base, offset);

    let mut extension = new_record(1, 1, 1u64 << 48);
    let offset = add_resident_attribute(
        &mut extension,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Bitmap,
        1,
        "stream",
        &[0xEE],
    );
    finish_record(&mut extension, offset);

    let mut reader = std::io::Cursor::new([base.clone(), extension].concat());
    let data = Mft::read_data_fs(
        &test_volume(),
        &mut reader,
        &base,
        NtfsAttributeType::Bitmap,
    )
    .expect("read");
    assert_eq!(data, None);
}

// A list entry whose target record belongs to another base record is ignored.
#[test]
fn read_data_fs_ignores_record_of_another_base() {
    let mut base = new_record(0, 1, 0);
    let list = [
        list_entry(NtfsAttributeType::Bitmap, "", 0, 1),
        list_entry(NtfsAttributeType::Bitmap, "", 0, 2),
    ]
    .concat();
    let mut offset = add_resident_attribute(
        &mut base,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::AttributeList,
        0,
        "",
        &list,
    );
    // $MFT's own $DATA, needed to locate records 1 and 2 through the list.
    offset = add_identity_mft_data(&mut base, offset, 1);
    finish_record(&mut base, offset);

    // Record 1 is an extension of record 5, not of record 0.
    let mut foreign = new_record(1, 1, (1u64 << 48) | ROOT_RECORD);
    let offset = add_resident_attribute(
        &mut foreign,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Bitmap,
        1,
        "",
        &[0xEE],
    );
    finish_record(&mut foreign, offset);
    let mut own = new_record(2, 1, 1u64 << 48);
    let offset = add_resident_attribute(
        &mut own,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Bitmap,
        1,
        "",
        &[0xAB, 0xCD],
    );
    finish_record(&mut own, offset);

    let mut reader = std::io::Cursor::new([base.clone(), foreign, own].concat());
    let data = Mft::read_data_fs(
        &test_volume(),
        &mut reader,
        &base,
        NtfsAttributeType::Bitmap,
    )
    .expect("read");
    assert_eq!(data, Some(vec![0xAB, 0xCD]));
}

// Review follow-up: a list entry whose target record's sequence no longer matches what the
// entry expects (freed and reused since the list was written) is ignored, the same check
// NtfsFile::records makes for a base record's own reference.
#[test]
fn read_data_fs_ignores_record_of_stale_sequence() {
    let mut base = new_record(0, 1, 0);
    let list = [
        list_entry(NtfsAttributeType::Bitmap, "", 0, 1),
        list_entry(NtfsAttributeType::Bitmap, "", 0, 2),
    ]
    .concat();
    let mut offset = add_resident_attribute(
        &mut base,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::AttributeList,
        0,
        "",
        &list,
    );
    // $MFT's own $DATA, needed to locate records 1 and 2 through the list.
    offset = add_identity_mft_data(&mut base, offset, 1);
    finish_record(&mut base, offset);

    // list_entry() always writes sequence 1. Record 1's base is still record 0 (unlike the
    // "another base" case above), but its own sequence is 2: freed and reused since the list
    // was written.
    let mut stale = new_record(1, 2, 1u64 << 48);
    let offset = add_resident_attribute(
        &mut stale,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Bitmap,
        1,
        "",
        &[0xEE],
    );
    finish_record(&mut stale, offset);

    // Record 2's sequence (1) matches what the list expects.
    let mut fresh = new_record(2, 1, 1u64 << 48);
    let offset = add_resident_attribute(
        &mut fresh,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Bitmap,
        1,
        "",
        &[0xAB, 0xCD],
    );
    finish_record(&mut fresh, offset);

    let mut reader = std::io::Cursor::new([base.clone(), stale, fresh].concat());
    let data = Mft::read_data_fs(
        &test_volume(),
        &mut reader,
        &base,
        NtfsAttributeType::Bitmap,
    )
    .expect("read");
    assert_eq!(data, Some(vec![0xAB, 0xCD]));
}

// A short record is an error, not a panic.
#[test]
fn read_data_fs_rejects_short_record() {
    let record = [0u8; 16];
    let mut reader = std::io::Cursor::new(Vec::new());
    let result = Mft::read_data_fs(
        &test_volume(),
        &mut reader,
        &record,
        NtfsAttributeType::Data,
    );
    assert!(result.is_err());
}

// Found by fuzzing (a cargo-fuzz target, since removed): a non-resident $DATA attribute's
// declared size was trusted up to u64::MAX against only the runs present, and runs are cheap to
// fake. try_reserve stopped a panic, but a 2 GiB request still aborted the fuzzer and wasted real
// work for a size no genuine $MFT stream reaches. Fixed by bounding the declared size against the
// volume's size in `read_runs`.
#[test]
fn read_data_fs_rejects_size_larger_than_the_volume() {
    let mut base = new_record(0, 1, 0);
    let offset = add_nonresident_data_runs(
        &mut base,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Data,
        "",
        0,
        0,
        1_000_000,
        // 250 clusters (4096 bytes each, test_volume()'s cluster size) = 1,024,000 bytes of
        // runs: enough to pass the "runs at least cover the declared size" check, so the
        // rejection below is really about the volume-size bound, not a shorter-runs error.
        &[0x11, 250, 1],
    );
    finish_record(&mut base, offset);

    // No real volume this small could hold a 1,000,000-byte $DATA.
    let volume = test_volume().with_volume_size(1000);

    let mut reader = std::io::Cursor::new(vec![0u8; 2000]);
    let result = Mft::read_data_fs(&volume, &mut reader, &base, NtfsAttributeType::Data);
    assert!(
        matches!(result, Err(NtfsReaderError::InvalidDataRun { .. })),
        "a declared $DATA size (1,000,000) larger than the whole volume (1000 bytes) must be \
             rejected, not trusted enough to attempt allocating it; got {result:?}",
    );
}

// `$MFT` data is read into one buffer, so its size must fit `usize`: on a 32-bit build, a size
// above 4 GiB is refused, not truncated to a short buffer. The volume is unbounded
// (`volume_size` 0), so the volume-size check above is not what refuses it.
#[cfg(target_pointer_width = "32")]
#[test]
fn read_runs_rejects_a_size_that_does_not_fit_in_usize() {
    let size = 1u64 << 32;
    let mut reader = std::io::Cursor::new(Vec::new());

    let result = Mft::read_runs(
        &mut reader,
        &test_volume(),
        size,
        &[DataRun::Sparse { length: size }],
    );

    assert!(
        matches!(result, Err(NtfsReaderError::AllocationTooLarge { size: refused }) if refused == size),
        "{result:?}"
    );
}

// A size that fits `usize` but no allocator can satisfy is an error too, not an abort.
#[test]
fn read_runs_reports_an_allocation_that_cannot_be_made() {
    let size = 1u64 << 62;
    let mut reader = std::io::Cursor::new(Vec::new());

    let result = Mft::read_runs(
        &mut reader,
        &test_volume(),
        size,
        &[DataRun::Sparse { length: size }],
    );

    assert!(
        matches!(result, Err(NtfsReaderError::AllocationTooLarge { size: refused }) if refused == size),
        "{result:?}"
    );
}

// Translating a record number through $MFT's own $DATA runs must not overflow, even when a
// run's LCN sits implausibly close to u64::MAX.
#[test]
fn read_data_fs_does_not_overflow_record_position() {
    let mut base = new_record(0, 1, 0);
    let list = list_entry(NtfsAttributeType::Bitmap, "", 0, 4);
    let mut offset = add_resident_attribute(
        &mut base,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::AttributeList,
        0,
        "",
        &list,
    );
    // One run whose LCN is the largest multiple of CLUSTER_SIZE that fits in a u64: record 4's
    // within-run offset (4096 bytes) pushes the translated position exactly one past u64::MAX.
    let cluster_offset = u64::MAX / CLUSTER_SIZE as u64;
    let mut runs = vec![0x81, 0x02];
    runs.extend(cluster_offset.to_le_bytes());
    offset = add_nonresident_data_runs(
        &mut base,
        offset,
        NtfsAttributeType::Data,
        "",
        0,
        1,
        2 * CLUSTER_SIZE as u64,
        &runs,
    );
    finish_record(&mut base, offset);

    // What a wrapped-around position (0) would read: a valid extension record of record 0
    // holding the bitmap. The list entry names it, so only the overflow check keeps it out.
    let mut extension = new_record(4, 1, reference(1, 0));
    let offset = add_resident_attribute(
        &mut extension,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Bitmap,
        1,
        "",
        b"bits",
    );
    finish_record(&mut extension, offset);

    let mut reader = std::io::Cursor::new(extension);
    let result = Mft::read_data_fs(
        &test_volume(),
        &mut reader,
        &base,
        NtfsAttributeType::Bitmap,
    );
    assert!(matches!(result, Ok(None)), "{result:?}");
}

// A record whose update sequence does not match is skipped instead of failing the whole load.
#[test]
fn load_skips_record_with_bad_fixup() {
    const SECTOR_END_VALUE: u16 = 0x1234;
    let first = FIRST_NORMAL_RECORD as usize;
    let names = ["a.txt", "bad.txt", "c.txt"];

    let mut data = vec![0u8; first * RECORD_SIZE];
    let mut bitmap = vec![0u8; (first + names.len()).div_ceil(8)];
    for (index, name) in names.iter().enumerate() {
        let number = first + index;
        let mut record = new_record(number as u64, 1, 0);
        let offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, name, 0);
        finish_record(&mut record, offset);
        write_u16(&mut record, SECTOR_SIZE - 2, SECTOR_END_VALUE);
        protect_record(&mut record, 7);
        if *name == "bad.txt" {
            record[RECORD_SIZE - 2] ^= 0xFF;
        }
        bitmap[number / 8] |= 1 << (number % 8);
        data.extend(record);
    }

    let mft = Mft::from_parts(test_volume(), data, bitmap)
        .expect("one bad record must not fail the load");

    let loaded: Vec<_> = mft
        .files()
        .map(|file| file.best_name().expect("name").to_string())
        .collect();
    assert_eq!(loaded, ["a.txt", "c.txt"]);
    assert!(mft.record(first as u64 + 1).is_none());
    assert_eq!(mft.corrupt_records(), 1);

    // Fixups were still applied to the good records.
    let sector_end = first * RECORD_SIZE + SECTOR_SIZE - 2;
    assert_eq!(
        u16::from_le_bytes([mft.data[sector_end], mft.data[sector_end + 1]]),
        SECTOR_END_VALUE
    );
}

// An absolute LCN past `u64::MAX` bytes must be an error, not silently truncated.
#[test]
fn data_run_past_u64_max_is_an_error() {
    // 0x81: 1-byte cluster count, 8-byte offset. i64::MAX clusters of 4096 bytes is about
    // 2^75 bytes.
    let mut runs = vec![0x81, 0x01];
    runs.extend(i64::MAX.to_le_bytes());

    let mut record = new_record(24, 1, 0);
    let offset = add_nonresident_data_runs(
        &mut record,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Data,
        "",
        0,
        0,
        CLUSTER_SIZE as u64,
        &runs,
    );
    finish_record(&mut record, offset);

    let file = Record::new(24, &record).expect("record");
    let attribute = file.attributes().next().expect("data attribute");
    let result = attribute.nonresident_data_runs(&test_volume());
    assert!(result.is_err(), "{result:?}");
}

// A record whose base reference is itself is not its own extension; naming an extension record
// as its base means it belongs to no file (no records, no attributes - a base is never an
// extension record). A real base/extension pair next to it must still combine.
#[test]
fn self_referencing_record_yields_attributes_once() {
    let mut looped = new_record(24, 1, (1u64 << 48) | 24);
    let mut offset = add_standard_information(&mut looped, ATTRIBUTES_OFFSET, 0);
    offset = add_file_name(&mut looped, offset, "loop.txt", 0);
    finish_record(&mut looped, offset);

    let mut base = new_record(25, 1, 0);
    let offset = add_standard_information(&mut base, ATTRIBUTES_OFFSET, 0);
    finish_record(&mut base, offset);
    let mut extension = new_record(26, 1, (1u64 << 48) | 25);
    let offset = add_file_name(&mut extension, ATTRIBUTES_OFFSET, "base.txt", 0);
    finish_record(&mut extension, offset);

    let mft = mft_with(vec![looped, base, extension]);

    let looped = mft.record(24).expect("record 24");
    assert_eq!(looped.records().count(), 0);
    assert_eq!(looped.attributes().count(), 0);

    let base = mft.record(25).expect("record 25");
    assert_eq!(base.records().count(), 2);
    assert_eq!(base.attributes().count(), 2);
}

// `corrupt_records` counts allocated slots (per the bitmap) that could not be used, whichever
// check rejected them: a failed update sequence check, or a header that is not a `FILE` record.
// A free slot is never a corrupt record, whatever bytes it holds.
#[test]
fn corrupt_records_counts_only_allocated_slots() {
    let first = FIRST_NORMAL_RECORD;
    let good = |number: u64| {
        let mut record = new_record(number, 1, 0);
        let offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, "good.txt", 0);
        finish_record(&mut record, offset);
        record
    };
    let bad_fixup = |number: u64| {
        let mut record = good(number);
        record[RECORD_SIZE - 2] ^= 0xFF;
        record
    };
    // Passes the update sequence check (no array) but is no `FILE` record.
    let not_a_record = || vec![0u8; RECORD_SIZE];

    let (allocated_bad_fixup, allocated_not_a_record) = (first + 1, first + 2);
    let (free_bad_fixup, free_not_a_record, free_bad_fixup_too) = (first + 3, first + 4, first + 5);
    let (volume, data, mut bitmap) = raw_parts(vec![
        good(first),
        bad_fixup(allocated_bad_fixup),
        not_a_record(),
        bad_fixup(free_bad_fixup),
        not_a_record(),
        bad_fixup(free_bad_fixup_too),
    ]);
    for number in [free_bad_fixup, free_not_a_record, free_bad_fixup_too] {
        bitmap[number as usize / 8] &= !(1 << (number % 8));
    }

    let mft = build_from_parts(volume, data, bitmap);

    assert_eq!(mft.corrupt_records(), 2);
    let files: Vec<_> = mft.files().map(|file| file.number()).collect();
    assert_eq!(files, [first]);
    for number in [allocated_bad_fixup, allocated_not_a_record] {
        assert!(mft.is_allocated(number) && mft.record(number).is_none());
    }
}

// Windows rejects a record whose update sequence array does not hold one saved value per sector;
// accepting one would leave sector ends past the array unrestored.
#[test]
fn a_short_or_long_update_sequence_array_is_rejected() {
    let first = FIRST_NORMAL_RECORD as usize;
    for count in [1u16, 2, 4, 200] {
        let mut good = new_record(first as u64, 1, 0);
        let offset = add_file_name(&mut good, ATTRIBUTES_OFFSET, "good.txt", 0);
        finish_record(&mut good, offset);
        let mut bad = new_record(first as u64 + 1, 1, 0);
        let offset = add_file_name(&mut bad, ATTRIBUTES_OFFSET, "bad.txt", 0);
        finish_record(&mut bad, offset);
        write_u16(&mut bad, 6, count);

        assert!(
            Record::new(first as u64 + 1, &bad).is_none(),
            "an array of {count} entries passes for a 2 sector record"
        );
        let mft = mft_with(vec![good, bad]);
        let names: Vec<_> = mft
            .files()
            .map(|file| file.best_name().expect("name").to_string())
            .collect();
        assert_eq!(names, ["good.txt"], "array of {count}");
        assert_eq!(mft.corrupt_records(), 1, "array of {count}");
        assert!(mft.record(first as u64 + 1).is_none());
    }
}

// A record's data crosses the end of its first sector, where the update sequence number sits
// on disk; the fixup must restore the real bytes.
#[test]
fn fixups_restore_the_data_at_the_end_of_each_sector() {
    // Fills the record to its last byte: the value covers both sector ends.
    let value: Vec<u8> = (0..839).map(|index| (index % 251) as u8 + 1).collect();
    let mut record = new_record(FIRST_NORMAL_RECORD, 1, 0);
    let mut offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, "f.txt", 0);
    offset = add_resident_attribute(&mut record, offset, NtfsAttributeType::Data, 2, "", &value);
    assert!(
        offset > SECTOR_SIZE + 500,
        "the value must cross both sector ends"
    );
    finish_record(&mut record, offset);

    let protected = record.clone();
    assert_ne!(
        &protected[SECTOR_SIZE - 2..SECTOR_SIZE],
        &record_without_protection(&record)[SECTOR_SIZE - 2..SECTOR_SIZE],
        "the sector end holds the update sequence number on disk"
    );

    let mft = mft_with(vec![record]);
    assert_eq!(mft.corrupt_records(), 0);
    let file = mft.files().next().expect("one file");
    assert_eq!(file.resident_data(), Some(&value[..]));
}

/// The record with each sector end replaced by the saved value, by hand.
fn record_without_protection(record: &[u8]) -> Vec<u8> {
    let mut record = record.to_vec();
    for sector in 0..RECORD_SIZE / SECTOR_SIZE {
        let end = (sector + 1) * SECTOR_SIZE - 2;
        let slot = UPDATE_SEQUENCE_OFFSET + 2 + sector * 2;
        record.copy_within(slot..slot + 2, end);
    }
    record
}

/// `$MFT`'s `$DATA` in two extents: VCN 0 in record 0, and `extension_vcn` in record 3 (one
/// cluster each), reached through an attribute list built from `entries`.
fn read_split_mft_data(
    entries: &[Vec<u8>],
    extension_vcn: u64,
) -> NtfsReaderResult<Option<Vec<u8>>> {
    let mut base = new_record(0, 1, 0);
    let mut offset = add_resident_attribute(
        &mut base,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::AttributeList,
        0,
        "",
        &entries.concat(),
    );
    offset = add_nonresident_data_runs(
        &mut base,
        offset,
        NtfsAttributeType::Data,
        "",
        0,
        0,
        2 * CLUSTER_SIZE as u64,
        &[0x11, 0x01, 0x01],
    );
    finish_record(&mut base, offset);

    let mut extension = new_record(3, 1, 1u64 << 48);
    let offset = add_nonresident_data_runs(
        &mut extension,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Data,
        "",
        extension_vcn,
        extension_vcn,
        0,
        &[0x11, 0x01, 0x02],
    );
    finish_record(&mut extension, offset);

    let mut image = vec![0u8; 4 * CLUSTER_SIZE];
    image[7168..7168 + RECORD_SIZE].copy_from_slice(&extension);
    Mft::read_data_fs(
        &test_volume(),
        &mut std::io::Cursor::new(image),
        &base,
        NtfsAttributeType::Data,
    )
}

// Extents of a value must follow each other by VCN. A missing extent (the second starts at
// VCN 2, after a gap) or one listed twice would otherwise still be joined, shifting everything
// after it.
#[test]
fn read_data_fs_rejects_extents_that_do_not_follow_each_other() {
    let entry = |vcn, record| list_entry(NtfsAttributeType::Data, "", vcn, record);
    let list_start = list_entry(NtfsAttributeType::AttributeList, "", 0, 0);

    let joined = read_split_mft_data(&[list_start.clone(), entry(0, 0), entry(1, 3)], 1)
        .expect("read")
        .expect("present");
    assert_eq!(joined.len(), 2 * CLUSTER_SIZE, "the well-formed pair joins");

    let gap = read_split_mft_data(&[list_start.clone(), entry(0, 0), entry(2, 3)], 2);
    assert!(
        matches!(gap, Err(NtfsReaderError::InvalidDataRun { .. })),
        "extents at VCN 0 and 2 (one cluster each): {gap:?}"
    );
    let repeated = read_split_mft_data(&[list_start, entry(0, 0), entry(1, 3), entry(1, 3)], 1);
    assert!(
        matches!(repeated, Err(NtfsReaderError::InvalidDataRun { .. })),
        "the same extent twice: {repeated:?}"
    );
}
