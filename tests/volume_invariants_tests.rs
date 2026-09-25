#![cfg(target_os = "windows")]

//! Invariants of a real volume's whole `Mft`, checked over every record instead of the few files
//! a test makes: live and deleted files are disjoint, `is_deleted()` agrees with `deleted_files()`,
//! every deleted file still has something to identify it, every file id finds its own record
//! again, and no deleted file's path or summary panics, whatever state the records are in. Runs on
//! the test volume (with the deleted files other tests left on it) and, read-only, on the parity
//! volume when `NTFS_READER_PARITY_VOLUME` is set (the VM's system drive: years of real deletes).

use std::collections::HashSet;

use ntfs_reader::{DeletedPathCache, FileInfo, Mft, Volume};

mod common;
use common::{flush_volume, parity_volume_letter, skip, test_volume_letter};

/// The most deleted files whose paths and summaries are computed (each walks its whole chain of
/// parents, so a volume with millions of them would take long). The counts are printed.
const MAX_DELETED_CHECKED: usize = 200_000;

fn load(letter: &str) -> Mft {
    flush_volume(letter);
    let volume = Volume::new(format!("\\\\.\\{letter}:")).expect("open the volume");
    Mft::new(volume).expect("load the MFT")
}

/// Checks one volume's invariants. With `strict` (the test volume, nothing else writes to it) a
/// violation fails the test; without it (a live system volume, changing between the record and
/// bitmap snapshots) violations are counted and printed instead. Nothing may panic either way.
fn check_volume(letter: &str, strict: bool) {
    let mft = load(letter);
    let live: HashSet<u64> = mft.files().map(|file| file.number()).collect();
    let deleted: HashSet<u64> = mft.deleted_files().map(|file| file.number()).collect();
    let mut violations: Vec<String> = Vec::new();
    let (mut both, mut disagree, mut not_found, mut wrong_summary) = (0u64, 0u64, 0u64, 0u64);

    for number in live.intersection(&deleted) {
        both += 1;
        violations.push(format!(
            "record {number} is in both files() and deleted_files()"
        ));
    }

    // `is_deleted()` is membership of `deleted_files()`, for every base record, including the ones
    // that are neither live nor deleted (a flag and a bit that disagree, a delete that is pending).
    let mut neither = 0u64;
    for number in 0..mft.record_count() {
        let Some(file) = mft.record(number) else {
            continue;
        };
        if file.is_extension() {
            continue;
        }
        if file.is_deleted() != deleted.contains(&number) {
            disagree += 1;
            violations.push(format!(
                "record {number}: is_deleted() and deleted_files() disagree"
            ));
        }
        if !live.contains(&number) && !deleted.contains(&number) {
            neither += 1;
        }
    }

    // Every file, live or deleted, is found again by its own id.
    for (kind, numbers) in [("live", &live), ("deleted", &deleted)] {
        for &number in numbers {
            let file = mft.record(number).expect("a listed record exists");
            let found = mft.record_by_id(file.file_id());
            if found.map(|found| found.number()) != Some(number) {
                not_found += 1;
                violations.push(format!(
                    "the id {:#x} of the {kind} record {number} does not find it",
                    file.file_id().as_u128()
                ));
            }
        }
    }

    // A deleted file has something that says what it was, and nothing about it panics.
    let (mut without_identity, mut names, mut complete, mut incomplete, mut checked) =
        (0u64, 0u64, 0u64, 0u64, 0usize);
    let mut cache = DeletedPathCache::new();
    let mut numbers: Vec<u64> = deleted.iter().copied().collect();
    numbers.sort_unstable();
    for &number in numbers.iter().take(MAX_DELETED_CHECKED) {
        let file = mft.record(number).expect("a listed record exists");
        if file.standard_information().is_none() && file.names().next().is_none() {
            without_identity += 1;
            violations.push(format!(
                "deleted record {number} has neither standard information nor a name"
            ));
        }
        for name in file.names() {
            names += 1;
            if mft.resolve_deleted_path(&name, &mut cache).complete {
                complete += 1;
            } else {
                incomplete += 1;
            }
        }
        if !FileInfo::new(&file).is_deleted {
            wrong_summary += 1;
            violations.push(format!("record {number}: FileInfo says it is not deleted"));
        }
        checked += 1;
    }
    println!(
        "MEASURED: {letter}: {} records, {} live files, {} deleted files ({checked} checked), {neither} \
         base records neither live nor deleted, {names} deleted names: {complete} with a complete path, \
         {incomplete} without. Violations: {both} in both lists, {disagree} is_deleted disagreements, \
         {not_found} ids that do not find their record, {without_identity} deleted without a name or \
         times, {wrong_summary} FileInfo not deleted; first ones: {:?}",
        mft.record_count(),
        live.len(),
        deleted.len(),
        &violations[..violations.len().min(10)],
    );
    assert!(
        !strict || violations.is_empty(),
        "{letter}: {} violation(s), first ones: {:?}",
        violations.len(),
        &violations[..violations.len().min(10)]
    );
}

#[test]
fn the_test_volume_holds_together() {
    check_volume(&test_volume_letter(), true);
}

// A smoke check, not an invariants check: the parity volume is read-only and live (the VM's
// system drive has years of real deletes, in shapes no test makes), so changes between the
// record and bitmap snapshots turn a violation into a count to look at, not a failure. What
// fails this test is a panic anywhere in the deleted-file API, or a volume that fails to load.
// The strict invariants are in `the_test_volume_holds_together`, on the test volume.
#[test]
fn the_parity_volume_is_smoke_checked() {
    const NAME: &str = "the_parity_volume_is_smoke_checked";
    if std::env::var_os("NTFS_READER_PARITY_VOLUME").is_none() {
        skip(NAME, "NTFS_READER_PARITY_VOLUME is not set");
        return;
    }
    let letter = parity_volume_letter();
    if letter == test_volume_letter() {
        skip(
            NAME,
            "the parity volume is the test volume, already checked",
        );
        return;
    }
    check_volume(&letter, false);
}
