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

// A list entry whose target record's sequence no longer matches what the
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

/// A compact `Mft` finds the same live file, with the same `FileInfo`, as the ordinary
/// fixed-stride one built from the same bytes, and ends up smaller (a name and a size take far
/// less than a whole 1 KiB record).
#[test]
fn new_compact_matches_from_parts_and_is_smaller() {
    let mut file = new_record(24, 1, 0);
    let offset = add_standard_information(&mut file, ATTRIBUTES_OFFSET, 0);
    let offset = add_file_name(&mut file, offset, "a.txt", 0);
    finish_record(&mut file, offset);

    let (volume, data, bitmap) = raw_parts(vec![file]);
    let whole = build_from_parts(volume.clone(), data.clone(), bitmap.clone());
    let compact = Mft::from_parts_compact(volume, data, bitmap).expect("a compact Mft");

    let mut cache = DefaultPathCache::new();
    let expected: Vec<_> = whole
        .files()
        .map(|file| (file.number(), FileInfo::with_cache(&file, &mut cache)))
        .collect();
    let found: Vec<_> = compact
        .files()
        .map(|file| (file.number(), FileInfo::with_cache(&file, &mut cache)))
        .collect();
    assert_eq!(found, expected);
    assert_eq!(found[0].1.name, "a.txt");

    assert!(
        compact.size_in_memory() < whole.size_in_memory(),
        "compact {} must be smaller than whole {}",
        compact.size_in_memory(),
        whole.size_in_memory()
    );
}

/// A compact `Mft` keeps the same extension-record index and freed records as a whole `Mft`,
/// preserving the metadata needed by `deleted_files()` and `is_deleted()`.
#[test]
fn new_compact_indexes_extensions_and_keeps_freed_records() {
    let mut live = new_record(24, 1, 0);
    let offset = add_standard_information(&mut live, ATTRIBUTES_OFFSET, 0);
    let offset = add_file_name(&mut live, offset, "live.txt", 0);
    finish_record(&mut live, offset);

    let mut extension = new_record(25, 1, reference(1, 24));
    let offset = add_nonresident_data(&mut extension, ATTRIBUTES_OFFSET, 5000);
    finish_record(&mut extension, offset);

    let freed_number = FIRST_NORMAL_RECORD + 2;
    let mut freed = new_record(freed_number, 3, 0);
    let offset = add_standard_information(&mut freed, ATTRIBUTES_OFFSET, 0);
    let offset = add_file_name(&mut freed, offset, "gone.txt", 0);
    finish_record(&mut freed, offset);
    mark_freed(&mut freed);

    let (volume, data, mut bitmap) = raw_parts(vec![live, extension, freed]);
    bitmap[freed_number as usize / 8] &= !(1 << (freed_number % 8));

    let whole = build_from_parts(volume.clone(), data.clone(), bitmap.clone());
    let names = |mft: &Mft| -> Vec<u64> { mft.files().map(|file| file.number()).collect() };
    let record_names =
        |mft: &Mft, base: u64| -> usize { mft.record(base).unwrap().records().count() };

    assert_eq!(names(&whole), vec![FIRST_NORMAL_RECORD]);
    assert_eq!(record_names(&whole, FIRST_NORMAL_RECORD), 2);
    assert_eq!(whole.deleted_files().count(), 1);

    let compact = Mft::from_parts_compact(volume, data, bitmap).expect("a compact Mft");
    assert_eq!(names(&compact), vec![FIRST_NORMAL_RECORD]);
    assert_eq!(record_names(&compact, FIRST_NORMAL_RECORD), 2);
    assert_eq!(compact.deleted_files().count(), 1);
    assert!(compact.record(freed_number).is_some());
}

fn assert_compact_keeps_record_with_the_usa_after_used_size(freed: bool) {
    let mut record = new_record(24, 2, 0);
    let offset = add_standard_information(&mut record, ATTRIBUTES_OFFSET, 0);
    let offset = add_file_name(&mut record, offset, "late-usa.txt", 0);
    finish_record(&mut record, offset);
    record.copy_within(UPDATE_SEQUENCE_OFFSET..UPDATE_SEQUENCE_OFFSET + 6, 900);
    write_u16(&mut record, 4, 900);
    if freed {
        mark_freed(&mut record);
    }
    let (volume, data, mut bitmap) = raw_parts(vec![record]);
    if freed {
        bitmap[24 / 8] &= !(1 << (24 % 8));
    }
    let whole = build_from_parts(volume.clone(), data.clone(), bitmap.clone());
    let compact = Mft::from_parts_compact(volume, data, bitmap).unwrap();
    let expected = whole
        .record(24)
        .expect("whole loader accepts this USA layout");
    let found = compact
        .record(24)
        .expect("compact loader must retain the accepted record's USA");
    assert_eq!(FileInfo::new(&found), FileInfo::new(&expected));
    assert_eq!(found.is_deleted(), freed);
    assert_eq!(compact.files().count(), whole.files().count());
    assert_eq!(
        compact.deleted_files().count(),
        whole.deleted_files().count()
    );
    assert_eq!(compact.corrupt_records(), whole.corrupt_records());
    assert_eq!(compact.corrupt_records(), 0);
}

#[test]
fn compact_keeps_live_record_with_the_usa_after_used_size() {
    assert_compact_keeps_record_with_the_usa_after_used_size(false);
}

#[test]
fn compact_keeps_freed_record_with_the_usa_after_used_size() {
    assert_compact_keeps_record_with_the_usa_after_used_size(true);
}

#[test]
fn compact_allocation_failures_return_errors() {
    let mut live = new_record(24, 1, 0);
    let end = add_file_name(&mut live, ATTRIBUTES_OFFSET, "live", 0);
    finish_record(&mut live, end);
    let mut live_extension = new_record(25, 1, reference(1, 24));
    let end = add_nonresident_data(&mut live_extension, ATTRIBUTES_OFFSET, 123);
    finish_record(&mut live_extension, end);
    let mut freed = new_record(26, 2, 0);
    let end = add_file_name(&mut freed, ATTRIBUTES_OFFSET, "freed", 0);
    finish_record(&mut freed, end);
    mark_freed(&mut freed);
    let mut freed_extension = new_record(27, 2, reference(1, 26));
    let end = add_nonresident_data(&mut freed_extension, ATTRIBUTES_OFFSET, 456);
    finish_record(&mut freed_extension, end);
    mark_freed(&mut freed_extension);
    let (volume, data, mut bitmap) = raw_parts(vec![live, live_extension, freed, freed_extension]);
    bitmap[26 / 8] &= !((1 << (26 % 8)) | (1 << (27 % 8)));
    let compact_bytes: u64 = data
        .as_chunks::<RECORD_SIZE>()
        .0
        .iter()
        .map(|record| u32::from_le_bytes(record[24..28].try_into().unwrap()) as u64)
        .map(|used| used.next_multiple_of(8))
        .sum();
    // Chunk buffer, compact bytes, offset table, live and freed extension indexes.
    let requests = [
        data.len() as u64,
        compact_bytes,
        (data.len() as u64 / 1024 + 1) * 8,
        16,
        16,
    ];
    for (successful, expected) in requests.into_iter().enumerate() {
        let result = allocation_budget::after(successful, || {
            Mft::from_parts_compact(volume.clone(), data.clone(), bitmap.clone())
        });
        assert!(
            matches!(result, Err(NtfsReaderError::AllocationTooLarge { size }) if size == expected),
            "allocation {successful} ({expected} bytes) must return AllocationTooLarge: {result:?}"
        );
    }
    let compact = allocation_budget::after(requests.len(), || {
        Mft::from_parts_compact(volume, data, bitmap)
    })
    .unwrap();
    assert_eq!(compact.record(24).unwrap().records().count(), 2);
    assert_eq!(compact.record(26).unwrap().records().count(), 2);
}

/// Switch images when the compact loader seeks back after reading its first image in full.
struct ChangingCompactReader {
    cursor: std::io::Cursor<Vec<u8>>,
    second: Option<Vec<u8>>,
}

impl Read for ChangingCompactReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.cursor.read(buf)
    }
}

impl Seek for ChangingCompactReader {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        if position == SeekFrom::Start(0)
            && self.cursor.position() == self.cursor.get_ref().len() as u64
        {
            if let Some(second) = self.second.take() {
                self.cursor = std::io::Cursor::new(second);
            }
        }
        self.cursor.seek(position)
    }
}

fn changing_compact(existing: bool) -> NtfsReaderResult<Mft> {
    let record = |contents: &[u8]| {
        let mut record = new_record(24, 1, 0);
        let end = add_file_name(&mut record, ATTRIBUTES_OFFSET, "growing.txt", 0);
        let end =
            add_resident_attribute(&mut record, end, NtfsAttributeType::Data, 1, "", contents);
        finish_record(&mut record, end);
        record
    };
    let (volume, mut first, bitmap) = raw_parts(vec![record(&[])]);
    if !existing {
        first.fill(0);
    }
    let (_, second, _) = raw_parts(vec![record(&[0xAB; 512])]);
    let size = first.len() as u64;
    let value = MftValue::Runs(RunCursor::new(
        &volume,
        size,
        vec![DataRun::Data {
            offset: 0,
            length: size,
        }],
    )?);
    let reader = ChangingCompactReader {
        cursor: std::io::Cursor::new(first),
        second: Some(second),
    };
    Mft::from_value_compact(volume, reader, value, bitmap)
}

fn assert_compact_growth_is_fallible(existing: bool, successful: usize) {
    let result = allocation_budget::after(successful, || changing_compact(existing));
    assert!(
        matches!(result, Err(NtfsReaderError::AllocationTooLarge { .. })),
        "growing second-pass records must reserve fallibly: {result:?}"
    );
    let compact = changing_compact(existing).unwrap();
    let file = compact.record(24).expect("second-pass record is retained");
    assert_eq!(FileInfo::new(&file).size, 512);
    assert_eq!(FileInfo::new(&file).name, "growing.txt");
    assert_eq!(compact.corrupt_records(), 0);
    assert_eq!(compact.files().count(), 1);
}

#[test]
fn compact_newly_valid_records_reserve_fallibly() {
    // The initially empty compact buffer needs no allocation until the second pass.
    assert_compact_growth_is_fallible(false, 2);
}

#[test]
fn compact_growing_records_reserve_fallibly() {
    assert_compact_growth_is_fallible(true, 3);
}

#[test]
fn compact_size_arithmetic_rejects_unrepresentable_allocations() {
    assert_eq!(checked_compact_entries(24).unwrap(), 25);
    assert!(matches!(
        checked_compact_entries(u64::MAX),
        Err(NtfsReaderError::AllocationTooLarge { .. })
    ));
    let table = checked_compact_entries(u32::MAX as u64);
    let four_gib_table = checked_compact_entries(536_870_911);
    let bytes = checked_chunk_bytes(4_294_967_296, 1);
    if usize::BITS == 32 {
        assert!(
            matches!(
                four_gib_table,
                Err(NtfsReaderError::AllocationTooLarge {
                    size: 4_294_967_296
                })
            ),
            "representable entry count still needs a 4 GiB offset table: {four_gib_table:?}"
        );
        assert!(
            matches!(table, Err(NtfsReaderError::AllocationTooLarge { .. })),
            "compact offset table must not narrow on i686: {table:?}"
        );
        assert!(
            matches!(
                bytes,
                Err(NtfsReaderError::AllocationTooLarge {
                    size: 4_294_967_296
                })
            ),
            "compact byte count must not narrow on i686: {bytes:?}"
        );
    } else {
        assert_eq!(four_gib_table.unwrap(), 536_870_912);
        assert_eq!(table.unwrap() as u64, 4_294_967_296);
        assert_eq!(bytes.unwrap() as u64, 4_294_967_296);
    }
}

/// Chunk sizing must check both multiplication overflow and narrowing to `usize`. An unchecked
/// `(count * record_size) as usize` truncates once the product exceeds 32-bit `usize` capacity.
/// Exercise u64 overflow separately from a 4 GiB product that fits u64 but not 32-bit usize,
/// using the arithmetic helper without allocating a large buffer.
#[test]
fn checked_chunk_bytes_rejects_what_it_cannot_hold() {
    assert_eq!(checked_chunk_bytes(10, 1024).unwrap(), 10240);
    assert!(matches!(
        checked_chunk_bytes(u64::MAX, 2),
        Err(NtfsReaderError::AllocationTooLarge { .. })
    ));
    let four_gib = checked_chunk_bytes(4_194_304, 1024);
    if usize::BITS == 32 {
        assert!(
            matches!(
                four_gib,
                Err(NtfsReaderError::AllocationTooLarge {
                    size: 4_294_967_296
                })
            ),
            "4 GiB fits u64 but must not narrow into a 32-bit chunk: {four_gib:?}"
        );
    } else {
        assert_eq!(four_gib.unwrap() as u64, 4_294_967_296);
    }
}

#[test]
fn a_small_window_rejects_a_record_ending_at_four_gib() {
    let mut mft = mft_with(Vec::new());
    mft.record_count = 4_194_304;
    mft.load_window(0, 1, |data| {
        data.fill(0);
        Ok(())
    })
    .unwrap();
    assert!(mft.record(4_194_303).is_none());
    assert!(mft.record(4_194_304).is_none());
    // A nonzero window start must apply the same bound to the relative record index.
    mft.first = 1;
    mft.record_count += 1;
    assert!(mft.record(4_194_304).is_none());
}

#[test]
fn run_cursor_visits_each_sequential_run_once() {
    const RUNS: usize = 4096;
    let mut cursor = RunCursor::new(
        &test_volume(),
        (RUNS * RECORD_SIZE) as u64,
        vec![
            DataRun::Sparse {
                length: RECORD_SIZE as u64
            };
            RUNS
        ],
    )
    .unwrap();
    let mut reader = std::io::Cursor::new(Vec::new());
    let mut buf = [0xAB; RECORD_SIZE];
    for _ in 0..RUNS {
        assert_eq!(cursor.read(&mut reader, &mut buf).unwrap(), RECORD_SIZE);
        assert!(buf.iter().all(|&byte| byte == 0));
    }
    assert_eq!(cursor.read(&mut reader, &mut buf).unwrap(), 0);
    assert_eq!(
        cursor.run_visits, RUNS,
        "sequential reads must not revisit earlier runs"
    );
}

#[test]
fn run_cursor_preserves_bytes_across_chunk_boundaries_and_rewind() {
    let runs = vec![
        DataRun::Data {
            offset: 5,
            length: 3,
        },
        DataRun::Sparse { length: 0 },
        DataRun::Sparse { length: 3 },
        DataRun::Data {
            offset: 1,
            length: 5,
        },
        DataRun::Sparse { length: 2 },
        DataRun::Data {
            offset: 8,
            length: 4,
        },
    ];
    let expected = [5, 6, 7, 0, 0, 0, 1, 2, 3, 4, 5, 0, 0, 8, 9];
    for chunk_size in 1..=18 {
        let mut value = MftValue::Runs(
            RunCursor::new(&test_volume(), expected.len() as u64, runs.clone()).unwrap(),
        );
        let mut reader = std::io::Cursor::new((0..32).collect::<Vec<u8>>());
        for _ in 0..2 {
            let mut found = Vec::new();
            let mut buf = vec![0xAB; chunk_size];
            loop {
                let read = value.read(&mut reader, &mut buf).unwrap();
                if read == 0 {
                    break;
                }
                found.extend_from_slice(&buf[..read]);
            }
            assert_eq!(found, expected, "chunk size {chunk_size}");
            value.rewind();
        }
    }
}

/// Chunk-buffer growth must return `NtfsReaderError` when allocation fails rather than abort
/// through `Vec::resize`. `resize_checked` first reserves with `try_reserve_exact`.
#[test]
fn resize_checked_reports_an_allocation_that_cannot_be_made_instead_of_aborting() {
    let mut buf = Vec::new();
    assert!(matches!(
        resize_checked(&mut buf, usize::MAX),
        Err(NtfsReaderError::AllocationTooLarge { size }) if size == usize::MAX as u64
    ));
    resize_checked(&mut buf, 16).unwrap();
    assert_eq!(buf, vec![0u8; 16]);
}

/// Compact offsets use 8-byte units, so a u32 table would wrap past about 32 GiB of trimmed
/// records and make later records address the wrong spans. Exercise `push_compact_offset`
/// across that boundary without allocating the corresponding record data.
#[test]
fn compact_offsets_do_not_wrap_past_32_gib_of_trimmed_records() {
    // One 8-byte unit below the u32 offset boundary, then four more 8-byte records.
    let mut offsets = vec![0u64];
    let mut total = (u64::from(u32::MAX) - 1) * 8;
    for _ in 0..4 {
        push_compact_offset(&mut offsets, &mut total, 8);
    }
    assert!(
        offsets.windows(2).all(|pair| pair[0] < pair[1]),
        "offsets must strictly increase past the u32 boundary too: {offsets:?}"
    );
    let last = *offsets.last().unwrap();
    assert!(
        last > u64::from(u32::MAX),
        "the real offset is past u32::MAX: {last}"
    );
    assert_ne!(
        last, last as u32 as u64,
        "this offset would wrap in a u32 table"
    );
}

/// Compact trimming must preserve resident data at the end of a record: data filling the
/// record with no room for an End marker, a marker in the last eight bytes, and a shorter
/// record that can be trimmed. These exact boundaries expose alignment and off-by-one errors.
#[test]
fn new_compact_matches_at_used_size_edges() {
    for target in [RECORD_SIZE, RECORD_SIZE - 8, RECORD_SIZE - 16] {
        let mut base = new_record(24, 1, 0);
        let mut offset = ATTRIBUTES_OFFSET;
        offset = add_standard_information(&mut base, offset, 0);
        offset = add_file_name(&mut base, offset, "a.txt", 0);

        // Every offset these builders hand back is 8-aligned (they `align_to_eight` their own
        // length), and so is every `target`, so this fills the resident $DATA value to exactly
        // `target` bytes with nothing left to pad: `add_resident_attribute`'s own 8-byte
        // alignment of the value never has to round up.
        let value_len = target - offset - 24; // 24 = add_resident_attribute_raw's NAME_OFFSET.
        assert_eq!(
            value_len % 8,
            0,
            "target {target} is not reachable with no padding"
        );
        let value = vec![0xABu8; value_len];
        offset = add_resident_attribute(&mut base, offset, NtfsAttributeType::Data, 2, "", &value);
        assert_eq!(
            offset, target,
            "fixture did not reach the intended used_size"
        );
        finish_record(&mut base, offset);

        let (volume, data, bitmap) = raw_parts(vec![base]);
        let whole = build_from_parts(volume.clone(), data.clone(), bitmap.clone());
        let compact = Mft::from_parts_compact(volume, data, bitmap)
            .unwrap_or_else(|e| panic!("target {target}: a compact Mft: {e}"));

        let whole_file = whole
            .files()
            .next()
            .unwrap_or_else(|| panic!("target {target}: whole"));
        let compact_file = compact
            .files()
            .next()
            .unwrap_or_else(|| panic!("target {target}: compact"));
        assert_eq!(
            compact_file.resident_data(),
            whole_file.resident_data(),
            "target {target}: resident $DATA differs between Mft::new_compact and Mft::new"
        );
        assert_eq!(
            FileInfo::new(&compact_file),
            FileInfo::new(&whole_file),
            "target {target}: FileInfo differs between Mft::new_compact and Mft::new"
        );
    }
}
