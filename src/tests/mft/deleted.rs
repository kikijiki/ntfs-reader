// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

// Unit tests of the deleted-file rules: the reference rule, the freed extension index,
// `NtfsFile::records` for a freed base, `Mft::deleted_files`, `Mft::record_by_id`. Rules measured
// on a real volume: a freed record keeps its bytes except the in-use flag, and its sequence is one
// higher than when live. Fixtures give a freed record the sequence it has AFTER the delete, and a
// reference to it the sequence it had BEFORE.

use crate::api::*;
use crate::file::NtfsFile;
use crate::file_info::FileInfo;
use crate::mft::test_records::*;
use crate::mft::Mft;
use crate::path::{CachedPath, DefaultPathCache, PathCache};

const ARCHIVE: u32 = NtfsFileNameFlags::Archive as u32;

/// A base record with standard information, one name and resident data. `sequence` is what it is
/// stored with.
fn base(number: u64, sequence: u16, name: &str) -> Vec<u8> {
    let mut record = new_record(number, sequence, 0);
    let mut offset = add_standard_information(&mut record, ATTRIBUTES_OFFSET, ARCHIVE);
    offset = add_file_name(&mut record, offset, name, ARCHIVE);
    offset = add_resident_attribute(&mut record, offset, NtfsAttributeType::Data, 3, "", b"data");
    finish_record(&mut record, offset);
    record
}

/// An extension record that holds one more name of the file whose base reference is
/// `base_reference`.
fn extension(number: u64, sequence: u16, base_reference: u64, name: &str) -> Vec<u8> {
    let mut record = new_record(number, sequence, base_reference);
    let offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, name, ARCHIVE);
    finish_record(&mut record, offset);
    record
}

/// An extension record with no attributes: what an unlink leaves of a record that held one name.
fn empty_extension(number: u64, sequence: u16, base_reference: u64) -> Vec<u8> {
    let mut record = new_record(number, sequence, base_reference);
    let offset = add_end_marker(&mut record, ATTRIBUTES_OFFSET);
    finish_record(&mut record, offset);
    record
}

/// A base record with only an `$ATTRIBUTE_LIST` (and, if given, standard information): the shape
/// of a file whose names and data live in extension records.
fn base_with_list(number: u64, sequence: u16, standard_information: bool) -> Vec<u8> {
    let mut record = new_record(number, sequence, 0);
    let mut offset = ATTRIBUTES_OFFSET;
    if standard_information {
        offset = add_standard_information(&mut record, offset, ARCHIVE);
    }
    let list = list_entry(NtfsAttributeType::FileName, "", 0, number + 1);
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

fn freed(mut record: Vec<u8>) -> Vec<u8> {
    mark_freed(&mut record);
    record
}

// Shapes measured on NTFS 3.1: a file whose base holds only `$STANDARD_INFORMATION` and an
// `$ATTRIBUTE_LIST` (names and data live in extension records); an extension record with one
// name; an extension record emptied when its only name was unlinked (used size 64: 56 byte
// header, 8 byte end marker, no attributes). Pinned byte-for-byte to NTFS's own output.
#[test]
fn the_fixtures_of_the_measured_shapes_are_what_was_measured() {
    let u32_at =
        |record: &[u8], at: usize| u32::from_le_bytes(record[at..at + 4].try_into().unwrap());

    // Base: header, `$STANDARD_INFORMATION` (0x60 bytes), `$ATTRIBUTE_LIST` (24 byte header and one
    // 32 byte entry: 56), the end marker: 56 + 96 + 56 + 8 = 216.
    let base = base_with_list(24, 2, true);
    assert_eq!(u32_at(&base, 0x18), 216, "used size");
    assert_eq!(u32_at(&base, 56), 0x10, "first attribute");
    assert_eq!(u32_at(&base, 56 + 0x60), 0x20, "second attribute");
    assert_eq!(u32_at(&base, 56 + 0x60 + 56), 0xFFFF_FFFF, "end marker");
    // Without standard information the list is first.
    let base = base_with_list(24, 2, false);
    assert_eq!(u32_at(&base, 0x18), 56 + 56 + 8);
    assert_eq!(u32_at(&base, 56), 0x20);

    // An extension record with one name: 56 + 0x60 + 8.
    let extension = extension(25, 2, reference(1, 24), "b");
    assert_eq!(u32_at(&extension, 0x18), 56 + 0x60 + 8);
    assert_eq!(u32_at(&extension, 56), 0x30);
    assert_eq!(&extension[32..40], &reference(1, 24).to_le_bytes());

    // Emptied: nothing between the header and the end marker.
    let emptied = empty_extension(26, 2, reference(1, 24));
    assert_eq!(u32_at(&emptied, 0x18), 64);
    assert_eq!(u32_at(&emptied, 56), 0xFFFF_FFFF);

    // `freed` here only clears the flag (tests below store a freed record with its post-delete
    // sequence, and a reference with its pre-delete one); `delete_record` is the whole change
    // (see the pin in `test_records`).
    let gone = freed(base.clone());
    assert_eq!(u16::from_le_bytes([gone[22], gone[23]]) & 1, 0);
    assert_eq!(
        gone[24..],
        base[24..],
        "the rest of the record, used size included"
    );
    assert_eq!(
        gone[16..22],
        base[16..22],
        "the sequence number is the caller's"
    );
}

fn names(file: &NtfsFile) -> Vec<String> {
    file.names().map(|name| name.to_string()).collect()
}

fn record_numbers(file: &NtfsFile) -> Vec<u64> {
    file.records().map(|record| record.number()).collect()
}

/// An `Mft` of one record, 24, stored with `sequence`, in the given state.
fn mft_with_state(sequence: u16, in_use: bool, allocated: bool) -> Mft {
    let mut record = base(24, sequence, "a.txt");
    if !in_use {
        mark_freed(&mut record);
    }
    mft_with_freed(vec![record], if allocated { &[] } else { &[24] })
}

// The reference rule: a live record is named by its own reference; a freed one (`$BITMAP` bit
// clear) by the reference one sequence below its own; everything else names another incarnation.
// Seen failing: comparing the reference's own sequence for freed records
// (`Liveness::of_reference`, `Freed => reference`) fails the freed rows; accepting any sequence
// fails the S+2 and S rows.
#[test]
fn a_reference_names_a_live_record_by_its_sequence_and_a_freed_one_by_its_sequence_minus_one() {
    // (stored sequence, in use, allocated, sequence of the reference, is named)
    #[rustfmt::skip]
    let rows = [
        // Live: the same sequence and nothing else.
        (7, true, true, 7, true),
        (7, true, true, 6, false), // a record in use at S + 1 is a reuse
        (7, true, true, 8, false),
        // Freed: one sequence below its own.
        (7, false, false, 6, true),
        (7, false, false, 7, false),
        (7, false, false, 5, false), // freed, reused and freed again: S + 2
        (7, false, false, 8, false),
        // The sequence wraps at 16 bits; NTFS is documented to skip 0, so a record freed from
        // 0xFFFF carries 0 or 1. Unverified on a real volume.
        (0, false, false, 0xFFFF, true),
        (1, false, false, 0xFFFF, true),
        (1, false, false, 0, true), // plain rule: freed from 0
        (1, false, false, 0xFFFE, false),
        (2, false, false, 0xFFFF, false),
        (0, false, false, 0xFFFE, false),
        (0, false, false, 0, false),
        (1, true, true, 0xFFFF, false), // in use at 1 is a reuse, whatever it was
        (0xFFFF, false, false, 0xFFFE, true),
        (0xFFFF, false, false, 0xFFFF, false),
        (0xFFFF, true, true, 0xFFFF, true),
        // The flag and the bitmap must agree.
        (8, false, true, 7, false),
        (8, false, true, 8, false),
        (8, true, false, 8, false),
        (8, true, false, 7, false),
    ];
    for (sequence, in_use, allocated, claimed, named) in rows {
        let mft = mft_with_state(sequence, in_use, allocated);
        let id = FileId::from(reference(claimed, 24));
        let found = mft.record_by_id(id);
        assert_eq!(
            found.is_some(),
            named,
            "record {sequence} in use {in_use} allocated {allocated}, reference {claimed}"
        );
        if let Some(found) = found {
            assert_eq!(found.number(), 24);
            assert_eq!(found.is_used(), in_use);
        }
    }
}

#[test]
fn record_by_id_finds_live_and_freed_records_and_nothing_else() {
    let mft = mft_with_freed(
        vec![
            base(24, 3, "live.txt"),
            freed(base(25, 6, "deleted.txt")),
            extension(26, 2, reference(3, 24), "ext-of-live"),
        ],
        &[25],
    );

    let live = mft
        .record_by_id(FileId::from(reference(3, 24)))
        .expect("live");
    assert_eq!((live.number(), live.is_used()), (24, true));
    assert_eq!(names(&live), ["live.txt", "ext-of-live"]);

    // The id a journal record for the delete carries: the sequence before the free.
    let deleted = mft
        .record_by_id(FileId::from(reference(5, 25)))
        .expect("deleted");
    assert_eq!((deleted.number(), deleted.is_used()), (25, false));
    assert_eq!(names(&deleted), ["deleted.txt"]);
    // `NtfsFile::file_id` gives the id the file had while live, so it finds the record; the raw
    // reference, one sequence up, names another incarnation.
    assert_eq!(deleted.file_id(), FileId::from(reference(5, 25)));
    assert_eq!(
        mft.record_by_id(deleted.file_id())
            .map(|found| found.number()),
        Some(25)
    );
    assert!(mft
        .record_by_id(FileId::from(deleted.reference()))
        .is_none());

    // Stale: the record was reused, or freed twice, or the id is of another record.
    for id in [
        reference(4, 24),
        reference(3, 25),
        reference(6, 25),
        reference(3, 26),
    ] {
        assert!(mft.record_by_id(FileId::from(id)).is_none(), "{id:#x}");
    }
    // Out of range.
    assert!(mft.record_by_id(FileId::from(reference(1, 27))).is_none());
    assert!(mft
        .record_by_id(FileId::from(reference(1, mft.record_count())))
        .is_none());
    assert!(mft
        .record_by_id(FileId::from(reference(1, u32::MAX as u64)))
        .is_none());
    // Not a 48+16 bit reference.
    let wide = (1u128 << 64) | reference(3, 24) as u128;
    assert!(mft.record_by_id(FileId::from(wide)).is_none());

    // Like `Mft::record`, it does not filter extension records out.
    let extension = mft
        .record_by_id(FileId::from(reference(2, 26)))
        .expect("extension");
    assert!(extension.is_extension());
}

// A record freed and then reused: the old id is stale, the new file has its own.
#[test]
fn a_reused_record_is_not_named_by_the_id_of_the_file_it_replaced() {
    // The old file was (3, 24); freeing made it 4; the new file lives at 4.
    let mft = mft_with(vec![base(24, 4, "new.txt")]);
    assert!(mft.record_by_id(FileId::from(reference(3, 24))).is_none());
    let new = mft
        .record_by_id(FileId::from(reference(4, 24)))
        .expect("new file");
    assert_eq!(names(&new), ["new.txt"]);
}

// Attribution of freed extension records: a freed base with none, one, and several. Extensions
// keep the base reference the live base had (sequence 7); the freed base is stored with 8.
#[test]
fn a_freed_base_is_its_records_and_the_freed_extensions_that_name_it() {
    let mft = mft_with_freed(
        vec![
            freed(base(24, 8, "alone.txt")),
            freed(base(25, 8, "several.txt")),
            freed(extension(26, 4, reference(7, 25), "second-name")),
            freed(extension(27, 4, reference(7, 25), "third-name")),
            freed(base(28, 8, "one.txt")),
            freed(extension(29, 4, reference(7, 28), "other-name")),
        ],
        &[24, 25, 26, 27, 28, 29],
    );

    let alone = mft.record(24).expect("record");
    assert!(!alone.is_used());
    assert_eq!(record_numbers(&alone), [24]);
    assert_eq!(names(&alone), ["alone.txt"]);

    let several = mft.record(25).expect("record");
    assert_eq!(record_numbers(&several), [25, 26, 27]);
    assert_eq!(
        names(&several),
        ["several.txt", "second-name", "third-name"]
    );
    assert_eq!(several.hard_links().count(), 3);
    // Asked of an extension record, it resolves to the base first.
    let from_extension = mft.record(27).expect("record");
    assert_eq!(record_numbers(&from_extension), [25, 26, 27]);

    let one = mft.record(28).expect("record");
    assert_eq!(record_numbers(&one), [28, 29]);
    assert_eq!(names(&one), ["one.txt", "other-name"]);
}

// A live base next to a freed extension record with matching numbers and base reference: the
// 0.4.5 bug (a live file picking up a stale freed extension). Seen failing: routing freed
// extensions into the live index (`from_parts`, `Liveness::Freed => extension_records.push`)
// reports "stale-name".
#[test]
fn a_live_base_ignores_a_freed_extension_record_that_names_it() {
    let mft = mft_with_freed(
        vec![
            base(24, 7, "live.txt"),
            extension(25, 3, reference(7, 24), "live-name-2"),
            freed(extension(26, 4, reference(7, 24), "stale-name")),
        ],
        &[26],
    );
    let file = mft.record(24).expect("record");
    assert!(file.is_used());
    assert_eq!(record_numbers(&file), [24, 25]);
    assert_eq!(names(&file), ["live.txt", "live-name-2"]);
}

// Leftovers of an earlier incarnation of record 24. It held a file at sequence 8 with two
// extension records; deleting it freed the base (now 9), leaving them with base reference
// (8, 24). A new file then took the record at 9 (reuse does not bump the sequence again); the
// extensions stayed put (seen on a real volume: 28 next to a reused base). The new, live file
// ignores them.
#[test]
fn a_reused_live_base_ignores_the_freed_extensions_of_the_file_it_replaced() {
    let mft = mft_with_freed(
        vec![
            base(24, 9, "new.txt"),
            freed(extension(25, 5, reference(8, 24), "old-name-1")),
            freed(extension(26, 5, reference(8, 24), "old-name-2")),
        ],
        &[25, 26],
    );
    let file = mft.record(24).expect("record");
    assert_eq!(record_numbers(&file), [24]);
    assert_eq!(names(&file), ["new.txt"]);
    assert_eq!(mft.files().count(), 1);
}

// The same record freed twice: the old file's extensions remain, base reference (8, 24); the
// second file's own are (9, 24); the base is now stored with 10. Only the second file's
// extensions are its. Seen failing: dropping the freed rule's sequence check
// (`Liveness::of_reference` accepting a freed record by number alone) brings the old names back.
#[test]
fn a_freed_base_ignores_the_freed_extensions_of_the_file_it_replaced() {
    let mft = mft_with_freed(
        vec![
            freed(base(24, 10, "second.txt")),
            freed(extension(25, 5, reference(8, 24), "first-name")),
            freed(extension(26, 5, reference(9, 24), "second-name")),
        ],
        &[24, 25, 26],
    );
    let file = mft.record(24).expect("record");
    assert_eq!(record_numbers(&file), [24, 26]);
    assert_eq!(names(&file), ["second.txt", "second-name"]);
}

// An unlink frees an extension record while its base stays live: it is empty but carries the
// live base's reference. It belongs to the file by rule, adding nothing, live or deleted.
#[test]
fn an_empty_freed_extension_adds_nothing_to_a_live_or_deleted_file() {
    let mft = mft_with_freed(
        vec![
            base(24, 7, "live.txt"),
            freed(empty_extension(25, 4, reference(7, 24))),
            freed(base(26, 8, "deleted.txt")),
            freed(empty_extension(27, 4, reference(7, 26))),
        ],
        &[25, 26, 27],
    );
    let live = mft.record(24).expect("record");
    assert_eq!(record_numbers(&live), [24]);
    assert_eq!(names(&live), ["live.txt"]);

    let deleted = mft.record(26).expect("record");
    assert_eq!(record_numbers(&deleted), [26, 27]);
    assert_eq!(names(&deleted), ["deleted.txt"]);
    assert_eq!(deleted.attributes().count(), 3);
}

// The flag and the bitmap disagreeing: neither state is trusted, so the record has no file
// behind it.
#[test]
fn a_record_whose_flag_and_bitmap_disagree_has_no_records() {
    for (in_use, allocated) in [(false, true), (true, false)] {
        let mft = mft_with_state(8, in_use, allocated);
        let file = mft.record(24).expect("record");
        assert_eq!(
            record_numbers(&file),
            [] as [u64; 0],
            "{in_use} {allocated}"
        );
        assert_eq!(names(&file), [] as [String; 0]);
    }
}

// A base that is neither live nor freed (flag and bitmap disagree) is not a file, but live
// extension records that name it by its own reference are still its records; the deleted-file
// rules leave that alone. Found by the property test
// `a_described_mft_loads_to_what_the_description_says` (seed 0x49ff6e8100010000) when
// `records()` dropped them.
#[test]
fn a_base_that_is_neither_live_nor_freed_keeps_its_live_extension_records() {
    for (in_use, allocated) in [(true, false), (false, true)] {
        let mut record = base(24, 7, "base.txt");
        if !in_use {
            mark_freed(&mut record);
        }
        let mft = mft_with_freed(
            vec![record, extension(25, 3, reference(7, 24), "extension-name")],
            if allocated { &[] } else { &[24] },
        );
        let extension = mft.record(25).expect("record");
        assert_eq!(
            record_numbers(&extension),
            [25],
            "base in use {in_use} allocated {allocated}"
        );
        assert_eq!(names(&extension), ["extension-name"]);
    }
}

// A live record never sees a deleted file, even one it points at: a live extension record whose
// base is freed is still live, and a path walk reaching it (a parent reference) must not get the
// deleted file's names. Found by the property test
// `a_described_mft_loads_to_what_the_description_says` (seed 0x2933d42300010000) when `records()`
// looked only at the base.
#[test]
fn a_live_extension_record_of_a_freed_base_does_not_see_the_deleted_file() {
    let mft = mft_with_freed(
        vec![
            freed(base(24, 8, "deleted.txt")),
            freed(extension(25, 4, reference(7, 24), "deleted-name-2")),
            extension(26, 3, reference(7, 24), "live-extension"),
            // A file whose parent reference names the live extension record.
            {
                let mut record = new_record(27, 1, 0);
                let offset = add_file_name_ex(
                    &mut record,
                    ATTRIBUTES_OFFSET,
                    1,
                    reference(3, 26),
                    NtfsFileNamespace::Win32,
                    "child.txt",
                    0,
                );
                finish_record(&mut record, offset);
                record
            },
        ],
        &[24, 25],
    );
    // The deleted file keeps its own names.
    assert_eq!(
        names(&mft.record(24).expect("record")),
        ["deleted.txt", "deleted-name-2"]
    );
    // The live record next to it is unaffected and sees none of it.
    let live = mft.record(26).expect("record");
    assert!(live.is_used());
    assert_eq!(record_numbers(&live), [] as [u64; 0]);
    assert_eq!(names(&live), [] as [String; 0]);
    let child = mft.record(27).expect("record").best_name().expect("a name");
    assert_eq!(mft.resolve_path(&child, &mut ()), None);
}

// A live extension record whose base is freed (or the reverse) is not the file's: the base's
// state picks the index.
#[test]
fn the_base_decides_between_live_and_freed_extension_records() {
    let mft = mft_with_freed(
        vec![
            // A deleted file whose extension record is still marked live, with the right reference.
            freed(base(24, 8, "deleted.txt")),
            extension(25, 4, reference(7, 24), "live-extension"),
            // A live file whose extension is freed, with the right reference.
            base(26, 7, "live.txt"),
            freed(extension(27, 4, reference(7, 26), "freed-extension")),
        ],
        &[24, 27],
    );
    assert_eq!(names(&mft.record(24).expect("record")), ["deleted.txt"]);
    assert_eq!(names(&mft.record(26).expect("record")), ["live.txt"]);
}

// A record naming an extension record as its base belongs to no file: extension records have no
// extensions of their own. Freed E1 (25) is an extension of freed base 24; freed E2 (26) names E1
// as its base. The base owns E1 as a record; E2 owns nobody: not deleted, no records, its name not
// the file's. Seen failing: without `!base.is_extension()` in `NtfsFile::is_deleted`, E2 is
// deleted; without it in `NtfsFile::records`'s `freed` branch, E2 asks E1 and gets `[25, 26]`.
#[test]
fn a_freed_extension_that_names_an_extension_record_as_its_base_belongs_to_no_file() {
    let mft = mft_with_freed(
        vec![
            freed(base(24, 8, "deleted.txt")),
            freed(extension(25, 4, reference(7, 24), "first-extension")),
            freed(extension(26, 4, reference(3, 25), "nobody-name")),
        ],
        &[24, 25, 26],
    );
    let base = mft.record(24).expect("record");
    assert!(base.is_deleted());
    assert_eq!(record_numbers(&base), [24, 25]);
    assert_eq!(names(&base), ["deleted.txt", "first-extension"]);
    assert!(mft.record(25).expect("record").is_deleted());

    let nobody = mft.record(26).expect("record");
    assert!(!nobody.is_deleted());
    assert_eq!(record_numbers(&nobody), [] as [u64; 0]);
    assert_eq!(names(&nobody), [] as [String; 0]);
}

// The live side: E2 (26) is in use and names live extension E1 (25) as its base. A healthy
// volume never has this shape, so the rule here is a decision, not a measurement: pinned only
// that it is not a deleted file, and that a live record only ever sees live records. Whether E2
// sees E1 as a record, or nothing, is left open.
#[test]
fn a_live_extension_that_names_a_live_extension_record_as_its_base_is_not_deleted() {
    let mft = mft_with_freed(
        vec![
            base(24, 7, "live.txt"),
            extension(25, 3, reference(7, 24), "first-extension"),
            extension(26, 3, reference(3, 25), "second-extension"),
        ],
        &[],
    );
    assert_eq!(
        names(&mft.record(24).expect("record")),
        ["live.txt", "first-extension"],
        "the file itself is not changed by a record that names an extension record as its base"
    );
    for number in [24, 25, 26] {
        let file = mft.record(number).expect("record");
        assert!(!file.is_deleted(), "record {number}");
        assert!(
            file.records().all(|record| record.is_used()),
            "record {number} sees {:?}",
            record_numbers(&file)
        );
    }
}

// Which freed records are files. Numbers below 24 are the reserved records.
#[test]
fn deleted_files_yields_the_freed_base_records_that_still_hold_a_file() {
    let mut bad_header = freed(base(27, 8, "bad-header"));
    bad_header[0..4].copy_from_slice(b"BAAD");

    let mft = mft_with_at_freed(
        vec![
            // A reserved record, freed with a name: not a file.
            (10, freed(base(10, 8, "reserved"))),
            // In use: a file, not a deleted one.
            (24, base(24, 3, "live.txt")),
            // Freed with a name, standard information and data: yielded.
            (25, freed(base(25, 8, "deleted.txt"))),
            // A slot that never held a record.
            (26, vec![0u8; RECORD_SIZE]),
            // A freed record with no valid header.
            (27, bad_header),
            // A freed extension record of 25: never a file, and 25 counts once.
            (28, freed(extension(28, 4, reference(7, 25), "second-name"))),
            // Not in use by its flag, but the bitmap still says allocated.
            (29, freed(base(29, 8, "flag-only"))),
            // In use by its flag, but the bitmap says free.
            (30, base(30, 8, "bitmap-only")),
            // The shape of a file with an attribute list: only standard information and the
            // list in the base, no name.
            (31, freed(base_with_list(31, 8, true))),
            // Nothing but the list in the base; the name is in the freed extension record 33.
            (32, freed(base_with_list(32, 8, false))),
            (
                33,
                freed(extension(33, 4, reference(7, 32), "only-in-extension")),
            ),
            // Data only: no standard information and no name anywhere.
            (34, freed(data_only(34, 8))),
            // The name is in an extension record of an earlier incarnation.
            (35, freed(base_with_list(35, 8, false))),
            (
                36,
                freed(extension(36, 4, reference(6, 35), "old-incarnation")),
            ),
        ],
        &[10, 25, 26, 27, 28, 30, 31, 32, 33, 34, 35, 36],
    );

    let deleted: Vec<u64> = mft.deleted_files().map(|file| file.number()).collect();
    assert_eq!(deleted, [25, 31, 32]);
    for file in mft.deleted_files() {
        assert!(!file.is_used());
        assert!(!file.is_extension());
    }

    let with_extension = mft.record(32).expect("record");
    assert_eq!(names(&with_extension), ["only-in-extension"]);
    let no_name = mft.record(31).expect("record");
    assert_eq!(names(&no_name), [] as [String; 0]);
    assert!(no_name.standard_information().is_some());
    assert_eq!(FileInfo::new(&no_name).name, "");

    // `files()` is the live ones, and only those.
    let live: Vec<u64> = mft.files().map(|file| file.number()).collect();
    assert_eq!(live, [24]);
}

fn data_only(number: u64, sequence: u16) -> Vec<u8> {
    let mut record = new_record(number, sequence, 0);
    let offset = add_resident_attribute(
        &mut record,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Data,
        2,
        "",
        b"data",
    );
    finish_record(&mut record, offset);
    record
}

// A deleted file's accessors report exactly what its record holds, sizes included: a
// non-resident stream with an attribute list was seen truncated to 0, and is reported as such.
#[test]
fn a_deleted_file_reports_the_size_its_record_still_says() {
    let mut record = new_record(24, 8, 0);
    let mut offset = add_standard_information(&mut record, ATTRIBUTES_OFFSET, ARCHIVE);
    offset = add_file_name(&mut record, offset, "truncated.bin", ARCHIVE);
    offset = add_nonresident_data(&mut record, offset, 0);
    offset = add_nonresident_attribute(
        &mut record,
        offset,
        NtfsAttributeType::Data,
        3,
        "ads",
        0,
        4096,
    );
    finish_record(&mut record, offset);
    mark_freed(&mut record);

    let mft = mft_with_freed(vec![record], &[24]);
    let file = mft.deleted_files().next().expect("a deleted file");
    let streams: Vec<_> = file
        .data_streams()
        .map(|stream| (stream.name, stream.size))
        .collect();
    assert_eq!(streams, [(None, 0), (Some("ads".into()), 4096)]);
    let info = FileInfo::new(&file);
    assert_eq!((info.name.as_str(), info.size), ("truncated.bin", 0));
}

/// A base record whose `$ATTRIBUTE_LIST` is non-resident, as every list seen on a real volume was.
fn base_with_nonresident_list(number: u64, sequence: u16) -> Vec<u8> {
    let mut record = new_record(number, sequence, 0);
    let mut offset = add_standard_information(&mut record, ATTRIBUTES_OFFSET, ARCHIVE);
    offset = add_nonresident_attribute(
        &mut record,
        offset,
        NtfsAttributeType::AttributeList,
        4,
        "",
        0,
        4096,
    );
    finish_record(&mut record, offset);
    record
}

/// An extension record that holds an `$ATTRIBUTE_LIST` and nothing else.
fn extension_with_list(number: u64, sequence: u16, base_reference: u64) -> Vec<u8> {
    let mut record = new_record(number, sequence, base_reference);
    let list = list_entry(NtfsAttributeType::FileName, "", 0, number + 1);
    let offset = add_resident_attribute(
        &mut record,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::AttributeList,
        4,
        "",
        &list,
    );
    finish_record(&mut record, offset);
    record
}

// A stream loses its runs only for a deleted file with an `$ATTRIBUTE_LIST` among its records.
// Seen failing: dropping `!base.is_used()` fails the live rows (24, 31); using `self` instead of
// the base fails 32; accepting any record with a list fails 35; dropping the list test fails 26;
// using the base only fails 36.
#[test]
fn stream_data_lost_is_true_only_for_a_deleted_file_with_an_attribute_list() {
    let mft = mft_with_freed(
        vec![
            // 24, 25: live files, with a list and without one.
            base_with_list(24, 7, true),
            base(25, 7, "live.txt"),
            // 26: deleted, no list. 27, 28: deleted with a list, resident and non-resident.
            freed(base(26, 8, "deleted.txt")),
            freed(base_with_list(27, 8, true)),
            freed(base_with_nonresident_list(28, 8)),
            // 29, 30: a deleted file with a list whose name is in a freed extension record.
            freed(base_with_list(29, 8, false)),
            freed(extension(30, 4, reference(7, 29), "name")),
            // 31, 32: a live file with a list and a freed extension record that names it.
            base_with_list(31, 7, true),
            freed(extension(32, 4, reference(7, 31), "stale")),
            // 33, 34: a deleted file with a list and a live extension record that names it.
            freed(base_with_list(33, 8, true)),
            extension(34, 3, reference(7, 33), "live-extension"),
            // 35: not in use, but the `$BITMAP` bit is set: neither live nor deleted.
            freed(base_with_list(35, 8, true)),
            // 36, 37: a deleted file whose only list is in an extension record that names it.
            freed(base(36, 8, "listed-elsewhere.txt")),
            freed(extension_with_list(37, 4, reference(7, 36))),
        ],
        &[26, 27, 28, 29, 30, 32, 33, 36, 37],
    );
    let lost = |number: u64| mft.record(number).expect("record").stream_data_lost();
    let rows = [
        (24, false),
        (25, false),
        (26, false),
        (27, true),
        (28, true),
        (29, true),
        (30, true),
        (31, false),
        (32, false),
        (33, true),
        (34, false),
        (35, false),
        (36, true),
        (37, true),
    ];
    for (number, expected) in rows {
        assert_eq!(lost(number), expected, "record {number}");
    }
}

// The path cache holds live directories only: a deleted directory's path is never inserted, or a
// reference to the freed record would resolve through it. The live directory is the control,
// confirming the check is not inert.
#[test]
fn file_info_does_not_cache_the_path_of_a_deleted_directory() {
    let directory = |number, sequence, name: &str| {
        let mut record = new_record(number, sequence, 0);
        set_record_flags(&mut record, directory_flags());
        let offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, name, 0);
        finish_record(&mut record, offset);
        record
    };
    let mft = mft_with_freed(
        vec![directory(24, 3, "here"), freed(directory(25, 8, "gone"))],
        &[25],
    );
    let mut cache = DefaultPathCache::new();

    let live = FileInfo::with_cache(&mft.record(24).expect("record"), &mut cache);
    assert!(live.is_directory && live.path.is_some());
    assert!(matches!(
        cache.get(reference(3, 24)),
        CachedPath::Resolved(_)
    ));

    let deleted = FileInfo::with_cache(&mft.record(25).expect("record"), &mut cache);
    assert!(deleted.is_directory);
    assert_eq!(deleted.name, "gone");
    for claimed in [7, 8] {
        assert!(
            matches!(cache.get(reference(claimed, 25)), CachedPath::Unknown),
            "the deleted directory was cached under sequence {claimed}"
        );
    }
}
