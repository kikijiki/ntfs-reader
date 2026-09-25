// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

// Consumer scenarios for the deleted-file API: each test acts out what someone using the crate
// does (per the README and examples) and asserts the outcome that person needs, not what the code
// happens to return. Written from the caller's side on purpose: two defects once passed a green
// suite because the tests asserted the implementation (a deleted file's `file_id()` that
// `record_by_id` rejected; a stream with lost extents that answered "free").
//
// Fixtures give a deleted record what NTFS leaves: in-use flag off, `$BITMAP` bit clear, sequence
// number one above its live value; every reference held elsewhere (parent reference, an extension
// record's base reference, a journal id) keeps the live sequence.

use std::io::{Cursor, Read};

use crate::api::*;
use crate::bitmap::{allocation_of, AllocationState, ClusterBitmap};
use crate::errors::NtfsReaderError;
use crate::file::NtfsFile;
use crate::file_info::FileInfo;
use crate::mft::test_records::*;
use crate::mft::Mft;
use crate::path::{DefaultPathCache, DeletedPathCache};
use crate::stream::{open, ExtentLocation, StreamExtent};
use crate::volume::Volume;

const ARCHIVE: u32 = NtfsFileNameFlags::Archive as u32;
const CS: u64 = CLUSTER_SIZE as u64;

/// A file with standard information, one name in the root and resident data, live with `sequence`.
fn file_record(number: u64, sequence: u16, name: &str) -> Vec<u8> {
    let mut record = new_record(number, sequence, 0);
    let mut offset = add_standard_information(&mut record, ATTRIBUTES_OFFSET, ARCHIVE);
    offset = add_file_name(&mut record, offset, name, ARCHIVE);
    offset = add_resident_attribute(&mut record, offset, NtfsAttributeType::Data, 3, "", b"data");
    finish_record(&mut record, offset);
    record
}

/// An extension record of the file whose base is `base_number` (live sequence `base_sequence`),
/// holding one more name.
fn extension_record(number: u64, sequence: u16, base: (u16, u64), name: &str) -> Vec<u8> {
    let mut record = new_record(number, sequence, reference(base.0, base.1));
    let offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, name, ARCHIVE);
    finish_record(&mut record, offset);
    record
}

/// What a delete does to a record's own bytes (see [`delete_record`]). The `$BITMAP` bit is the
/// `Mft` builder's business.
fn deleted(mut record: Vec<u8>) -> Vec<u8> {
    delete_record(&mut record);
    record
}

fn ids_of<'a>(files: impl Iterator<Item = NtfsFile<'a>>) -> Vec<(u64, FileId)> {
    files.map(|file| (file.number(), file.file_id())).collect()
}

/// The same volume twice: `(live, after the delete)`. Records 24 and 25 are plain files, 26 and 27
/// are a file and its extension record, 28 is a directory. In the second volume every one of them
/// was deleted.
fn before_and_after_a_delete() -> (Mft, Mft) {
    let volume = |gone: bool| {
        let mut directory = file_record(28, 9, "dir");
        set_record_flags(
            &mut directory,
            directory_flags() | NtfsFileFlags::InUse as u16,
        );
        let records = vec![
            file_record(24, 1, "a.txt"),
            file_record(25, 7, "b.txt"),
            file_record(26, 3, "c.txt"),
            extension_record(27, 5, (3, 26), "c-second-name"),
            directory,
        ];
        if gone {
            mft_with_freed(
                records.into_iter().map(deleted).collect(),
                &[24, 25, 26, 27, 28],
            )
        } else {
            mft_with(records)
        }
    };
    (volume(false), volume(true))
}

// ---- Story: list the deleted files, keep an id, look the file up again ----------------------------

#[test]
fn every_deleted_file_is_found_again_by_its_file_id() {
    let (_, mft) = before_and_after_a_delete();
    let listed: Vec<_> = mft.deleted_files().collect();
    assert_eq!(listed.len(), 4, "the deleted base records");

    for file in listed {
        let id = file.file_id();
        let found = mft.record_by_id(id).unwrap_or_else(|| {
            panic!(
                "record {}: its own file_id {id:?} finds nothing",
                file.number()
            )
        });
        assert_eq!(found.number(), file.number());
        assert_eq!(
            FileInfo::new(&found).name,
            FileInfo::new(&file).name,
            "the lookup gave another file"
        );
    }
}

#[test]
fn every_live_file_is_found_again_by_its_file_id() {
    let (mft, _) = before_and_after_a_delete();
    assert_eq!(mft.files().count(), 4, "the live base records");
    for file in mft.files() {
        let found = mft.record_by_id(file.file_id()).expect("a live file's id");
        assert_eq!(found.number(), file.number());
    }
}

#[test]
fn a_file_id_taken_before_the_delete_is_the_file_id_after_it() {
    let (before, after) = before_and_after_a_delete();
    let captured = ids_of(before.files());
    assert_eq!(captured.len(), 4, "the live base records");

    for (number, id) in captured {
        let gone = after.record(number).expect("the record");
        assert!(gone.is_deleted(), "record {number}");
        assert_eq!(gone.file_id(), id, "record {number}");
        assert_eq!(
            after.record_by_id(id).map(|found| found.number()),
            Some(number)
        );
    }
    // The extension record belongs to the same story: its id is the one it had while live.
    assert_eq!(
        before.record(27).unwrap().file_id(),
        after.record(27).unwrap().file_id()
    );
}

#[test]
fn reference_is_the_raw_header_value_and_says_a_record_was_freed() {
    let (before, after) = before_and_after_a_delete();
    for number in [24, 25, 26, 28] {
        let live = before.record(number).unwrap().reference();
        let gone = after.record(number).unwrap().reference();
        assert_eq!(
            gone,
            live + (1 << 48),
            "record {number}: freeing adds one to the sequence"
        );
    }
}

// The sequence number is 16 bits; NTFS is documented to skip 0 on wrap (unmeasured), so a record
// freed from 0xFFFF may carry 0 (plain wrap) or 1 (0 skipped). Either way, the file's id is
// 0xFFFF.
#[test]
fn a_file_id_survives_the_sequence_wrap() {
    for freed_sequence in [0u16, 1] {
        let live_id = FileId::from(reference(0xFFFF, 24));
        let mut record = file_record(24, freed_sequence, "wrapped.txt");
        mark_freed(&mut record);
        let mft = mft_with_freed(vec![record], &[24]);

        let listed: Vec<_> = mft.deleted_files().collect();
        assert_eq!(listed.len(), 1, "freed with sequence {freed_sequence}");
        assert_eq!(
            listed[0].file_id(),
            live_id,
            "freed with sequence {freed_sequence}"
        );
        let found = mft.record_by_id(live_id).unwrap_or_else(|| {
            panic!("the id of the live file finds nothing, freed with sequence {freed_sequence}")
        });
        assert_eq!(found.number(), 24);
    }
}

#[test]
fn a_freed_extension_record_is_attributed_across_the_sequence_wrap() {
    for freed_sequence in [0u16, 1] {
        let base = deleted_to(file_record(24, 0xFFFF, "wrapped.txt"), freed_sequence);
        let extension = extension_record(25, 3, (0xFFFF, 24), "second-name");
        let mft = mft_with_freed(vec![base, deleted(extension)], &[24, 25]);

        let file = mft.deleted_files().next().expect("the deleted file");
        let names: Vec<_> = file.names().map(|name| name.to_string()).collect();
        assert_eq!(
            names,
            ["wrapped.txt", "second-name"],
            "base freed with sequence {freed_sequence}"
        );
    }
}

/// `record` with its sequence number replaced and freed.
fn deleted_to(mut record: Vec<u8>, sequence: u16) -> Vec<u8> {
    write_u16(&mut record, 16, sequence);
    mark_freed(&mut record);
    record
}

// ---- Story: is this file deleted? -----------------------------------------------------------------

#[test]
fn a_file_says_whether_it_is_deleted_the_same_way_however_it_is_reached() {
    let (live, gone) = before_and_after_a_delete();
    assert_eq!(live.files().count(), 4, "the live base records");
    assert_eq!(gone.deleted_files().count(), 4, "the deleted base records");

    for file in live.files() {
        assert!(!file.is_deleted(), "live record {}", file.number());
        assert!(
            !FileInfo::new(&file).is_deleted,
            "live record {}",
            file.number()
        );
    }
    for file in gone.deleted_files() {
        assert!(file.is_deleted(), "record {}", file.number());
        assert!(FileInfo::new(&file).is_deleted, "record {}", file.number());
    }
    // The extension record of a deleted file is part of that file: the base decides.
    let extension = gone.record(27).unwrap();
    assert!(extension.is_extension());
    assert!(extension.is_deleted());
    assert!(FileInfo::new(&extension).is_deleted);
    // And of a live one.
    let extension = live.record(27).unwrap();
    assert!(!extension.is_deleted());
    assert!(!FileInfo::new(&extension).is_deleted);
}

#[test]
fn a_freed_extension_record_of_a_live_file_does_not_make_it_deleted() {
    // The file lives; one of its extension records was freed (a name was unlinked).
    let mft = mft_with_freed(
        vec![
            file_record(24, 3, "kept.txt"),
            deleted(extension_record(25, 5, (3, 24), "unlinked")),
        ],
        &[25],
    );
    let base = mft.record(24).unwrap();
    let extension = mft.record(25).unwrap();
    assert!(!extension.is_used());

    assert!(!base.is_deleted());
    assert!(!extension.is_deleted());
    assert!(!FileInfo::new(&base).is_deleted);
    assert!(!FileInfo::new(&extension).is_deleted);
    assert_eq!(mft.files().map(|f| f.number()).collect::<Vec<_>>(), [24]);
    assert_eq!(mft.deleted_files().count(), 0);
}

// A record whose in-use flag and `$BITMAP` bit disagree (caught between the two writes, or corrupt)
// is neither a live file nor a deleted one, and every way of asking agrees.
#[test]
fn a_record_whose_flag_and_bitmap_disagree_is_neither_live_nor_deleted() {
    // 24: flag off, bit set. 25: flag on, bit clear.
    let mut flag_off = file_record(24, 4, "flag-off.txt");
    mark_freed(&mut flag_off);
    let mft = mft_with_freed(vec![flag_off, file_record(25, 4, "bit-clear.txt")], &[25]);

    assert_eq!(mft.files().count(), 0);
    assert_eq!(mft.deleted_files().count(), 0);
    for number in [24, 25] {
        let file = mft.record(number).unwrap();
        assert!(!file.is_deleted(), "record {number}");
        assert!(!FileInfo::new(&file).is_deleted, "record {number}");
    }
}

// A file deleted while something holds it open is renamed under `$Extend\$Deleted` and stays in use
// until the last handle closes: it is still a live file.
#[test]
fn a_delete_pending_file_is_not_deleted_yet() {
    let extend = 11;
    let deleted_directory = 29;
    let mut extend_record = new_record(extend, 1, 0);
    set_record_flags(
        &mut extend_record,
        directory_flags() | NtfsFileFlags::InUse as u16,
    );
    let offset = add_file_name(&mut extend_record, ATTRIBUTES_OFFSET, "$Extend", ARCHIVE);
    finish_record(&mut extend_record, offset);
    let mut holder = new_record(deleted_directory, 1, 0);
    set_record_flags(&mut holder, directory_flags() | NtfsFileFlags::InUse as u16);
    let offset = add_file_name_ex(
        &mut holder,
        ATTRIBUTES_OFFSET,
        1,
        reference(1, extend),
        NtfsFileNamespace::Win32,
        "$Deleted",
        ARCHIVE,
    );
    finish_record(&mut holder, offset);
    let mut pending = new_record(30, 2, 0);
    let mut offset = add_standard_information(&mut pending, ATTRIBUTES_OFFSET, ARCHIVE);
    offset = add_file_name_ex(
        &mut pending,
        offset,
        1,
        reference(1, deleted_directory),
        NtfsFileNamespace::Win32,
        "1b3f9a",
        ARCHIVE,
    );
    finish_record(&mut pending, offset);
    let mft = mft_with_at(vec![
        (ROOT_RECORD, root_record()),
        (extend, extend_record),
        (deleted_directory, holder),
        (30, pending),
    ]);

    let file = mft.record(30).unwrap();
    assert!(file.is_used());
    assert!(!file.is_deleted());
    assert!(!FileInfo::new(&file).is_deleted);
    assert!(mft.files().any(|f| f.number() == 30));
    assert!(mft.deleted_files().all(|f| f.number() != 30));

    // Its parent is live, so `resolve_path` succeeds with the random name under `$Extend\$Deleted`;
    // the deleted walk stops at `$Deleted` and says the path is not complete.
    let name = file.best_name().unwrap();
    let path = |components: &[&str]| {
        components
            .iter()
            .fold(std::path::PathBuf::from(VOLUME_PATH), |path, c| {
                path.join(c)
            })
    };
    assert_eq!(
        mft.resolve_path(&name, &mut DefaultPathCache::new()),
        Some(path(&["$Extend", "$Deleted", "1b3f9a"]))
    );
    let walked = mft.resolve_deleted_path(&name, &mut DeletedPathCache::new());
    assert!(!walked.complete);
    assert_eq!(walked.path, path(&["<deleted>", "1b3f9a"]));
}

// An extension record whose own flag and `$BITMAP` bit disagree is not a freed record, so it is
// not part of the deleted file, though its base is deleted.
#[test]
fn an_extension_record_whose_flag_and_bitmap_disagree_is_not_deleted_with_its_base() {
    // 25: flag off, bit set.
    let mut extension = extension_record(25, 5, (3, 24), "half-freed");
    mark_freed(&mut extension);
    let mft = mft_with_freed(
        vec![deleted(file_record(24, 3, "gone.txt")), extension],
        &[24],
    );

    assert!(mft.record(24).unwrap().is_deleted());
    let extension = mft.record(25).unwrap();
    assert!(!extension.is_deleted());
    assert!(!FileInfo::new(&extension).is_deleted);
}

// One answer for all three: `is_deleted`, `deleted_files`, and `files` agree about every record
// on a volume with each kind of record.
#[test]
fn is_deleted_agrees_with_the_listings_for_every_record() {
    let mut flag_off = file_record(30, 4, "flag-off.txt");
    mark_freed(&mut flag_off);
    let mft = mft_with_freed(
        vec![
            file_record(24, 3, "live.txt"),
            deleted(file_record(25, 3, "gone.txt")),
            file_record(26, 3, "with-ext.txt"),
            deleted(extension_record(27, 5, (3, 26), "freed-ext")),
            deleted(file_record(28, 3, "gone-with-ext.txt")),
            deleted(extension_record(29, 5, (3, 28), "freed-ext-of-gone")),
            flag_off,
            file_record(31, 4, "bit-clear.txt"),
        ],
        &[25, 27, 28, 29, 31],
    );
    let listed_deleted: Vec<u64> = mft.deleted_files().map(|f| f.number()).collect();
    let listed_live: Vec<u64> = mft.files().map(|f| f.number()).collect();
    assert_eq!(listed_deleted, [25, 28]);
    assert_eq!(listed_live, [24, 26]);

    for number in FIRST_NORMAL_RECORD..mft.record_count() {
        let Some(file) = mft.record(number) else {
            continue;
        };
        // Extension records answer for the file they belong to.
        let base = file.base_number().unwrap_or(number);
        let expected = listed_deleted.contains(&base);
        assert_eq!(file.is_deleted(), expected, "record {number}");
        assert_eq!(FileInfo::new(&file).is_deleted, expected, "record {number}");
    }
}

// ---- Story: how much of the file is left ----------------------------------------------------------

/// A deleted file with an `$ATTRIBUTE_LIST` whose non-resident stream came back zeroed.
fn file_with_a_list(number: u64, sequence: u16) -> Vec<u8> {
    let mut record = new_record(number, sequence, 0);
    let mut offset = add_standard_information(&mut record, ATTRIBUTES_OFFSET, ARCHIVE);
    offset = add_file_name(&mut record, offset, "big.bin", ARCHIVE);
    let list = list_entry(NtfsAttributeType::Data, "", 0, number);
    offset = add_resident_attribute(
        &mut record,
        offset,
        NtfsAttributeType::AttributeList,
        4,
        "",
        &list,
    );
    offset = add_truncated_nonresident(&mut record, offset, NtfsAttributeType::Data, "");
    finish_record(&mut record, offset);
    record
}

#[test]
fn a_deleted_file_that_lost_where_its_data_was_says_so_next_to_its_size() {
    let mft = mft_with_freed(
        vec![
            deleted(file_with_a_list(24, 3)),
            file_with_a_list(25, 3),
            deleted(file_record(26, 3, "plain.txt")),
        ],
        &[24, 26],
    );
    let info = |number| FileInfo::new(&mft.record(number).unwrap());

    // The size says 0, and only `data_lost` tells that this is not a file that was empty.
    let lost = info(24);
    assert!(lost.is_deleted);
    assert_eq!(lost.size, 0);
    assert!(lost.data_lost);
    // A live file with a list, and a deleted file without one, lost nothing.
    assert!(!info(25).data_lost);
    let plain = info(26);
    assert!(plain.is_deleted);
    assert!(!plain.data_lost);
}

// ---- Story: read what is there -----------------------------------------------------------------

/// A base record 24 with one non-resident default stream of `size` bytes made of `runs`
/// (`Some(lcn)` for data, `None` for a hole, each `(clusters, lcn)`).
fn stream_record(number: u64, size: u64, runs: &[(u64, Option<u64>)]) -> Vec<u8> {
    stream_file(number, "stream.bin", size, runs)
}

/// [`stream_record`] with a name of its own.
fn stream_file(number: u64, name: &str, size: u64, runs: &[(u64, Option<u64>)]) -> Vec<u8> {
    let mut previous = 0i64;
    let deltas: Vec<_> = runs
        .iter()
        .map(|&(clusters, lcn)| {
            let delta = lcn.map(|lcn| {
                let delta = lcn as i64 - previous;
                previous = lcn as i64;
                delta
            });
            (clusters, delta)
        })
        .collect();
    let mut record = new_record(number, 1, 0);
    let mut offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, name, ARCHIVE);
    let clusters: u64 = runs.iter().map(|run| run.0).sum();
    offset = add_nonresident_data_runs(
        &mut record,
        offset,
        NtfsAttributeType::Data,
        "",
        0,
        clusters - 1,
        size,
        &encode_runs(&deltas),
    );
    offset = add_end_marker(&mut record, offset);
    finish_record(&mut record, offset);
    record
}

fn volume_of(clusters: u64, cluster_size: u64, path: &str) -> Volume {
    Volume::synthetic(
        path,
        cluster_size,
        clusters * cluster_size,
        RECORD_SIZE as u64,
        0,
    )
}

/// A bitmap of `volume` in which cluster `n` is allocated when `allocated(n)`.
fn bitmap_of(volume: &Volume, allocated: impl Fn(u64) -> bool) -> ClusterBitmap {
    let clusters = volume.volume_size() / volume.cluster_size();
    let mut bits = vec![0u8; clusters.div_ceil(8) as usize];
    for cluster in (0..clusters).filter(|&cluster| allocated(cluster)) {
        bits[(cluster / 8) as usize] |= 1 << (cluster % 8);
    }
    let size = bits.len() as u64;
    ClusterBitmap::read(volume, size, Cursor::new(bits)).expect("a bitmap")
}

/// Records of a volume of 64 clusters (path [`VOLUME_PATH`]), with `records` from 24 and the
/// `$BITMAP` bit of each record number in `freed` clear.
fn mft_of_64_clusters(records: Vec<Vec<u8>>, freed: &[u64]) -> Mft {
    let (_, data, mut bitmap) = raw_parts(records);
    for &number in freed {
        bitmap[number as usize / 8] &= !(1 << (number % 8));
    }
    build_from_parts(volume_of(64, CS, VOLUME_PATH), data, bitmap)
}

// A file whose only stored part is a hole, its second half's extent record lost: what a deleted
// file with a freed extension record looks like. Reading it gives the zeroes, then fails at the
// byte where the record was lost - the byte a caller needs to resume or report how much was
// recovered.
#[test]
fn a_read_that_reaches_a_lost_extent_reports_where_it_was_lost() {
    // Size 3 clusters, a hole of 1, and the extension record that had the rest is gone.
    let mft = mft_of_64_clusters(
        vec![deleted(stream_record(24, 3 * CS, &[(1, None)]))],
        &[24],
    );
    let file = mft.record(24).unwrap();
    let mut stream = file
        .open_stream(None)
        .expect("a stream without clusters to read opens without the volume");

    let mut recovered = Vec::new();
    let err = stream
        .read_to_end(&mut recovered)
        .expect_err("the lost part");

    // The documented recipe.
    let lost_at = err
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<NtfsReaderError>())
        .and_then(|error| match error {
            NtfsReaderError::StreamExtentMissing { offset } => Some(*offset),
            _ => None,
        })
        .unwrap_or_else(|| panic!("not a StreamExtentMissing: {err:?}"));
    assert_eq!(lost_at, CS);
    assert_eq!(
        recovered,
        vec![0u8; CS as usize],
        "the bytes before the loss"
    );
}

#[test]
fn a_stream_that_is_not_stored_in_clusters_needs_no_volume_to_open_and_read() {
    // A hole and an empty non-resident stream: nothing to fetch from the volume, so nothing to
    // open (opening a raw volume needs elevation; this path does not exist).
    let mft = mft_of_64_clusters(
        vec![
            stream_record(24, 2 * CS, &[(2, None)]),
            stream_record(25, 0, &[(1, None)]),
        ],
        &[],
    );
    let mut sparse = mft.record(24).unwrap().open_stream(None).expect("open");
    let mut bytes = Vec::new();
    sparse.read_to_end(&mut bytes).expect("read");
    assert_eq!(bytes, vec![0u8; 2 * CS as usize]);

    let mut empty = mft.record(25).unwrap().open_stream(None).expect("open");
    assert_eq!(empty.read_to_end(&mut bytes).expect("read"), 0);
}

// ---- Story: recover_file, on a volume with every kind of deleted file ------------------------------

/// The byte a synthetic volume holds at `offset`: different at every offset that matters.
fn image_byte(offset: u64) -> u8 {
    (offset.wrapping_mul(2_654_435_761) >> 13) as u8 ^ (offset >> 12) as u8
}

/// A volume of 64 clusters with its bytes, and its deleted files: 24 `free.bin` (3 clusters, free),
/// 25 `reused.bin` (2 clusters somebody else has now), 26 `mixed.bin` (2 free and 2 reused clusters),
/// 27 `big.bin` (lost its runs), 28 `missing.bin` (2 clusters of 4, the record with the rest is
/// gone), 29 `small.txt` (resident), 30 `docs` (a directory). Every one is deleted.
/// The bitmap has clusters 0..4 (metadata) and 20..24 (reused) allocated.
fn recovery_volume() -> (Mft, Vec<u8>, ClusterBitmap) {
    let image: Vec<u8> = (0..64 * CS).map(image_byte).collect();
    let mut docs = directory_record(30, 3, "docs");
    delete_record(&mut docs);
    let mft = mft_of_64_clusters(
        vec![
            deleted(stream_file(24, "free.bin", 3 * CS - 100, &[(3, Some(8))])),
            deleted(stream_file(25, "reused.bin", 2 * CS, &[(2, Some(20))])),
            deleted(stream_file(
                26,
                "mixed.bin",
                4 * CS,
                &[(2, Some(12)), (2, Some(20))],
            )),
            deleted(file_with_a_list(27, 3)),
            deleted(stream_file(28, "missing.bin", 4 * CS, &[(2, Some(30))])),
            deleted(file_record(29, 3, "small.txt")),
            docs,
        ],
        &[24, 25, 26, 27, 28, 29, 30],
    );
    let bitmap = bitmap_of(mft.volume(), |cluster| {
        cluster < 4 || (20..24).contains(&cluster)
    });
    (mft, image, bitmap)
}

/// What a person running `recover_file` on one deleted file sees and gets: the report, and the copy
/// that the example writes (nothing when it says there is nothing to recover).
struct Recovery {
    name: String,
    state: AllocationState,
    size: u64,
    in_free: u64,
    in_allocated: u64,
    missing: u64,
    parts: u64,
    data_lost: bool,
    refused: bool,
    copied: Vec<u8>,
    stopped_at: Option<u64>,
    /// The exit code of `recover_file`: 0 the whole stream was copied, 1 nothing was written, 2 a
    /// copy that cannot be trusted whole.
    exit: u8,
}

fn recover(file: &NtfsFile, bitmap: &ClusterBitmap, image: &[u8]) -> Recovery {
    let stream = file.open_stream(None).expect("open the stream");
    let allocation = stream
        .allocation(bitmap)
        .expect("the bitmap of this volume");
    let parts = allocation.in_free_clusters()
        + allocation.in_allocated_clusters()
        + allocation.resident()
        + allocation.sparse()
        + allocation.missing()
        + allocation.beyond_initialized()
        + allocation.outside_bitmap();
    let size = stream.size();
    // The example's rule: a lost stream, or one that is all reused or missing, has nothing to copy.
    let unusable =
        allocation.in_allocated_clusters() + allocation.missing() + allocation.outside_bitmap();
    let refused = stream.data_lost() || (size > 0 && unusable >= size);
    let (mut copied, mut stopped_at) = (Vec::new(), None);
    if !refused {
        let mut reader = open(file, None, Cursor::new(image)).expect("open over the image");
        if let Err(error) = reader.read_to_end(&mut copied) {
            stopped_at = match error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<NtfsReaderError>())
            {
                Some(NtfsReaderError::StreamExtentMissing { offset }) => Some(*offset),
                _ => panic!("the copy stopped for another reason: {error:?}"),
            };
        }
    }
    // The example's exit code: nothing written is 1; a copy is partial (2) when the read stopped
    // early, part of the stream is missing or has no place, or it holds other files' bytes.
    let exit = if refused {
        1
    } else if stopped_at.is_some()
        || (copied.len() as u64) < size
        || allocation.missing() > 0
        || allocation.in_allocated_clusters() > 0
        || allocation.outside_bitmap() > 0
    {
        2
    } else {
        0
    };
    Recovery {
        name: FileInfo::new(file).name,
        state: allocation.state(),
        size,
        in_free: allocation.in_free_clusters(),
        in_allocated: allocation.in_allocated_clusters(),
        missing: allocation.missing(),
        parts,
        data_lost: stream.data_lost(),
        refused,
        copied,
        stopped_at,
        exit,
    }
}

#[test]
fn recovering_each_kind_of_deleted_file_reports_what_a_user_can_trust_and_copies_what_is_there() {
    let (mft, image, bitmap) = recovery_volume();
    let files: Vec<_> = mft
        .deleted_files()
        .filter(|file| !file.is_directory())
        .collect();
    assert_eq!(files.len(), 6, "the deleted files of the volume");
    let recovered: Vec<Recovery> = files
        .iter()
        .map(|file| recover(file, &bitmap, &image))
        .collect();
    let by_name = |name: &str| {
        recovered
            .iter()
            .find(|recovery| recovery.name == name)
            .unwrap_or_else(|| panic!("no report for {name}"))
    };

    // Every report accounts for every byte: the parts add up to the size, whatever the shape.
    for recovery in &recovered {
        assert_eq!(recovery.parts, recovery.size, "{}", recovery.name);
    }
    // The exit code says what can be trusted: only a copy of free clusters and resident data is a
    // clean 0; one that holds other files' bytes, or stops early, is 2; nothing written is 1.
    let exits: Vec<_> = [
        "free.bin",
        "reused.bin",
        "mixed.bin",
        "big.bin",
        "missing.bin",
        "small.txt",
    ]
    .map(|name| (name, by_name(name).exit))
    .into();
    assert_eq!(
        exits,
        [
            ("free.bin", 0),
            ("reused.bin", 1),
            ("mixed.bin", 2),
            ("big.bin", 1),
            ("missing.bin", 2),
            ("small.txt", 0),
        ]
    );

    // Free clusters: the whole file is there, byte for byte, and the state says only "free".
    let free = by_name("free.bin");
    assert_eq!(free.state, AllocationState::Free);
    assert_eq!(
        (free.in_free, free.in_allocated, free.missing),
        (free.size, 0, 0)
    );
    assert!(!free.refused && free.stopped_at.is_none());
    assert_eq!(free.copied, image[8 * CS as usize..][..free.size as usize]);

    // Reused clusters: nothing of the file is left to copy.
    let reused = by_name("reused.bin");
    assert_eq!(reused.state, AllocationState::Allocated);
    assert_eq!(reused.in_allocated, reused.size);
    assert!(reused.refused && reused.copied.is_empty());

    // Half and half: partly allocated; the copy is complete in length (what it holds in the
    // reused clusters is somebody else's - the report says so).
    let mixed = by_name("mixed.bin");
    assert_eq!(mixed.state, AllocationState::PartlyAllocated);
    assert_eq!((mixed.in_free, mixed.in_allocated), (2 * CS, 2 * CS));
    assert!(!mixed.refused);
    assert_eq!(mixed.copied.len() as u64, mixed.size);

    // Runs wiped by the delete: size 0, and the report says why instead of "empty".
    let lost = by_name("big.bin");
    assert_eq!(lost.state, AllocationState::Lost);
    assert!(lost.data_lost && lost.refused && lost.size == 0);
    assert!(lost.copied.is_empty());

    // A lost extension record: incomplete, never "free", and the copy stops where the loss is,
    // which is the documented downcast.
    let missing = by_name("missing.bin");
    assert_eq!(missing.state, AllocationState::Incomplete);
    assert_eq!(missing.missing, 2 * CS);
    assert!(!missing.refused);
    assert_eq!(missing.stopped_at, Some(2 * CS));
    assert_eq!(missing.copied, image[30 * CS as usize..][..2 * CS as usize]);

    // Resident data is in the record: nothing to look up in the bitmap, and it copies.
    let small = by_name("small.txt");
    assert_eq!(small.state, AllocationState::NoStoredData);
    assert_eq!(small.copied, b"data");
}

// ---- Story: the allocation column of list_deleted ---------------------------------------------------

/// The column `list_deleted --allocation` prints, as the example writes it. The `match` has no
/// wildcard on purpose: a state added to the crate must be given a word here.
fn allocation_column(file: &NtfsFile, bitmap: &ClusterBitmap) -> String {
    if file.is_directory() {
        return "directory".into();
    }
    let stream = match file.open_stream(None) {
        Ok(stream) => stream,
        // No default stream at all: its record was freed and reused.
        Err(_) if file.stream_data_lost() => return "lost".into(),
        Err(error) => return format!("error: {error}"),
    };
    let allocation = match stream.allocation(bitmap) {
        Ok(allocation) => allocation,
        Err(error) => return format!("error: {error}"),
    };
    match allocation.state() {
        AllocationState::Free => "free",
        AllocationState::PartlyAllocated => "partly allocated",
        AllocationState::Allocated => "allocated",
        AllocationState::Lost => "lost",
        AllocationState::NoStoredData => "no stored data",
        AllocationState::Incomplete => "incomplete",
    }
    .into()
}

#[test]
fn the_allocation_column_of_list_deleted_names_every_shape_of_deleted_file() {
    let (mft, _, bitmap) = recovery_volume();
    let columns: Vec<(String, String)> = mft
        .deleted_files()
        .map(|file| (FileInfo::new(&file).name, allocation_column(&file, &bitmap)))
        .collect();
    assert_eq!(
        columns,
        [
            ("free.bin", "free"),
            ("reused.bin", "allocated"),
            ("mixed.bin", "partly allocated"),
            ("big.bin", "lost"),
            ("missing.bin", "incomplete"),
            ("small.txt", "no stored data"),
            ("docs", "directory"),
        ]
        .map(|(name, column)| (name.to_string(), column.to_string()))
    );

    // A bitmap of another volume is an error printed in the column, not a wrong word.
    let elsewhere = bitmap_of(&volume_of(64, CS, r"\\.\Z:"), |_| false);
    let free = mft.record(24).unwrap();
    assert!(allocation_column(&free, &elsewhere).starts_with("error:"));
}

// ---- Story: how much of the stream is in clusters somebody else has now ---------------------------

#[test]
fn a_stream_that_lost_an_extension_record_is_never_reported_as_free() {
    // Cluster 8 holds the first cluster of the file and is free; the second half lost its extent.
    let mft = mft_of_64_clusters(
        vec![deleted(stream_record(24, 2 * CS, &[(1, Some(8))]))],
        &[24],
    );
    let stream = mft.record(24).unwrap().open_stream(None).expect("open");
    let bitmap = bitmap_of(mft.volume(), |cluster| cluster != 8);

    let allocation = stream.allocation(&bitmap).expect("the same volume");
    assert_eq!(allocation.in_free_clusters(), CS);
    assert_eq!(allocation.missing(), CS);
    assert_eq!(allocation.state(), AllocationState::Incomplete);
}

#[test]
fn a_stream_whose_clusters_the_bitmap_does_not_cover_is_never_reported_as_allocated() {
    // The file says it is in cluster 70 of a volume of 64: junk, or another volume's bitmap.
    let extents = [StreamExtent {
        stream_offset: 0,
        length: CS,
        location: ExtentLocation::Volume { offset: 70 * CS },
    }];
    let bitmap = bitmap_of(&volume_of(64, CS, VOLUME_PATH), |_| true);
    let allocation = allocation_of(&extents, CS, CS, false, &bitmap);
    assert_eq!(allocation.outside_bitmap(), CS);
    assert_eq!(allocation.state(), AllocationState::Incomplete);
}

// Every combination of the seven kinds of bytes a stream can have, and the state a reader expects:
// the bitmap's verdict once every byte's place is known, "incomplete" as soon as one is not (a
// lost extent, or clusters the bitmap does not cover). Bytes in the record, holes, or never
// written have no cluster to judge and change nothing.
#[test]
fn every_combination_of_parts_has_the_state_a_reader_expects() {
    const KINDS: [&str; 7] = [
        "resident",
        "sparse",
        "beyond initialized",
        "free",
        "allocated",
        "missing",
        "outside",
    ];
    // 16 clusters: 0..8 free, 8..16 allocated.
    let bitmap = bitmap_of(&volume_of(16, CS, VOLUME_PATH), |cluster| cluster >= 8);

    for (combination, lost) in (0u32..1 << KINDS.len()).flat_map(|c| [(c, false), (c, true)]) {
        let has =
            |kind: &str| combination & 1 << KINDS.iter().position(|&k| k == kind).unwrap() != 0;
        let mut extents = Vec::new();
        // One cluster of the stream per kind.
        let push = |extents: &mut Vec<StreamExtent>, location| {
            let stream_offset = extents.len() as u64 * CS;
            extents.push(StreamExtent {
                stream_offset,
                length: CS,
                location,
            });
        };
        if has("resident") {
            push(&mut extents, ExtentLocation::Resident);
        }
        if has("sparse") {
            push(&mut extents, ExtentLocation::Sparse);
        }
        if has("free") {
            push(&mut extents, ExtentLocation::Volume { offset: 2 * CS });
        }
        if has("allocated") {
            push(&mut extents, ExtentLocation::Volume { offset: 10 * CS });
        }
        if has("missing") {
            push(&mut extents, ExtentLocation::Missing);
        }
        if has("outside") {
            push(&mut extents, ExtentLocation::Volume { offset: 40 * CS });
        }
        let initialized = extents.len() as u64 * CS;
        if has("beyond initialized") {
            // Never written: whatever the extent says, it reads as zeroes and is not judged.
            push(&mut extents, ExtentLocation::Volume { offset: 10 * CS });
        }
        let size = extents.len() as u64 * CS;

        let allocation = allocation_of(&extents, size, initialized, lost, &bitmap);
        let what = format!(
            "{:?}, data lost {lost}",
            KINDS.iter().filter(|kind| has(kind)).collect::<Vec<_>>()
        );
        // The fixture made what it was asked to.
        assert_eq!(allocation.resident() > 0, has("resident"), "{what}");
        assert_eq!(allocation.sparse() > 0, has("sparse"), "{what}");
        assert_eq!(
            allocation.beyond_initialized() > 0,
            has("beyond initialized"),
            "{what}"
        );
        assert_eq!(allocation.in_free_clusters() > 0, has("free"), "{what}");
        assert_eq!(
            allocation.in_allocated_clusters() > 0,
            has("allocated"),
            "{what}"
        );
        assert_eq!(allocation.missing() > 0, has("missing"), "{what}");
        assert_eq!(allocation.outside_bitmap() > 0, has("outside"), "{what}");

        // A part with no known place makes the answer unknown, then a stream that lost its data
        // says so, then the bitmap's verdict on what is stored in clusters.
        let expected = if has("missing") || has("outside") {
            AllocationState::Incomplete
        } else if lost {
            AllocationState::Lost
        } else if has("free") && has("allocated") {
            AllocationState::PartlyAllocated
        } else if has("free") {
            AllocationState::Free
        } else if has("allocated") {
            AllocationState::Allocated
        } else {
            AllocationState::NoStoredData
        };
        assert_eq!(allocation.state(), expected, "{what}");
    }
}

// The bitmap must be the stream's own volume's: another cluster size or volume would judge the
// wrong clusters and give a confident wrong answer, so it is an error.
#[test]
fn a_bitmap_of_another_cluster_size_is_refused() {
    let mft = mft_of_64_clusters(vec![stream_record(24, CS, &[(1, Some(8))])], &[]);
    let stream = mft.record(24).unwrap().open_stream(None).expect("open");
    let small_clusters = bitmap_of(&volume_of(512, 512, VOLUME_PATH), |_| true);

    match stream.allocation(&small_clusters) {
        Err(NtfsReaderError::InvalidClusterBitmap { details }) => {
            assert!(details.contains("cluster size"), "{details}")
        }
        other => panic!("a bitmap of another cluster size was accepted: {other:?}"),
    }
}

#[test]
fn a_bitmap_of_another_volume_is_refused() {
    let mft = mft_of_64_clusters(vec![stream_record(24, CS, &[(1, Some(8))])], &[]);
    let stream = mft.record(24).unwrap().open_stream(None).expect("open");
    let elsewhere = bitmap_of(&volume_of(64, CS, r"\\.\Z:"), |_| true);

    match stream.allocation(&elsewhere) {
        Err(NtfsReaderError::InvalidClusterBitmap { details }) => {
            assert!(details.contains("volume"), "{details}")
        }
        other => panic!("a bitmap of another volume was accepted: {other:?}"),
    }

    // The same volume, however its letter is cased.
    let same = bitmap_of(&volume_of(64, CS, r"\\.\t:"), |_| true);
    assert!(stream.allocation(&same).is_ok());
    let same = bitmap_of(&volume_of(64, CS, VOLUME_PATH), |_| true);
    assert!(stream.allocation(&same).is_ok());
}

// ---- The whole story of the examples --------------------------------------------------------------

// list_deleted: list deleted files, print name and size, say what's recoverable, resolve the
// path. A directory deleted first is still part of the path. One file has resident data, the
// other lives in clusters; the allocation column is the bitmap's word on each.
#[test]
fn listing_deleted_files_gives_names_sizes_paths_and_what_can_be_recovered() {
    let mut directory = new_record(24, 2, 0);
    set_record_flags(
        &mut directory,
        directory_flags() | NtfsFileFlags::InUse as u16,
    );
    let offset = add_file_name(&mut directory, ATTRIBUTES_OFFSET, "docs", ARCHIVE);
    finish_record(&mut directory, offset);

    let mut report = new_record(25, 4, 0);
    let mut offset = add_standard_information(&mut report, ATTRIBUTES_OFFSET, ARCHIVE);
    offset = add_file_name_ex(
        &mut report,
        offset,
        1,
        reference(2, 24),
        NtfsFileNamespace::Win32,
        "report.txt",
        ARCHIVE,
    );
    offset = add_resident_attribute(
        &mut report,
        offset,
        NtfsAttributeType::Data,
        3,
        "",
        b"quarterly",
    );
    finish_record(&mut report, offset);

    // A file in the same directory whose 2 clusters are free.
    let mut archive = new_record(26, 1, 0);
    let mut offset = add_file_name_ex(
        &mut archive,
        ATTRIBUTES_OFFSET,
        1,
        reference(2, 24),
        NtfsFileNamespace::Win32,
        "archive.bin",
        ARCHIVE,
    );
    offset = add_nonresident_data_runs(
        &mut archive,
        offset,
        NtfsAttributeType::Data,
        "",
        0,
        1,
        2 * CS,
        &encode_runs(&[(2, Some(10))]),
    );
    offset = add_end_marker(&mut archive, offset);
    finish_record(&mut archive, offset);

    let mft = mft_of_64_clusters(
        vec![
            deleted(directory),
            deleted(report),
            deleted(archive),
            file_record(27, 1, "live.txt"),
        ],
        &[24, 25, 26],
    );
    let bitmap = bitmap_of(mft.volume(), |cluster| cluster < 4);

    let mut cache = DeletedPathCache::new();
    let mut lines = Vec::new();
    for file in mft.deleted_files().filter(|file| !file.is_directory()) {
        let info = FileInfo::new(&file);
        assert!(info.is_deleted);
        let name = file.best_name().expect("a name");
        let path = mft.resolve_deleted_path(&name, &mut cache);
        assert!(path.complete);
        let mut stream = file.open_stream(None).expect("the stream opens");
        let state = stream.allocation(&bitmap).expect("the bitmap").state();
        let mut bytes = Vec::new();
        if state == AllocationState::NoStoredData {
            // Resident data is in the record and reads without the volume.
            stream.read_to_end(&mut bytes).expect("read");
        }
        assert!(!info.data_lost);
        lines.push((info.name, info.size, path.path, state, bytes));
    }
    let docs = std::path::Path::new(VOLUME_PATH).join("docs");
    assert_eq!(
        lines,
        [
            (
                "report.txt".to_string(),
                9,
                docs.join("report.txt"),
                AllocationState::NoStoredData,
                b"quarterly".to_vec()
            ),
            (
                "archive.bin".to_string(),
                2 * CS,
                docs.join("archive.bin"),
                AllocationState::Free,
                Vec::new()
            ),
        ]
    );
}

// An extension record names its file's base record. A record naming another extension record as
// its base (corrupt, or a bad reference) belongs to no file: the freed extension index must not
// turn an extension record into the base of a deleted file.
#[test]
fn an_extension_record_named_as_a_base_does_not_make_a_deleted_file() {
    // 24 is a real deleted file; 25 an extension record of 24; 26 names 25 as its base.
    let mft = mft_with_freed(
        vec![
            deleted(file_record(24, 3, "gone.txt")),
            deleted(extension_record(25, 5, (3, 24), "second-name")),
            deleted(extension_record(26, 7, (5, 25), "chained")),
        ],
        &[24, 25, 26],
    );
    assert_eq!(
        mft.deleted_files().map(|f| f.number()).collect::<Vec<_>>(),
        [24]
    );
    let names = |number| {
        mft.record(number)
            .unwrap()
            .names()
            .map(|name| name.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(24), ["gone.txt", "second-name"]);
    // The chained record is nobody's: no attributes, and not deleted.
    assert!(names(26).is_empty());
    assert!(!mft.record(26).unwrap().is_deleted());
    // Nor does it change what its named "base" reports.
    assert!(mft.record(25).unwrap().is_deleted());
}

// ---- Story: which stream lost its data ------------------------------------------------------------
//
// NTFS zeroes the sizes and runs of the non-resident streams of a deleted file that had an
// `$ATTRIBUTE_LIST` and leaves its resident streams alone. What it writes for a truncated attribute
// is `highest_vcn` -1, sizes 0 and a run list that is the terminator only (`add_truncated_nonresident`).

/// A file with an `$ATTRIBUTE_LIST`, a name, a default stream and an alternate stream `ads`, each
/// resident (`Some(bytes)`) or truncated (`None`).
fn file_with_streams(number: u64, default: Option<&[u8]>, ads: Option<&[u8]>) -> Vec<u8> {
    let mut record = new_record(number, 3, 0);
    let mut offset = add_standard_information(&mut record, ATTRIBUTES_OFFSET, ARCHIVE);
    offset = add_file_name(&mut record, offset, "streams.bin", ARCHIVE);
    let list = list_entry(NtfsAttributeType::Data, "", 0, number);
    offset = add_resident_attribute(
        &mut record,
        offset,
        NtfsAttributeType::AttributeList,
        4,
        "",
        &list,
    );
    for (id, name, content) in [(5, "", default), (6, "ads", ads)] {
        offset = match content {
            Some(bytes) => add_resident_attribute(
                &mut record,
                offset,
                NtfsAttributeType::Data,
                id,
                name,
                bytes,
            ),
            None => add_truncated_nonresident(&mut record, offset, NtfsAttributeType::Data, name),
        };
    }
    finish_record(&mut record, offset);
    record
}

/// The `Mft` of record 24 alone, on a volume of 64 clusters, `deleted` or live.
fn mft_of_a_file(record: Vec<u8>, deleted_file: bool) -> Mft {
    if deleted_file {
        mft_of_64_clusters(vec![deleted(record)], &[24])
    } else {
        mft_of_64_clusters(vec![record], &[])
    }
}

#[test]
fn a_stream_whose_runs_were_zeroed_says_it_lost_its_data() {
    let mft = mft_of_a_file(file_with_streams(24, None, Some(b"note")), true);
    let file = mft.record(24).unwrap();
    let bitmap = bitmap_of(mft.volume(), |_| true);

    // What a user sees of the default stream: it opens, is empty, and is marked as having lost
    // its data.
    let mut stream = file.open_stream(None).expect("a truncated stream opens");
    assert_eq!(stream.size(), 0);
    assert!(stream.extents().is_empty());
    assert!(stream.data_lost());
    assert_eq!(stream.read(&mut [0u8; 16]).unwrap(), 0);
    let allocation = stream.allocation(&bitmap).unwrap();
    assert!(allocation.data_lost());
    assert_eq!(allocation.state(), AllocationState::Lost);

    // The resident stream of the same file is intact and is not reported lost.
    let mut ads = file.open_stream(Some("ads".as_ref())).unwrap();
    assert!(!ads.data_lost());
    let mut bytes = Vec::new();
    ads.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"note");
    assert_eq!(
        ads.allocation(&bitmap).unwrap().state(),
        AllocationState::NoStoredData
    );

    // The same fact is available per stream, in data_streams(), and aggregated on the whole file.
    let streams: Vec<_> = file
        .data_streams()
        .map(|stream| (stream.name, stream.size, stream.data_lost))
        .collect();
    assert_eq!(streams, [(None, 0, true), (Some("ads".into()), 4, false)]);
    assert!(file.stream_data_lost());
    assert!(FileInfo::new(&file).data_lost);
}

#[test]
fn a_resident_default_stream_of_a_file_that_lost_other_data_is_not_lost() {
    let mft = mft_of_a_file(file_with_streams(24, Some(b"still here"), None), true);
    let file = mft.record(24).unwrap();
    let bitmap = bitmap_of(mft.volume(), |_| true);

    let info = FileInfo::new(&file);
    assert_eq!(info.size, 10);
    assert!(!info.data_lost, "the default stream is intact");
    // The file lost something: the alternate stream.
    assert!(file.stream_data_lost());
    let streams: Vec<_> = file
        .data_streams()
        .map(|stream| (stream.name, stream.data_lost))
        .collect();
    assert_eq!(streams, [(None, false), (Some("ads".into()), true)]);

    let default = file.open_stream(None).unwrap();
    assert!(!default.data_lost());
    assert_eq!(
        default.allocation(&bitmap).unwrap().state(),
        AllocationState::NoStoredData
    );
    let ads = file.open_stream(Some("ads".as_ref())).unwrap();
    assert!(ads.data_lost());
    assert_eq!(
        ads.allocation(&bitmap).unwrap().state(),
        AllocationState::Lost
    );
}

// An empty file has not lost data: without an attribute list, or while live, a non-resident
// stream of size 0 is genuinely empty.
#[test]
fn an_empty_stream_is_not_lost() {
    for (what, deleted_file, list) in [
        ("live, with a list", false, true),
        ("deleted, no list", true, false),
        ("live, no list", false, false),
    ] {
        let mut record = new_record(24, 3, 0);
        let mut offset = add_standard_information(&mut record, ATTRIBUTES_OFFSET, ARCHIVE);
        offset = add_file_name(&mut record, offset, "empty.bin", ARCHIVE);
        if list {
            let entry = list_entry(NtfsAttributeType::Data, "", 0, 24);
            offset = add_resident_attribute(
                &mut record,
                offset,
                NtfsAttributeType::AttributeList,
                4,
                "",
                &entry,
            );
        }
        offset = add_truncated_nonresident(&mut record, offset, NtfsAttributeType::Data, "");
        finish_record(&mut record, offset);
        let mft = mft_of_a_file(record, deleted_file);
        let file = mft.record(24).unwrap();
        let stream = file.open_stream(None).unwrap();
        assert!(!stream.data_lost(), "{what}");
        let bitmap = bitmap_of(mft.volume(), |_| true);
        assert_eq!(
            stream.allocation(&bitmap).unwrap().state(),
            AllocationState::NoStoredData,
            "{what}"
        );
        assert!(!FileInfo::new(&file).data_lost, "{what}");
        assert!(
            file.data_streams().all(|stream| !stream.data_lost),
            "{what}"
        );
    }
}

// ---- Story: the children of a directory, deleted or not ---------------------------------------------

/// A directory record, live with `sequence`, named `name` in the root.
fn directory_record(number: u64, sequence: u16, name: &str) -> Vec<u8> {
    let mut record = new_record(number, sequence, 0);
    set_record_flags(&mut record, directory_flags() | NtfsFileFlags::InUse as u16);
    let offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, name, ARCHIVE);
    finish_record(&mut record, offset);
    record
}

/// A file named `name` in the directory `parent` (a reference with the directory's live sequence).
fn file_in(number: u64, sequence: u16, parent: u64, name: &str) -> Vec<u8> {
    let mut record = new_record(number, sequence, 0);
    let mut offset = add_standard_information(&mut record, ATTRIBUTES_OFFSET, ARCHIVE);
    offset = add_file_name_ex(
        &mut record,
        offset,
        1,
        parent,
        NtfsFileNamespace::Win32,
        name,
        ARCHIVE,
    );
    finish_record(&mut record, offset);
    record
}

// A directory does not list its children: they are found by scanning for names whose parent is
// the directory. A name's parent is an id (record number plus the sequence the directory had
// while live) - what `file_id()` gives for a directory, live or deleted. `parent_reference` does
// not equal `reference()` of a freed directory: that is one higher.
#[test]
fn the_children_of_a_deleted_directory_are_found_by_the_id_of_the_directory() {
    let mft = mft_with_freed(
        vec![
            directory_record(24, 5, "docs"),
            deleted(directory_record(25, 7, "old")),
            deleted(file_in(26, 2, reference(7, 25), "a.txt")),
            deleted(file_in(27, 2, reference(7, 25), "b.txt")),
            file_in(28, 2, reference(5, 24), "live.txt"),
            deleted(file_in(29, 2, reference(5, 24), "gone.txt")),
        ],
        &[25, 26, 27, 29],
    );
    let children_of = |number: u64| {
        let directory = mft.record(number).unwrap();
        let mut children: Vec<String> = mft
            .files()
            .chain(mft.deleted_files())
            .filter(|file| {
                file.names()
                    .any(|name| name.parent_id() == directory.file_id())
            })
            .map(|file| file.best_name().unwrap().to_string())
            .collect();
        children.sort();
        children
    };
    assert_eq!(children_of(25), ["a.txt", "b.txt"], "a deleted directory");
    assert_eq!(
        children_of(24),
        ["gone.txt", "live.txt"],
        "a live directory"
    );

    // The stored parent reference of a name is not the `reference()` of a freed directory.
    let name = mft.record(26).unwrap().names().next().unwrap();
    assert_ne!(name.parent_reference(), mft.record(25).unwrap().reference());
    assert_eq!(name.parent_id(), mft.record(25).unwrap().file_id());
}

// ---- Story: a monitor holds a snapshot from before the delete ---------------------------------------

// monitor_journal / watch_deletes: a journal record says a file was deleted, with its file id,
// parent id, and name. The program keeps an `Mft` snapshot from before and loads another after.
// The later snapshot gives the path while the directory is still there, and gives what it can
// once the directory record is reused by another directory (the parent id then names nothing,
// and the path stops at a marker). The earlier snapshot still knows the whole path.
#[test]
fn a_journal_delete_is_resolved_from_the_snapshot_before_it_when_the_directory_is_gone_from_the_one_after(
) {
    let docs = directory_record(24, 2, "docs");
    let report = file_in(25, 4, reference(2, 24), "report.txt");
    let before = mft_with(vec![docs.clone(), report.clone()]);
    let expected_path = std::path::Path::new(VOLUME_PATH)
        .join("docs")
        .join("report.txt");

    // What the journal's FILE_DELETE record carries.
    let file_id = before.record(25).expect("the file").file_id();
    let parent_id = before.record(24).expect("the directory").file_id();
    assert_eq!(parent_id, FileId::from(reference(2, 24)));

    // After, with the directory deleted and its record still free.
    let plain = mft_with_freed(
        vec![deleted(docs.clone()), deleted(report.clone())],
        &[24, 25],
    );
    let parent = plain
        .record_by_id(parent_id)
        .expect("the deleted directory");
    assert!(parent.is_deleted());
    let gone = plain.record_by_id(file_id).expect("the deleted file");
    assert!(gone.is_deleted());
    let name = gone.best_name().expect("a name");
    let resolved = plain.resolve_deleted_path(&name, &mut DeletedPathCache::new());
    assert!(resolved.complete);
    assert_eq!(resolved.path, expected_path);

    // After, with the directory record taken by another directory (same sequence as the freed one:
    // reuse does not bump it again).
    let reused = mft_with_freed(
        vec![directory_record(24, 3, "other"), deleted(report)],
        &[25],
    );
    assert!(
        reused.record_by_id(parent_id).is_none(),
        "the id of the directory names another directory now"
    );
    let gone = reused.record_by_id(file_id).expect("the deleted file");
    let name = gone.best_name().expect("a name");
    let resolved = reused.resolve_deleted_path(&name, &mut DeletedPathCache::new());
    assert!(!resolved.complete);
    assert_eq!(
        resolved.path,
        std::path::Path::new(VOLUME_PATH)
            .join("<lost 24>")
            .join("report.txt")
    );
    assert_eq!(FileInfo::new(&gone).path, None);

    // The earlier snapshot has both, live, and the whole path.
    let directory = before.record_by_id(parent_id).expect("the directory then");
    assert_eq!(FileInfo::new(&directory).name, "docs");
    let file = before.record_by_id(file_id).expect("the file then");
    assert!(!file.is_deleted());
    let name = file.best_name().expect("a name");
    assert_eq!(
        before.resolve_path(&name, &mut DefaultPathCache::new()),
        Some(expected_path)
    );
}

// ---- Story: a default stream that cannot be found is not an empty file ------------------------------

/// The base record of a file whose `$FILE_NAME` and default `$DATA` live in an extension record:
/// only the standard information and the attribute list.
fn list_only_base(number: u64, sequence: u16, directory: bool) -> Vec<u8> {
    let mut record = new_record(number, sequence, 0);
    if directory {
        set_record_flags(&mut record, directory_flags() | NtfsFileFlags::InUse as u16);
    }
    let mut offset = add_standard_information(&mut record, ATTRIBUTES_OFFSET, ARCHIVE);
    let list = list_entry(NtfsAttributeType::Data, "", 0, number + 1);
    offset = add_resident_attribute(
        &mut record,
        offset,
        NtfsAttributeType::AttributeList,
        4,
        "",
        &list,
    );
    finish_record(&mut record, offset);
    record
}

// The extension record holding the name and default stream was freed with the file, then taken
// by a new file, so the deleted file shows size 0: "unknown," never "an empty file you can
// trust".
#[test]
fn a_deleted_file_whose_data_record_was_reused_is_not_an_empty_file() {
    let mft = mft_with_freed(
        vec![
            deleted(list_only_base(24, 3, false)),
            file_record(25, 6, "reused.txt"),
        ],
        &[24],
    );
    let file = mft.record(24).unwrap();
    assert!(file.is_deleted());
    assert!(file.stream_data_lost());
    let info = FileInfo::new(&file);
    assert_eq!(info.size, 0);
    assert!(
        info.data_lost,
        "size 0 must not read as a file that was empty"
    );
    // The listing a user gets shows the same: the file is there, its data is not.
    assert!(mft.deleted_files().any(|f| f.number() == 24));
}

#[test]
fn a_deleted_file_with_nothing_lost_still_says_so() {
    let mft = mft_with_freed(
        vec![
            // No list, no data attribute at all: an empty file that really was empty.
            deleted({
                let mut record = new_record(24, 3, 0);
                let offset = add_standard_information(&mut record, ATTRIBUTES_OFFSET, ARCHIVE);
                finish_record(&mut record, offset);
                record
            }),
            // A list, but the default stream was seen and is resident: intact.
            deleted(file_with_streams(25, Some(b"note"), None)),
            // A directory has no data to lose.
            deleted(list_only_base(26, 3, true)),
        ],
        &[24, 25, 26],
    );
    for number in [24, 25, 26] {
        let file = mft.record(number).unwrap();
        assert!(file.is_deleted(), "record {number}");
        assert!(!FileInfo::new(&file).data_lost, "record {number}");
    }
}

// ---- Story: a freed extension record of an earlier file is nobody's ---------------------------------

// File 24 (live sequence 3) had extension record 25 holding a name. Deleting it freed 24 at 4
// and 25 at 6; both keep the references held elsewhere, so the extension's base reference stays
// (3, 24). Record 24 was then reused by a new file, again starting at sequence 4. The old
// extension stays freed, naming a gone incarnation of 24, so whatever 24 is now, the extension
// belongs to no file: not deleted, no records, no names.
fn stale_extension() -> Vec<u8> {
    deleted(extension_record(25, 5, (3, 24), "old-second-name"))
}

#[test]
fn a_stale_extension_record_does_not_show_the_new_file_that_reused_its_base() {
    let mft = mft_with_freed(
        vec![file_record(24, 4, "new.txt"), stale_extension()],
        &[25],
    );
    let stale = mft.record(25).unwrap();
    assert!(!stale.is_deleted());
    assert_eq!(stale.records().count(), 0);
    assert_eq!(stale.names().count(), 0, "the new file's name is not its");
    // The new file is untouched.
    let new = mft.record(24).unwrap();
    assert_eq!(new.records().count(), 1);
    assert_eq!(new.names().count(), 1);
}

#[test]
fn a_stale_extension_record_is_not_deleted_when_the_reused_base_is_deleted_again() {
    // 24 was freed a second time: sequence 5. The old extension record names sequence 3, which is
    // the first incarnation, not this one.
    let mft = mft_with_freed(
        vec![deleted(file_record(24, 4, "new.txt")), stale_extension()],
        &[24, 25],
    );
    let stale = mft.record(25).unwrap();
    assert!(!stale.is_deleted());
    assert_eq!(stale.records().count(), 0);
    assert_eq!(stale.names().count(), 0);
    // The deleted file does not get it either.
    let gone = mft.record(24).unwrap();
    assert!(gone.is_deleted());
    assert_eq!(gone.records().count(), 1);
    assert_eq!(gone.names().count(), 1);
}

#[test]
fn an_extension_record_of_the_incarnation_that_was_deleted_is_part_of_that_file() {
    // The healthy case next to the two above: base freed at 4, extension naming (3, 24).
    let mft = mft_with_freed(
        vec![deleted(file_record(24, 3, "gone.txt")), stale_extension()],
        &[24, 25],
    );
    let extension = mft.record(25).unwrap();
    assert!(extension.is_deleted());
    assert_eq!(extension.records().count(), 2);
    assert_eq!(extension.names().count(), 2);
    assert_eq!(mft.record(24).unwrap().records().count(), 2);
}

// An extension record that names another extension record as its base belongs to no file, freed or
// live: there is no base to decide.
#[test]
fn an_extension_record_whose_base_is_an_extension_record_belongs_to_no_file() {
    // Freed: E1 (25) is a freed extension of the freed base 24, E2 (26) is freed and names E1.
    let freed = mft_with_freed(
        vec![
            deleted(file_record(24, 3, "gone.txt")),
            deleted(extension_record(25, 5, (3, 24), "first")),
            deleted(extension_record(26, 7, (5, 25), "second")),
        ],
        &[24, 25, 26],
    );
    let second = freed.record(26).unwrap();
    assert!(!second.is_deleted());
    assert_eq!(second.records().count(), 0);
    assert_eq!(freed.record(25).unwrap().records().count(), 2, "24 and 25");

    // Live: the same shape, all in use. E2 names E1, which is not a file.
    let live = mft_with(vec![
        file_record(24, 3, "kept.txt"),
        extension_record(25, 5, (3, 24), "first"),
        extension_record(26, 7, (5, 25), "second"),
    ]);
    let second = live.record(26).unwrap();
    assert!(!second.is_deleted());
    assert_eq!(second.records().count(), 0);
    assert_eq!(second.names().count(), 0);
    assert_eq!(live.record(24).unwrap().records().count(), 2);
}

// A deleted file whose default stream cannot be found at all (its extension record was freed and
// reused): `open_stream` has nothing to open, and the examples must say "data lost," not "no such
// data stream" or print `0 bytes` as if the file had been empty.
#[test]
fn a_deleted_file_whose_data_record_was_reused_is_reported_as_lost_by_the_examples() {
    let mft = mft_of_64_clusters(
        vec![
            deleted(list_only_base(24, 3, false)),
            file_record(25, 6, "reused.txt"),
        ],
        &[24],
    );
    let bitmap = bitmap_of(mft.volume(), |_| false);
    let file = mft.record(24).unwrap();

    // list_deleted: the size column has "(data lost)" and the allocation column "lost".
    let info = FileInfo::new(&file);
    assert_eq!((info.size, info.data_lost), (0, true));
    assert_eq!(allocation_column(&file, &bitmap), "lost");
    // recover_file: the open fails, and the file's data is what says why.
    assert!(file.open_stream(None).is_err());
    assert!(
        info.data_lost,
        "recover_file reports data lost, not the open error"
    );
}

// ---- Story: reading a fragmented file does not read the rest of the volume ----------------------------

/// A volume reader that counts what it hands out, `(reads, bytes)`, where the test can see it.
struct Counting<'i> {
    inner: Cursor<&'i [u8]>,
    counts: std::rc::Rc<std::cell::Cell<(u64, u64)>>,
}

impl Read for Counting<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let count = self.inner.read(buf)?;
        let (reads, bytes) = self.counts.get();
        self.counts.set((reads + 1, bytes + count as u64));
        Ok(count)
    }
}

impl std::io::Seek for Counting<'_> {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(position)
    }
}

/// Reads the default stream of record 24 (made of `runs` on a volume of 4200 clusters whose bytes are
/// `image_byte`) in pieces of `piece` bytes, checks every byte, and returns `(reads, bytes)` the
/// volume handed out.
fn read_counting(runs: &[(u64, Option<u64>)], piece: usize) -> (u64, u64, u64) {
    let image: Vec<u8> = (0..4200 * CS).map(image_byte).collect();
    let size: u64 = runs.iter().map(|run| run.0).sum::<u64>() * CS;
    let (_, data, bitmap) = raw_parts(vec![stream_record(24, size, runs)]);
    let mft = build_from_parts(volume_of(4200, CS, VOLUME_PATH), data, bitmap);
    let file = mft.record(24).unwrap();
    let counts = std::rc::Rc::new(std::cell::Cell::new((0, 0)));
    let counting = Counting {
        inner: Cursor::new(&image),
        counts: counts.clone(),
    };
    let mut reader = open(&file, None, counting).expect("open");
    let mut expected = Vec::new();
    for &(clusters, lcn) in runs {
        let start = lcn.unwrap() * CS;
        expected.extend_from_slice(&image[start as usize..(start + clusters * CS) as usize]);
    }
    let mut got = Vec::new();
    let mut buffer = vec![0u8; piece];
    loop {
        let count = reader.read(&mut buffer).expect("read");
        if count == 0 {
            break;
        }
        got.extend_from_slice(&buffer[..count]);
    }
    assert!(got == expected, "the bytes read are not the file's");
    let (reads, bytes) = counts.get();
    (reads, bytes, size)
}

// A file in many small fragments: each piece is read once, not 64 KiB or more around it. Reading
// ahead pays only where the next piece is where this one ends.
#[test]
fn a_fragmented_file_is_read_from_the_volume_for_about_its_own_size() {
    // 200 one-cluster fragments 80 KiB apart: nothing is contiguous, and no read lands in the span
    // an earlier one read ahead.
    let runs: Vec<_> = (0..200).map(|index| (1, Some(4 + 20 * index))).collect();
    let (reads, bytes, size) = read_counting(&runs, 4096);
    assert!(
        bytes <= 2 * size,
        "read {bytes} bytes off the volume for a file of {size} ({reads} reads)"
    );
}

#[test]
fn a_contiguous_file_is_still_read_ahead_in_large_spans() {
    // 100 clusters (400 KiB) in one run, read in 8 KiB pieces: a read of 64 KiB for every eight
    // pieces, not one per piece (50 of them).
    let (reads, bytes, size) = read_counting(&[(100, Some(8))], 8192);
    assert!(reads <= 7, "{reads} reads of the volume for {size} bytes");
    assert!(bytes <= 2 * size, "{bytes} bytes for {size}");
}

#[test]
fn the_next_piece_is_read_ahead_only_when_it_continues_where_this_one_ends() {
    // Two runs of 4 clusters: the second right after the first, then the second far away.
    let (reads_together, _, _) = read_counting(&[(4, Some(10)), (4, Some(14))], 4096);
    assert_eq!(reads_together, 1, "the pieces are one span on the volume");
    let (reads_apart, bytes, size) = read_counting(&[(4, Some(10)), (4, Some(40))], 4096);
    assert_eq!(reads_apart, 2, "two spans");
    assert_eq!(bytes, size, "and nothing but the file is read");
}

// ---- Story: how much memory does the Mft hold -----------------------------------------------------

// `size_in_memory` is what a caller budgets with: records, `$BITMAP`, and the two extension
// indexes (live and freed, 16 bytes an entry) that live in the `Mft`.
#[test]
fn size_in_memory_counts_the_records_the_bitmap_and_the_extension_indexes() {
    let records = vec![
        file_record(24, 3, "live.txt"),
        extension_record(25, 5, (3, 24), "live-second"),
        deleted(file_record(26, 3, "gone.txt")),
        deleted(extension_record(27, 5, (3, 26), "gone-second")),
    ];
    let (volume, data, mut bitmap) = raw_parts(records);
    for number in [26u64, 27] {
        bitmap[number as usize / 8] &= !(1 << (number % 8));
    }
    // One live and one freed entry, a pair of record numbers each.
    let expected = data.len() + bitmap.len() + 2 * 2 * std::mem::size_of::<u64>();
    let mft = build_from_parts(volume, data, bitmap);
    assert_eq!(mft.size_in_memory(), expected);
}

// ---- Story: threads --------------------------------------------------------------------------------

// One `Mft` can be shared by threads, and so can what borrows from it; a path cache needs `&mut`,
// so each thread keeps its own. This test pins what the guides claim.
#[test]
fn what_the_guides_say_can_be_shared_between_threads_can() {
    fn shared<T: Send + Sync>() {}
    shared::<Mft>();
    shared::<NtfsFile<'static>>();
    shared::<crate::StreamReader>();
    shared::<ClusterBitmap>();
    shared::<DeletedPathCache>();
    shared::<DefaultPathCache>();
    shared::<crate::DeletedPath>();
}
