// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! `MftScan` against a whole `Mft` of the same bytes. The property test is
//! `mft::tests::structured::a_scan_finds_what_the_whole_mft_finds`.

use std::io::{self, Cursor, SeekFrom};
use std::path::PathBuf;

use super::*;
use crate::api::NtfsFileNamespace;
use crate::data_run::DataRun;
use crate::file_info::FileInfo;
use crate::mft::test_records::*;
use crate::mft::RunCursor;
use crate::path::{walk_budget, CachedPath, DefaultPathCache, DeletedPathCache, DeletedPathMarker};

/// Raw `$MFT` bytes (no fixups) and bitmap with each record at its number, all allocated.
fn parts_at(records: Vec<(u64, Vec<u8>)>) -> (Volume, Vec<u8>, Vec<u8>) {
    let count = records
        .iter()
        .map(|(number, _)| number + 1)
        .max()
        .unwrap_or(0) as usize;
    let mut data = vec![0u8; count * RECORD_SIZE];
    let mut bitmap = vec![0u8; count.div_ceil(8)];
    for (number, record) in records {
        let number = number as usize;
        bitmap[number / 8] |= 1 << (number % 8);
        data[number * RECORD_SIZE..(number + 1) * RECORD_SIZE].copy_from_slice(&record);
    }
    (test_volume(), data, bitmap)
}

/// Clears the `$BITMAP` bit of every number in `numbers`, as a delete leaves it.
fn free(bitmap: &mut [u8], numbers: &[u64]) {
    for &number in numbers {
        bitmap[number as usize / 8] &= !(1 << (number % 8));
    }
}

fn named(number: u64, parent: u64, name: &str, directory: bool) -> Vec<u8> {
    let mut record = new_record(number, 1, 0);
    if directory {
        set_record_flags(&mut record, directory_flags());
    }
    let offset = add_standard_information(&mut record, ATTRIBUTES_OFFSET, 0);
    let offset = add_file_name_ex(
        &mut record,
        offset,
        1,
        parent,
        NtfsFileNamespace::Win32,
        name,
        0,
    );
    finish_record(&mut record, offset);
    record
}

/// A file under a directory numbered above it, with its data in an extension record further
/// on; that directory under one whose Win32 name is only in an extension record at the end.
fn tangled() -> (Volume, Vec<u8>, Vec<u8>) {
    let mut file = new_record(24, 1, 0);
    let offset = add_standard_information(&mut file, ATTRIBUTES_OFFSET, 0);
    let offset = add_file_name_ex(
        &mut file,
        offset,
        1,
        reference(1, 30),
        NtfsFileNamespace::Win32,
        "a.txt",
        0,
    );
    finish_record(&mut file, offset);

    let mut data = new_record(40, 1, reference(1, 24));
    let offset = add_nonresident_data(&mut data, ATTRIBUTES_OFFSET, 1234);
    finish_record(&mut data, offset);

    let mut top = new_record(35, 1, 0);
    set_record_flags(&mut top, directory_flags());
    let offset = add_standard_information(&mut top, ATTRIBUTES_OFFSET, 0);
    let offset = add_file_name_ex(
        &mut top,
        offset,
        1,
        reference(ROOT_SEQUENCE, ROOT_RECORD),
        NtfsFileNamespace::Dos,
        "TOP~1",
        0,
    );
    finish_record(&mut top, offset);

    let mut top_name = new_record(45, 1, reference(1, 35));
    let offset = add_file_name(&mut top_name, ATTRIBUTES_OFFSET, "top", 0);
    finish_record(&mut top_name, offset);

    parts_at(vec![
        (ROOT_RECORD, root_record()),
        (24, file),
        (
            25,
            named(25, reference(ROOT_SEQUENCE, ROOT_RECORD), "b", false),
        ),
        (30, named(30, reference(1, 35), "dir", true)),
        (35, top),
        (40, data),
        (45, top_name),
    ])
}

fn whole_infos(volume: Volume, data: Vec<u8>, bitmap: Vec<u8>) -> Vec<(u64, FileInfo)> {
    let mft = build_from_parts(volume, data, bitmap);
    let mut cache = DefaultPathCache::new();
    mft.files()
        .map(|file| (file.number(), FileInfo::with_cache(&file, &mut cache)))
        .collect()
}

fn scan_infos(scan: &mut MftScan) -> Vec<(u64, FileInfo)> {
    let mut cache = DefaultPathCache::new();
    let mut infos = Vec::new();
    while let Some(chunk) = scan.next_chunk().unwrap() {
        infos.extend(
            chunk
                .files()
                .map(|file| (file.number(), FileInfo::with_cache(&file, &mut cache))),
        );
    }
    infos
}

#[test]
fn a_scan_in_any_chunk_size_finds_what_the_whole_mft_finds() {
    let (volume, data, bitmap) = tangled();
    let expected = whole_infos(volume.clone(), data.clone(), bitmap.clone());
    let file = &expected.iter().find(|(number, _)| *number == 24).unwrap().1;
    let path: PathBuf = [VOLUME_PATH, "top", "dir", "a.txt"].iter().collect();
    assert_eq!(file.path, Some(path));
    assert_eq!(file.size, 1234);

    for chunk_records in 1..=46 {
        let mut scan =
            MftScan::from_parts(volume.clone(), data.clone(), bitmap.clone(), chunk_records)
                .unwrap();
        assert_eq!(scan_infos(&mut scan), expected, "chunks of {chunk_records}");
    }
}

// Every chunk shares one `Mft` owner, so a `DefaultPathCache` must stay warm across chunks.
// A chain of DEPTH directories has one file below each directory, with one record per chunk.
// A warm cache takes only a few steps per file; resetting it for each chunk repeats the walk
// from the root and makes the total work quadratic in DEPTH.
#[test]
fn a_default_path_cache_stays_warm_across_the_chunks_of_a_scan() {
    const DEPTH: u64 = 50;
    const FIRST_DIR: u64 = 24;
    let first_file = FIRST_DIR + DEPTH;

    let mut records = vec![(ROOT_RECORD, root_record())];
    for level in 0..DEPTH {
        let parent = if level == 0 {
            reference(ROOT_SEQUENCE, ROOT_RECORD)
        } else {
            reference(1, FIRST_DIR + level - 1)
        };
        records.push((
            FIRST_DIR + level,
            named(FIRST_DIR + level, parent, "d", true),
        ));
    }
    for level in 0..DEPTH {
        let parent = reference(1, FIRST_DIR + level);
        records.push((
            first_file + level,
            named(first_file + level, parent, "f", false),
        ));
    }
    let (volume, data, bitmap) = parts_at(records);

    let mut scan = MftScan::from_parts(volume, data, bitmap, 1).unwrap();
    let mut cache = DefaultPathCache::new();
    let (_, steps) = walk_budget::run(6 * DEPTH, || {
        while let Some(chunk) = scan.next_chunk().unwrap() {
            for file in chunk.files() {
                if let Some(name) = file.best_name() {
                    chunk.resolve_path(&name, &mut cache);
                }
            }
        }
    });
    eprintln!("depth {DEPTH}, {steps} steps");
}

/// A live directory "top", a deleted directory "dir" under it, a deleted file "a.txt" under
/// "dir" whose data is in a freed extension record: every kept-record kind (live directory,
/// freed directory, freed extension record) in one image.
fn tangled_deleted() -> (Volume, Vec<u8>, Vec<u8>) {
    let mut top = new_record(35, 1, 0);
    set_record_flags(&mut top, directory_flags());
    let offset = add_standard_information(&mut top, ATTRIBUTES_OFFSET, 0);
    let offset = add_file_name_ex(
        &mut top,
        offset,
        1,
        reference(ROOT_SEQUENCE, ROOT_RECORD),
        NtfsFileNamespace::Win32,
        "top",
        0,
    );
    finish_record(&mut top, offset);

    // `named` gives every record sequence 1; `delete_record` bumps the stored sequence to 2 and
    // clears the in-use flag, so a child still names it as sequence 1 (one below its own, as
    // freeing leaves every reference held elsewhere).
    let mut dir = named(30, reference(1, 35), "dir", true);
    delete_record(&mut dir);

    let mut file = named(24, reference(1, 30), "a.txt", false);
    delete_record(&mut file);

    let mut data = new_record(40, 1, reference(1, 24));
    let offset = add_nonresident_data(&mut data, ATTRIBUTES_OFFSET, 1234);
    finish_record(&mut data, offset);
    delete_record(&mut data);

    let (volume, data_bytes, mut bitmap) = parts_at(vec![
        (ROOT_RECORD, root_record()),
        (24, file),
        (30, dir),
        (35, top),
        (40, data),
    ]);
    free(&mut bitmap, &[24, 30, 40]);
    (volume, data_bytes, bitmap)
}

/// Changes the disk image on the first rewind after a complete read, so pass 2 stays stable
/// while differing from what pass 1 indexed.
struct ChangedBetweenPasses {
    current: Cursor<Vec<u8>>,
    next: Option<Vec<u8>>,
}

impl Read for ChangedBetweenPasses {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.current.read(buf)
    }
}

impl Seek for ChangedBetweenPasses {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if position == SeekFrom::Start(0)
            && self.current.position() == self.current.get_ref().len() as u64
        {
            if let Some(next) = self.next.take() {
                self.current = Cursor::new(next);
            }
        }
        self.current.seek(position)
    }
}

fn scan_changed_between_passes(
    volume: Volume,
    first: Vec<u8>,
    second: Vec<u8>,
    bitmap: Vec<u8>,
    chunk_records: u64,
) -> MftScan {
    assert_eq!(first.len(), second.len());
    let length = first.len() as u64;
    let value = MftValue::Runs(
        RunCursor::new(&volume, length, vec![DataRun::Data { offset: 0, length }]).unwrap(),
    );
    let reader = Box::new(ChangedBetweenPasses {
        current: Cursor::new(first),
        next: Some(second),
    });
    MftScan::from_value(volume, reader, value, bitmap, chunk_records).unwrap()
}

/// Directory 30 fails pass 1's fixups, then is repaired before pass 2. Children before, in and
/// after its chunk reach it through indexed directory 29, so the deleted cache also remembers
/// a chain ending at the missing directory.
fn scan_with_repaired_parent(deleted: bool) -> MftScan {
    let mut records = vec![
        (ROOT_RECORD, root_record()),
        (24, named(24, reference(1, 29), "before", false)),
        (29, named(29, reference(1, 30), "childdir", true)),
        (
            30,
            named(30, reference(ROOT_SEQUENCE, ROOT_RECORD), "dir", true),
        ),
        (31, named(31, reference(1, 29), "near", false)),
        (40, named(40, reference(1, 29), "after", false)),
    ];
    if deleted {
        for (number, record) in &mut records {
            if *number != ROOT_RECORD {
                delete_record(record);
            }
        }
    }
    let (volume, second, mut bitmap) = parts_at(records);
    if deleted {
        free(&mut bitmap, &[24, 29, 30, 31, 40]);
    }
    let mut first = second.clone();
    first[30 * RECORD_SIZE + 510] ^= 1;
    scan_changed_between_passes(volume, first, second, bitmap, 4)
}

fn check_repaired_parent_live_paths(evict: bool) {
    let mut scan = scan_with_repaired_parent(false);
    let mut cache = if evict {
        DefaultPathCache::with_max_bytes(1024)
    } else {
        DefaultPathCache::new()
    };
    let mut visited = Vec::new();
    while let Some(chunk) = scan.next_chunk().unwrap() {
        for number in [24, 31, 40] {
            if !chunk.records().contains(&number) {
                continue;
            }
            let name = chunk.record(number).unwrap().best_name().unwrap();
            let warm = chunk.resolve_path(&name, &mut cache);
            let cold = chunk.resolve_path(&name, &mut ());
            assert_eq!(warm, cold, "cache changed the path at record {number}");
            assert_eq!(cold, None, "pass 2 revived an unindexed parent at {number}");
            visited.push(number);
            if evict {
                for other in 100..200 {
                    cache.insert_failed(reference(1, other));
                }
                assert_eq!(cache.get(reference(1, 29)), CachedPath::Unknown);
            }
        }
    }
    assert_eq!(visited, [24, 31, 40]);
}

#[test]
fn scan_directory_misses_are_stable_with_a_warm_live_cache() {
    check_repaired_parent_live_paths(false);
}

#[test]
fn scan_directory_misses_are_stable_with_live_cache_eviction() {
    check_repaired_parent_live_paths(true);
}

fn check_repaired_parent_deleted_paths(deleted: bool) {
    let mut scan = scan_with_repaired_parent(deleted);
    let mut cache = DeletedPathCache::new();
    let mut visited = Vec::new();
    while let Some(chunk) = scan.next_chunk().unwrap() {
        for number in [24, 31, 40] {
            if !chunk.records().contains(&number) {
                continue;
            }
            let name = chunk.record(number).unwrap().best_name().unwrap();
            let warm = chunk.resolve_deleted_path(&name, &mut cache);
            let cold = chunk.resolve_deleted_path(&name, &mut DeletedPathCache::new());
            assert_eq!(
                warm, cold,
                "cache changed the deleted path at record {number}"
            );
            assert_eq!(cold.marker, Some(DeletedPathMarker::Lost(30)));
            assert_eq!(
                cold.path,
                PathBuf::from("childdir").join(name.to_os_string())
            );
            visited.push(number);
        }
    }
    assert_eq!(visited, [24, 31, 40]);
}

#[test]
fn scan_directory_misses_are_stable_with_live_deleted_path_parents() {
    check_repaired_parent_deleted_paths(false);
}

#[test]
fn scan_directory_misses_are_stable_with_freed_deleted_path_parents() {
    check_repaired_parent_deleted_paths(true);
}

#[test]
fn scan_root_misses_are_stable_after_the_root_is_repaired() {
    let (volume, second, bitmap) = parts_at(vec![
        (ROOT_RECORD, root_record()),
        (
            24,
            named(24, reference(ROOT_SEQUENCE, ROOT_RECORD), "a", false),
        ),
    ]);
    let whole = build_from_parts(volume.clone(), second.clone(), bitmap.clone());
    let name = whole.record(24).unwrap().best_name().unwrap();
    let mut first = second.clone();
    first[ROOT_RECORD as usize * RECORD_SIZE + 510] ^= 1;
    let mut scan = scan_changed_between_passes(volume, first, second, bitmap, 4);
    let mut live = DefaultPathCache::new();
    let mut deleted = DeletedPathCache::new();
    while let Some(chunk) = scan.next_chunk().unwrap() {
        assert_eq!(chunk.resolve_path(&name, &mut live), None);
        assert_eq!(chunk.resolve_path(&name, &mut ()), None);
        for resolved in [
            chunk.resolve_deleted_path(&name, &mut deleted),
            chunk.resolve_deleted_path(&name, &mut DeletedPathCache::new()),
        ] {
            assert_eq!(resolved.marker, Some(DeletedPathMarker::Lost(ROOT_RECORD)));
            assert_eq!(resolved.path, PathBuf::from("a"));
        }
    }
}

fn check_extension_requires_base(deleted: bool) {
    let (volume, data, bitmap) = if deleted {
        tangled_deleted()
    } else {
        tangled()
    };
    let whole = build_from_parts(volume.clone(), data.clone(), bitmap.clone());
    let expected = whole.record(40).unwrap();
    assert_eq!(expected.is_deleted(), deleted);
    assert_eq!(FileInfo::new(&expected).size, 1234);
    let expected_records: Vec<_> = expected.records().map(|record| record.number()).collect();
    let expected_streams: Vec<_> = expected.data_streams().collect();
    let expected_names: Vec<_> = expected.names().map(|name| name.to_os_string()).collect();

    let mut scan = MftScan::from_parts(volume, data, bitmap, 1).unwrap();
    let mut found_base = false;
    let mut found_extension = false;
    while let Some(chunk) = scan.next_chunk().unwrap() {
        if chunk.records().contains(&24) {
            let extension = chunk
                .record(40)
                .expect("base and retained extension are available");
            assert_eq!(extension.is_deleted(), deleted);
            assert_eq!(FileInfo::new(&extension), FileInfo::new(&expected));
            assert_eq!(
                extension
                    .records()
                    .map(|record| record.number())
                    .collect::<Vec<_>>(),
                expected_records
            );
            assert_eq!(
                extension.data_streams().collect::<Vec<_>>(),
                expected_streams
            );
            assert_eq!(
                extension
                    .names()
                    .map(|name| name.to_os_string())
                    .collect::<Vec<_>>(),
                expected_names
            );
            found_base = true;
        } else {
            assert!(
                chunk.record(40).is_none(),
                "extension 40 was exposed without base 24 in chunk {:?}",
                chunk.records()
            );
        }
        found_extension |= chunk.records().contains(&40);
    }
    assert!(found_base && found_extension);
}

#[test]
fn a_live_extension_is_returned_only_with_its_base_view() {
    check_extension_requires_base(false);
}

#[test]
fn a_freed_extension_is_returned_only_with_its_base_view() {
    check_extension_requires_base(true);
}

// The whole-MFT reference rule accepts both 0 and 0xFFFF for a freed directory stored at 1.
// A directory index must preserve that rule, not just its canonical pre-delete file id.
#[test]
fn scan_freed_directory_reference_aliases_match_the_whole_mft() {
    for (stored, claimed, accepted) in [
        (1, 0, true),
        (1, 0xFFFF, true),
        (0, 0, false),
        (0, 0xFFFF, true),
        (2, 0, false),
        (2, 1, true),
    ] {
        let mut directory = named(30, reference(1, 13), "dir", true);
        mark_freed(&mut directory);
        write_u16(&mut directory, 16, stored);
        for deleted in [false, true] {
            let mut file = named(24, reference(claimed, 30), "leaf", false);
            if deleted {
                delete_record(&mut file);
            }
            let (volume, data, mut bitmap) = parts_at(vec![
                (ROOT_RECORD, root_record()),
                (24, file),
                (30, directory.clone()),
            ]);
            free(&mut bitmap, &[30]);
            if deleted {
                free(&mut bitmap, &[24]);
            }
            let expected = DeletedPath {
                marker: Some(DeletedPathMarker::Lost(if accepted { 13 } else { 30 })),
                path: if accepted {
                    PathBuf::from("dir").join("leaf")
                } else {
                    PathBuf::from("leaf")
                },
            };
            let whole = build_from_parts(volume.clone(), data.clone(), bitmap.clone());
            let name = whole.record(24).unwrap().best_name().unwrap();
            assert_eq!(
                whole.resolve_deleted_path(&name, &mut DeletedPathCache::new()),
                expected
            );
            for chunk_records in [1, 4, 40] {
                let mut scan = MftScan::from_parts(
                    volume.clone(),
                    data.clone(),
                    bitmap.clone(),
                    chunk_records,
                )
                .unwrap();
                let mut cache = DeletedPathCache::new();
                while let Some(chunk) = scan.next_chunk().unwrap() {
                    assert_eq!(
                        chunk.resolve_deleted_path(&name, &mut cache),
                        expected,
                        "stored {stored}, claimed {claimed}, deleted {deleted}, chunk {:?}",
                        chunk.records()
                    );
                    assert_eq!(
                        chunk.resolve_deleted_path(&name, &mut DeletedPathCache::new()),
                        expected
                    );
                }
            }
        }
    }
}

#[test]
fn scan_freed_directory_extension_names_accept_reference_aliases() {
    for (claimed, accepted) in [(0, true), (0xFFFF, true), (1, false)] {
        let mut directory = new_record(30, 1, 0);
        set_record_flags(&mut directory, directory_flags());
        let end = add_standard_information(&mut directory, ATTRIBUTES_OFFSET, 0);
        finish_record(&mut directory, end);
        mark_freed(&mut directory);
        let mut extension = new_record(40, 1, reference(claimed, 30));
        let end = add_file_name_ex(
            &mut extension,
            ATTRIBUTES_OFFSET,
            0,
            reference(1, 13),
            NtfsFileNamespace::Win32,
            "dir",
            0,
        );
        finish_record(&mut extension, end);
        mark_freed(&mut extension);
        let (volume, data, mut bitmap) = parts_at(vec![
            (ROOT_RECORD, root_record()),
            (24, named(24, reference(0xFFFF, 30), "leaf", false)),
            (30, directory),
            (40, extension),
        ]);
        free(&mut bitmap, &[30, 40]);
        let expected = DeletedPath {
            marker: Some(DeletedPathMarker::Lost(if accepted { 13 } else { 30 })),
            path: if accepted {
                PathBuf::from("dir").join("leaf")
            } else {
                PathBuf::from("leaf")
            },
        };
        let whole = build_from_parts(volume.clone(), data.clone(), bitmap.clone());
        let name = whole.record(24).unwrap().best_name().unwrap();
        assert_eq!(
            whole.resolve_deleted_path(&name, &mut DeletedPathCache::new()),
            expected
        );
        let mut scan = MftScan::from_parts(volume, data, bitmap, 1).unwrap();
        while let Some(chunk) = scan.next_chunk().unwrap() {
            assert_eq!(
                chunk.resolve_deleted_path(&name, &mut DeletedPathCache::new()),
                expected,
                "extension base sequence {claimed}, chunk {:?}",
                chunk.records()
            );
        }
    }
}

fn whole_deleted_infos(volume: Volume, data: Vec<u8>, bitmap: Vec<u8>) -> Vec<(u64, FileInfo)> {
    let mft = build_from_parts(volume, data, bitmap);
    let mut cache = DeletedPathCache::new();
    mft.deleted_files()
        .map(|file| {
            (
                file.number(),
                FileInfo::with_caches(&file, &mut (), &mut cache),
            )
        })
        .collect()
}

fn scan_deleted_infos(scan: &mut MftScan) -> Vec<(u64, FileInfo)> {
    let mut cache = DeletedPathCache::new();
    let mut infos = Vec::new();
    while let Some(chunk) = scan.next_chunk().unwrap() {
        infos.extend(chunk.deleted_files().map(|file| {
            (
                file.number(),
                FileInfo::with_caches(&file, &mut (), &mut cache),
            )
        }));
    }
    infos
}

#[test]
fn a_scan_finds_deleted_files_through_a_deleted_directory() {
    let (volume, data, bitmap) = tangled_deleted();
    let expected = whole_deleted_infos(volume.clone(), data.clone(), bitmap.clone());
    let file = &expected.iter().find(|(number, _)| *number == 24).unwrap().1;
    let path: PathBuf = [VOLUME_PATH, "top", "dir", "a.txt"].iter().collect();
    assert_eq!(file.path, Some(path), "a live-then-freed chain resolves");
    assert_eq!(file.size, 1234);
    assert!(file.is_deleted);

    for chunk_records in 1..=45 {
        let mut scan =
            MftScan::from_parts(volume.clone(), data.clone(), bitmap.clone(), chunk_records)
                .unwrap();
        assert_eq!(
            scan_deleted_infos(&mut scan),
            expected,
            "chunks of {chunk_records}"
        );
    }
}

/// A freed directory whose base record has no Win32 name settles its name from a freed
/// extension record after pass 1, the same as a live one.
#[test]
fn a_freed_directory_settles_its_name_from_a_freed_extension_record() {
    let mut dir = new_record(30, 1, 0);
    set_record_flags(&mut dir, directory_flags());
    let offset = add_standard_information(&mut dir, ATTRIBUTES_OFFSET, 0);
    let offset = add_file_name_ex(
        &mut dir,
        offset,
        1,
        reference(ROOT_SEQUENCE, ROOT_RECORD),
        NtfsFileNamespace::Dos,
        "DIR~1",
        0,
    );
    finish_record(&mut dir, offset);
    delete_record(&mut dir);

    let mut dir_name = new_record(45, 1, reference(1, 30));
    let offset = add_file_name(&mut dir_name, ATTRIBUTES_OFFSET, "dir", 0);
    finish_record(&mut dir_name, offset);
    delete_record(&mut dir_name);

    let mut file = named(24, reference(1, 30), "a.txt", false);
    delete_record(&mut file);

    let (volume, data, mut bitmap) = parts_at(vec![
        (ROOT_RECORD, root_record()),
        (24, file),
        (30, dir),
        (45, dir_name),
    ]);
    free(&mut bitmap, &[24, 30, 45]);

    for chunk_records in 1..=46 {
        let mut scan =
            MftScan::from_parts(volume.clone(), data.clone(), bitmap.clone(), chunk_records)
                .unwrap();
        let mut cache = DeletedPathCache::new();
        let mut found = Vec::new();
        while let Some(chunk) = scan.next_chunk().unwrap() {
            found.extend(chunk.deleted_files().map(|file| {
                let name = file.best_name().unwrap();
                (
                    file.number(),
                    file.mft().resolve_deleted_path(&name, &mut cache),
                )
            }));
        }
        let path: PathBuf = [VOLUME_PATH, "dir", "a.txt"].iter().collect();
        let (_, resolved) = found
            .iter()
            .find(|(number, _)| *number == 24)
            .expect("file 24 among the deleted files");
        assert_eq!(resolved.marker, None, "chunks of {chunk_records}");
        assert_eq!(resolved.path, path, "chunks of {chunk_records}");
    }
}

/// The path through a directory the scan never kept (missing entirely, not just unnamed) ends at
/// a `Lost` marker, the same as a whole `Mft` would for a missing parent.
#[test]
fn a_missing_parent_directory_ends_the_deleted_walk_at_lost() {
    let mut file = named(24, reference(1, 30), "a.txt", false);
    delete_record(&mut file);
    let (volume, data, mut bitmap) = parts_at(vec![(ROOT_RECORD, root_record()), (24, file)]);
    free(&mut bitmap, &[24]);

    let mut scan = MftScan::from_parts(volume, data, bitmap, 4).unwrap();
    let mut cache = DeletedPathCache::new();
    let mut found = None;
    while let Some(chunk) = scan.next_chunk().unwrap() {
        for file in chunk.deleted_files() {
            let name = file.best_name().unwrap();
            found = Some(file.mft().resolve_deleted_path(&name, &mut cache));
        }
    }
    assert_eq!(found.unwrap().marker, Some(DeletedPathMarker::Lost(30)));
}

#[test]
fn a_run_cursor_reads_the_same_bytes_in_any_piece_size() {
    let disk: Vec<u8> = (0..16384u32).map(|index| (index * 7 % 251) as u8).collect();
    let runs = vec![
        DataRun::Data {
            offset: 4096,
            length: 3000,
        },
        DataRun::Sparse { length: 1000 },
        DataRun::Data {
            offset: 100,
            length: 2500,
        },
    ];
    let size = 6000;
    let mut expected = disk[4096..7096].to_vec();
    expected.extend([0; 1000]);
    expected.extend(&disk[100..2100]);

    let volume = test_volume();
    let whole = RunCursor::new(&volume, size, runs.clone())
        .unwrap()
        .read_all(&mut Cursor::new(&disk))
        .unwrap();
    assert_eq!(whole, expected);

    for piece in [1, 7, 512, 999, 1000, 1001, 4096, 7000] {
        let mut cursor = RunCursor::new(&volume, size, runs.clone()).unwrap();
        let mut reader = Cursor::new(&disk);
        let mut read: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; piece];
        loop {
            let count = cursor.read(&mut reader, &mut buf).unwrap();
            if count == 0 {
                break;
            }
            read.extend(&buf[..count]);
        }
        assert_eq!(read, expected, "pieces of {piece}");
    }
}

/// A record naming itself as its own base (corrupt: `base_reference` equals its own reference)
/// is never indexed as a directory, even though its own directory flag is set. `Mft::record`'s
/// `records()` gives it nothing (a base looking for its own base sees only `is_extension()`
/// records naming it, and this one names itself), and, on a whole `Mft`, a parent reference to
/// it is refused (`is_extension()` is true whatever the directory flag says), so indexing it
/// here would let a scan resolve a corrupt parent a whole `Mft` does not.
#[test]
fn a_record_naming_itself_as_its_base_is_never_indexed_as_a_directory() {
    let mut dir = new_record(30, 1, reference(1, 30));
    set_record_flags(&mut dir, directory_flags());
    let offset = add_standard_information(&mut dir, ATTRIBUTES_OFFSET, 0);
    let offset = add_file_name_ex(
        &mut dir,
        offset,
        1,
        reference(ROOT_SEQUENCE, ROOT_RECORD),
        NtfsFileNamespace::Win32,
        "dir",
        0,
    );
    finish_record(&mut dir, offset);

    let (volume, data, bitmap) = parts_at(vec![(ROOT_RECORD, root_record()), (30, dir)]);
    let scan = MftScan::from_parts(volume, data, bitmap, 4).unwrap();
    let indexed = scan
        .view
        .side()
        .and_then(|side| side.names())
        .and_then(|names| names.get(30));
    assert!(
        indexed.is_none(),
        "a record naming itself as its base must never be indexed as a directory"
    );
}

/// A run list shorter than `$MFT $DATA`'s declared size must produce an error during pass 1.
/// Treating a short read as a zero-filled tail would desynchronize the window from its reader.
/// The fixture declares two records (2048 bytes) but backs them with only 1024 bytes of runs,
/// so a single two-record chunk asks for more than the reader can supply.
#[test]
fn a_short_read_inside_a_chunk_is_an_error_not_a_zero_filled_tail() {
    let volume = test_volume();
    let value = MftValue::Runs(
        RunCursor::new(
            &volume,
            2 * RECORD_SIZE as u64,
            vec![DataRun::Data {
                offset: 0,
                length: RECORD_SIZE as u64,
            }],
        )
        .unwrap(),
    );
    let bitmap = vec![0u8; 1];
    let reader: Box<dyn ReadSeek> = Box::new(Cursor::new(vec![0u8; RECORD_SIZE]));

    let result = MftScan::from_value(volume, reader, value, bitmap, 4096);
    assert!(
        matches!(result, Err(NtfsReaderError::InvalidDataRun { .. })),
        "a run list shorter than $MFT $DATA's own declared size must be an error: {result:?}"
    );
}

/// A hostile `$MFT` can flag many records as directories without giving them a `$FILE_NAME`.
/// Removing those entries must remain roughly linear: `retain` with a repeated linear search
/// would make settling the directory index quadratic. The generous time bound allows debug
/// builds and machine noise while catching that change in complexity.
#[test]
fn many_unnamed_directories_settle_in_roughly_linear_time() {
    const UNNAMED_DIRECTORIES: u64 = 64000;
    let records: Vec<_> = (0..UNNAMED_DIRECTORIES)
        .map(|offset| {
            let mut record = new_record(FIRST_RECORD + offset, 1, 0);
            set_record_flags(&mut record, directory_flags());
            finish_record(&mut record, ATTRIBUTES_OFFSET);
            record
        })
        .collect();
    let (volume, data, bitmap) = raw_parts(records);

    let started = std::time::Instant::now();
    let scan = MftScan::from_parts(volume, data, bitmap, 4096).unwrap();
    let elapsed = started.elapsed();

    assert_eq!(
        scan.view.side().and_then(|side| side.names()).map(|names| {
            (0..UNNAMED_DIRECTORIES)
                .filter(|&offset| names.get(FIRST_RECORD + offset).is_some())
                .count()
        }),
        Some(0),
        "every nameless directory must still be dropped"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "settling {UNNAMED_DIRECTORIES} unnamed directories took {elapsed:?}: quadratic directory-index pruning?"
    );
}
