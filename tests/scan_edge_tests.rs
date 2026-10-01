#![cfg(target_os = "windows")]

//! Edge cases for `MftScan`, `Mft::new_compact` and `DefaultPathCache::with_max_bytes` against
//! a real NTFS volume on `NTFS_READER_TEST_VOLUME`. Every test builds its own fixtures and
//! tears them down (`fixture_dir`/`TempDirGuard`); `scan_stress_tests.rs` compares scans and
//! caches against a whole `Mft` on a separate volume without writing to it.
//!
//! Tests that write to the volume take `serial()` first: NTFS hands a new file the lowest free
//! record at once, so two tests creating and deleting fixtures at the same time can steal each
//! other's records (the same rule `deleted_metadata_tests.rs`/`deleted_path_tests.rs` follow).

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use ntfs_reader::{DefaultPathCache, Mft, MftScan, Volume};

mod common;
use common::{
    assert_parity, compare_mfts, compare_scan, describe_record, flush_volume, reference_of,
    scan_chunk_sizes, test_volume_letter, ComparisonPolicy, TempDirGuard, RECORD_NUMBER_MASK,
};

/// The tests create and delete fixtures on the shared test volume; they cannot overlap (see the
/// file's docs).
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fixture_dir(name: &str) -> TempDirGuard {
    TempDirGuard::new(format!("{}:\\scan-edge-{name}", test_volume_letter()))
        .expect("create the fixture directory")
}

/// Flushes the volume and loads its whole `$MFT`.
fn load_mft() -> Mft {
    let letter = test_volume_letter();
    flush_volume(&letter);
    let volume = Volume::new(format!("\\\\.\\{letter}:")).expect("open the volume");
    Mft::new(volume).expect("load the MFT")
}

/// As [`load_mft`], retrying for up to a few seconds until `ready` holds: for a delete-pending
/// fixture, the record does not free itself the instant the handle closes (`deleted_metadata_tests.rs`'s
/// `load_where`, copied here since integration test binaries share no code but `common`).
fn load_mft_where(what: &str, numbers: &[u64], mut ready: impl FnMut(&Mft) -> bool) -> Mft {
    let mut last = Vec::new();
    for _ in 0..6 {
        let mft = load_mft();
        if ready(&mft) {
            return mft;
        }
        last = numbers
            .iter()
            .map(|&number| describe_record(&mft, number))
            .collect();
        std::thread::sleep(Duration::from_millis(1500));
    }
    panic!("{what}: the MFT never showed the expected state: {last:#?}");
}

fn record_number_of(path: &Path) -> u64 {
    reference_of(path) & RECORD_NUMBER_MASK
}

/// Number of UTF-16 units `path` would take as a Win32 path (what `resolve_path`'s 32767-unit
/// limit counts): the same measure `std::os::windows::ffi::OsStrExt::encode_wide` gives the
/// crate's own path building.
fn path_units(path: &Path) -> usize {
    path.as_os_str().encode_wide().count()
}

/// Compares public-file snapshots from `mft` (already loaded) against a fresh [`MftScan`] of
/// the same quiet volume, using `chunk_records` records per chunk. Requires an exact match.
fn assert_scan_matches(mft: &Mft, chunk_records: u64) {
    let volume_path = mft.volume().path().to_string_lossy().into_owned();
    let mut scan = MftScan::with_chunk_records(
        Volume::new(&volume_path).expect("open the volume"),
        chunk_records,
    )
    .unwrap_or_else(|e| panic!("MftScan::with_chunk_records({chunk_records}): {e}"));
    assert_parity(
        ComparisonPolicy::Quiet,
        &format!("scan {volume_path}, chunk_records={chunk_records}"),
        compare_scan(mft, &mut scan),
    );
    assert_eq!(
        scan.corrupt_records(),
        mft.corrupt_records(),
        "quiet scan corrupt_records"
    );
}

// ---- Chunk sizes ------------------------------------------------------------------------------

#[test]
fn scan_matches_mft_at_every_chunk_size() {
    let _guard = serial();
    let mft = load_mft();
    assert!(mft.record_count() > 0, "the test volume's $MFT is empty");
    for chunk_records in scan_chunk_sizes(mft.record_count()) {
        assert_scan_matches(&mft, chunk_records);
    }
}

// ---- Extension records far from their base -----------------------------------------------------

/// A file whose attributes spill into an `$ATTRIBUTE_LIST` created late, so its extension
/// records land far from its base record in the `$MFT`. Enough hard links and named streams
/// force extensions; small chunks put the base and extensions in different chunks.
#[test]
fn scan_matches_mft_when_extension_records_land_in_a_far_chunk() {
    let _guard = serial();
    let dir = fixture_dir("far-extension");
    let target = dir.path().join("target.txt");
    fs::write(&target, b"far").expect("write target.txt");

    const LINK_DIRS: usize = 8;
    // NTFS permits at most 1024 $FILE_NAME attributes, including the original name. Keep one
    // below that ceiling so this fixture does not fail with ERROR_TOO_MANY_LINKS (1142).
    const EXTRA_LINKS: usize = 1022;
    const STREAMS: usize = 3000;
    let link_dirs: Vec<PathBuf> = (0..LINK_DIRS)
        .map(|i| {
            let sub = dir.path().join(format!("links{i}"));
            fs::create_dir(&sub).unwrap_or_else(|e| panic!("create links{i}: {e}"));
            sub
        })
        .collect();
    for i in 0..EXTRA_LINKS {
        let sub = &link_dirs[i % LINK_DIRS];
        fs::hard_link(&target, sub.join(format!("link{i}.txt")))
            .unwrap_or_else(|e| panic!("hard_link {i}: {e}"));
    }
    for i in 0..STREAMS {
        fs::write(dir.path().join(format!("target.txt:s{i}")), b"x")
            .unwrap_or_else(|e| panic!("named stream {i}: {e}"));
    }

    let mft = load_mft();
    let number = record_number_of(&target);
    let file = mft
        .record(number)
        .unwrap_or_else(|| panic!("record {number} (target.txt) missing from the loaded $MFT"));
    assert!(
        file.records().count() > 1,
        "fixture is inert: target.txt has no extension record (records().count() == {})",
        file.records().count()
    );
    assert_eq!(
        file.hard_links().count(),
        1 + EXTRA_LINKS,
        "fixture is inert: expected every hard link to be counted"
    );
    assert_eq!(
        file.data_streams().filter(|s| s.name.is_some()).count(),
        STREAMS,
        "fixture is inert: expected every named stream to be counted"
    );

    assert_scan_matches(&mft, 4);
    let compact = Mft::new_compact(Volume::new(mft.volume().path()).expect("open volume"))
        .expect("compact Mft of the hard-link/ADS fixture");
    assert_parity(
        ComparisonPolicy::Quiet,
        "compact hard-link/ADS fixture",
        compare_mfts(&mft, &compact),
    );
}

// ---- A directory numbered above its children ---------------------------------------------------

/// A directory whose record number is higher than its children's: create the children first (so
/// they take the lower record numbers), then create the directory and move the children into it.
#[test]
fn scan_matches_mft_when_a_directory_is_numbered_above_its_children() {
    let _guard = serial();
    let dir = fixture_dir("late-parent");
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    fs::write(&a, b"a").expect("write a.txt");
    fs::write(&b, b"b").expect("write b.txt");

    let target = dir.path().join("target");
    fs::create_dir(&target).expect("create target");
    let a2 = target.join("a.txt");
    let b2 = target.join("b.txt");
    fs::rename(&a, &a2).expect("move a.txt under target");
    fs::rename(&b, &b2).expect("move b.txt under target");

    let target_number = record_number_of(&target);
    let a_number = record_number_of(&a2);
    let b_number = record_number_of(&b2);
    assert!(
        target_number > a_number && target_number > b_number,
        "fixture is inert: target ({target_number}) must be numbered above its children \
         (a={a_number}, b={b_number})"
    );

    let mft = load_mft();
    assert_scan_matches(&mft, 2);
}

// ---- Deep trees and near-limit paths ------------------------------------------------------------

/// 2000 nested directories. Short names keep this well under the 32767-unit path limit;
/// [`scan_matches_mft_with_a_path_near_the_unit_limit`] covers that edge separately.
#[test]
fn scan_matches_mft_with_a_2000_level_deep_tree() {
    let _guard = serial();
    const LEVELS: usize = 2000;
    let letter = test_volume_letter();
    let root = format!("\\\\?\\{letter}:\\scan-edge-deep");
    let _cleanup = TempDirGuard::new(&root).expect("create the deep-tree root");

    let mut path = PathBuf::from(&root);
    for i in 0..LEVELS {
        path.push(format!("d{i}"));
    }
    fs::create_dir_all(&path).unwrap_or_else(|e| panic!("create {LEVELS} nested directories: {e}"));
    let leaf_file = path.join("leaf.txt");
    fs::write(&leaf_file, b"leaf").expect("write the leaf file");

    let mft = load_mft();
    let leaf_number = record_number_of(&leaf_file);
    let leaf = mft
        .record(leaf_number)
        .expect("the leaf file's record is in the $MFT");
    let mut cache = DefaultPathCache::new();
    let leaf_name = leaf.best_name().expect("the leaf file has a name");
    let resolved = mft
        .resolve_path(&leaf_name, &mut cache)
        .expect("a 2000-level-deep path must still resolve while live");
    assert!(
        resolved.components().count() >= LEVELS,
        "resolved path has only {} components, expected at least {LEVELS}",
        resolved.components().count()
    );

    assert_scan_matches(&mft, 8);
}

/// A path as close to the 32767-unit Win32 limit as this real volume accepts must resolve.
/// The exact cutoff is covered by `resolve_path_stops_at_exactly_32767_utf16_units` on
/// synthetic records. Real NTFS/Win32 may refuse further levels before that limit with
/// ERROR_PATH_NOT_FOUND (3), so stop at the deepest path that can be created.
#[test]
fn scan_matches_mft_with_a_path_near_the_unit_limit() {
    let _guard = serial();
    // Leave headroom for the final component and stop if Windows refuses another level.
    const BUDGET_UNITS: usize = 32700;
    const LEVEL_NAME_LEN: usize = 200;
    let letter = test_volume_letter();
    let root = format!("\\\\?\\{letter}:\\scan-edge-long");
    let _cleanup = TempDirGuard::new(&root).expect("create the long-path root");

    let component = "L".repeat(LEVEL_NAME_LEN);
    let mut path = PathBuf::from(&root);
    let mut units = path_units(&path);
    let mut levels = 0usize;
    while units + 1 + LEVEL_NAME_LEN + 1 + 12 < BUDGET_UNITS {
        let next = path.join(&component);
        match fs::create_dir(&next) {
            Ok(()) => {
                path = next;
                units = path_units(&path);
                levels += 1;
            }
            // Windows itself refuses to go deeper before the budget loop even asked for the last
            // possible level: stop here, with whatever depth was actually reached, rather than
            // failing the test over a real-OS ceiling this test isn't about.
            Err(_) => break,
        }
    }
    assert!(
        levels > 10,
        "the fixture path did not grow deep enough to be a real check (reached {units} units, \
         {levels} levels)"
    );

    let mft = load_mft();
    let dir_number = record_number_of(&path);
    let dir = mft
        .record(dir_number)
        .expect("the deepest directory's record is in the $MFT");
    let mut cache = DefaultPathCache::new();
    let dir_name = dir.best_name().expect("the deepest directory has a name");
    assert!(
        mft.resolve_path(&dir_name, &mut cache).is_some(),
        "a path {units} units deep, as close to the limit as this volume takes, must still resolve"
    );

    assert_scan_matches(&mft, 16);
}

// ---- A directory with 200k entries --------------------------------------------------------------

/// A directory with 200k entries. Slow (tens of seconds to a few minutes to create):
/// this is the real-OS check that a scan's directory name index and per-chunk file listing do not
/// assume a small directory.
#[test]
fn scan_matches_mft_with_a_200k_entry_directory() {
    let _guard = serial();
    const ENTRIES: usize = 200_000;
    let dir = fixture_dir("200k");
    for i in 0..ENTRIES {
        File::create(dir.path().join(format!("f{i:06}.txt")))
            .unwrap_or_else(|e| panic!("create file {i} of {ENTRIES}: {e}"));
    }

    let mft = load_mft();
    let dir_number = record_number_of(dir.path());
    let count = mft
        .files()
        .filter(|file| file.names().any(|name| name.parent_number() == dir_number))
        .count();
    assert_eq!(
        count, ENTRIES,
        "expected every created file to be a live child of the fixture directory"
    );

    assert_scan_matches(&mft, 4096); // MftScan::new's own default chunk size.
    assert_scan_matches(&mft, 97);
}

// ---- Deleted files, every way -------------------------------------------------------------------

/// Files and directories deleted through `remove_file`, `remove_dir`,
/// `remove_dir_all`, and delete-pending (deleted while a handle is still open). `MftScan` must
/// see every one of them exactly as a whole `Mft` does.
#[test]
fn scan_matches_mft_for_files_deleted_in_every_way() {
    let _guard = serial();
    let dir = fixture_dir("deleted-methods");

    let removed_file = dir.path().join("removed-file.txt");
    fs::write(&removed_file, b"a").expect("write removed-file.txt");
    fs::remove_file(&removed_file).expect("remove_file");

    let removed_dir = dir.path().join("removed-dir");
    fs::create_dir(&removed_dir).expect("create removed-dir");
    fs::remove_dir(&removed_dir).expect("remove_dir");

    let removed_tree = dir.path().join("removed-tree");
    fs::create_dir(&removed_tree).expect("create removed-tree");
    fs::write(removed_tree.join("child.txt"), b"child").expect("write removed-tree/child.txt");
    fs::remove_dir_all(&removed_tree).expect("remove_dir_all");

    let pending_file = dir.path().join("pending.txt");
    fs::write(&pending_file, b"pending").expect("write pending.txt");
    let pending_number = record_number_of(&pending_file);
    let handle = OpenOptions::new()
        .read(true)
        .share_mode(7) // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
        .open(&pending_file)
        .expect("open pending.txt with FILE_SHARE_DELETE");
    fs::remove_file(&pending_file).expect("remove_file while open");

    let mft = load_mft();
    assert!(
        mft.record(pending_number).is_some_and(|f| f.is_used()),
        "the delete-pending record must stay in use while its handle is open: {}",
        describe_record(&mft, pending_number)
    );
    assert_scan_matches(&mft, 4);

    drop(handle);
    let mft = load_mft_where(
        "scan_matches_mft_for_files_deleted_in_every_way",
        &[pending_number],
        |mft| !mft.record(pending_number).is_some_and(|f| f.is_used()),
    );
    assert_scan_matches(&mft, 4);
}

// ---- The volume changing mid-scan ---------------------------------------------------------------

/// The volume changes between `MftScan::new` (pass 1) and iterating chunks (pass 2): create,
/// delete, rename, move across directories, and grow a file into extension records. The scan
/// must terminate without panicking, find live files, and resolve paths using the directory
/// names captured in pass 1. Stable fixtures elsewhere check exact file-by-file parity.
#[test]
fn scan_survives_the_volume_changing_between_pass_1_and_pass_2() {
    let _guard = serial();
    let letter = test_volume_letter();
    let dir = fixture_dir("mid-scan");

    fs::write(dir.path().join("untouched.txt"), b"untouched").expect("write untouched.txt");
    let to_delete = dir.path().join("to-delete.txt");
    fs::write(&to_delete, b"gone soon").expect("write to-delete.txt");
    let to_rename = dir.path().join("to-rename.txt");
    fs::write(&to_rename, b"renamed soon").expect("write to-rename.txt");
    let move_from = dir.path().join("move-from");
    fs::create_dir(&move_from).expect("create move-from");
    let to_move = move_from.join("move-me.txt");
    fs::write(&to_move, b"moved soon").expect("write move-from/move-me.txt");
    let move_to = dir.path().join("move-to");
    fs::create_dir(&move_to).expect("create move-to");
    let to_grow = dir.path().join("to-grow.bin");
    fs::write(&to_grow, b"small").expect("write to-grow.bin");

    // Rename a directory and create another between the two passes. `renamed_dir`'s child must
    // keep resolving under the name captured in pass 1; `new_dir`'s child must resolve to `None`
    // because pass 1 never saw that directory.
    let renamed_dir = dir.path().join("renamed-dir");
    fs::create_dir(&renamed_dir).expect("create renamed-dir");
    let renamed_dir_child = renamed_dir.join("child.txt");
    fs::write(&renamed_dir_child, b"child").expect("write renamed-dir/child.txt");
    let renamed_dir_child_number = record_number_of(&renamed_dir_child);

    flush_volume(&letter);
    let mut scan = MftScan::with_chunk_records(
        Volume::new(format!("\\\\.\\{letter}:")).expect("open the volume"),
        4,
    )
    .expect("pass 1 (MftScan::new)");

    // Everything below happens strictly after pass 1 read the $MFT, before pass 2 reads a single
    // chunk of it.
    fs::remove_file(&to_delete).expect("delete to-delete.txt after pass 1");
    fs::rename(&to_rename, dir.path().join("renamed.txt")).expect("rename after pass 1");
    fs::rename(&to_move, move_to.join("move-me.txt"))
        .expect("move across directories after pass 1");
    {
        let mut grow_handle = OpenOptions::new()
            .append(true)
            .open(&to_grow)
            .expect("open to-grow.bin for append");
        // Enough extra hard links to push the growing file's attributes into an extension record
        // ("grow a file into extension records"), on top of the size growth itself.
        grow_handle
            .write_all(&vec![b'x'; 64 * 1024])
            .expect("grow to-grow.bin");
    }
    for i in 0..40 {
        fs::hard_link(&to_grow, dir.path().join(format!("grow-link{i}.txt")))
            .unwrap_or_else(|e| panic!("hard_link grow-link{i}: {e}"));
    }
    fs::write(dir.path().join("created-after.txt"), b"new").expect("write created-after.txt");

    let renamed_dir_new_name = dir.path().join("renamed-dir-new-name");
    fs::rename(&renamed_dir, &renamed_dir_new_name).expect("rename renamed-dir after pass 1");
    let new_dir = dir.path().join("new-dir-created-after-pass-1");
    fs::create_dir(&new_dir).expect("create new-dir-created-after-pass-1 after pass 1");
    let new_dir_child = new_dir.join("child.txt");
    fs::write(&new_dir_child, b"child").expect("write new-dir-created-after-pass-1/child.txt");
    let new_dir_child_number = record_number_of(&new_dir_child);

    let record_count = scan.record_count();
    let mut chunks = 0u64;
    let mut total_live = 0usize;
    let mut cache = DefaultPathCache::new();
    let mut renamed_dir_child_path = None;
    let mut new_dir_child_path = None;
    let mut new_dir_child_seen = false;
    while let Some(chunk) = scan
        .next_chunk()
        .expect("pass 2 must not error out under a volume that changed underneath it")
    {
        chunks += 1;
        assert!(
            chunks <= record_count,
            "pass 2 must not read more chunks than there are records; looks like a loop"
        );
        total_live += chunk.files().count();
        for file in chunk.files() {
            if file.number() == renamed_dir_child_number {
                let name = file.best_name().expect("renamed-dir's child has a name");
                renamed_dir_child_path = Some(chunk.resolve_path(&name, &mut cache));
            } else if file.number() == new_dir_child_number {
                new_dir_child_seen = true;
                let name = file.best_name().expect("new-dir's child has a name");
                new_dir_child_path = Some(chunk.resolve_path(&name, &mut cache));
            }
        }
    }
    assert!(chunks > 0, "pass 2 must read at least one chunk");
    assert!(
        total_live > 0,
        "the scan must still find live files despite the volume changing under it"
    );

    let renamed_dir_child_path = renamed_dir_child_path
        .expect("pass 2 must still find the record kept under renamed-dir's old name");
    let resolved = renamed_dir_child_path.unwrap_or_else(|| {
        panic!("renamed-dir's child must still resolve under the directory's old name")
    });
    assert!(
        resolved.to_string_lossy().contains("renamed-dir")
            && !resolved.to_string_lossy().contains("renamed-dir-new-name"),
        "expected renamed-dir's child to resolve under the OLD directory name, got {resolved:?}"
    );

    // A record created after pass 1 for a file under a directory pass 1 never saw may not even be
    // read by pass 2 (chunk_records=4 keeps the window small, and NTFS may or may not have handed
    // the new file a record inside the range pass 1 already measured); either way is consistent
    // with the docs ("a file under a new directory has no path"), so this only strengthens the
    // check when the record was actually seen.
    if new_dir_child_seen {
        assert_eq!(
            new_dir_child_path,
            Some(None),
            "a file under a directory created after pass 1 must resolve to no path, not a wrong \
             one"
        );
    }
}

// ---- DefaultPathCache::with_max_bytes on a real volume -------------------------------------------

/// `DefaultPathCache::with_max_bytes` at 0, a few bytes, a bound that only fits a handful of
/// entries at a time, and one generous enough to be close to the volume's real working set, gives
/// the same answer as an unbounded cache and as `()` for every live file's every name.
/// `bytes()` never rises above the limit except to keep the one entry the doc comment says
/// eviction never drops to nothing for.
#[test]
fn bounded_path_cache_matches_unbounded_and_none_on_the_test_volume() {
    let _guard = serial();
    let mft = load_mft();
    let live: Vec<_> = mft.files().collect();
    assert!(
        !live.is_empty(),
        "nothing to compare: the test volume looks empty of live files"
    );

    let mut unbounded_probe = DefaultPathCache::new();
    for file in &live {
        for name in file.names() {
            let _ = mft.resolve_path(&name, &mut unbounded_probe);
        }
    }
    let working_set = unbounded_probe.bytes().max(4096);

    for max_bytes in [0usize, 8, 64, working_set] {
        let mut bounded = DefaultPathCache::with_max_bytes(max_bytes);
        let mut unbounded = DefaultPathCache::new();
        for file in &live {
            for name in file.names() {
                let via_bounded = mft.resolve_path(&name, &mut bounded);
                let via_unbounded = mft.resolve_path(&name, &mut unbounded);
                let via_none = mft.resolve_path(&name, &mut ());
                assert_eq!(
                    via_bounded,
                    via_unbounded,
                    "max_bytes={max_bytes}: a bounded cache disagreed with an unbounded one for \
                     file {}",
                    file.number()
                );
                assert_eq!(
                    via_bounded,
                    via_none,
                    "max_bytes={max_bytes}: a bounded cache disagreed with no cache for file {}",
                    file.number()
                );
            }
        }
        assert!(
            bounded.bytes() <= max_bytes || bounded.len() <= 1,
            "max_bytes={max_bytes}: bytes()={} must not exceed the limit while more than one \
             entry is cached (len()={})",
            bounded.bytes(),
            bounded.len()
        );
    }
}
