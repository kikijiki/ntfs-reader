#![cfg(target_os = "windows")]

//! The path of a deleted file, on a real volume. Each test builds a small tree under a fixture
//! directory, records the Win32 path of every entry while live, deletes the tree one way, reloads
//! the `Mft`, and resolves the freed records' names with `Mft::resolve_deleted_path`.
//!
//! Measured on NTFS 3.1: a freed directory keeps its real name, named by its old reference's
//! sequence plus one, when deleted with `remove_dir`. `remove_dir_all`, and a delete that waited
//! on an open handle, rename it to a random name under `$Extend\$Deleted` first. A record reused
//! since is another file.
//!
//! NTFS hands a new file the lowest free record at once, so another test's file can take a
//! just-freed record. Tests here take a lock and run one at a time; nothing else may create files
//! on the volume while they run.
//!
//! `remove_dir` and `remove_dir_all` differ only in what `std` does (in place vs. renamed below
//! `$Extend\$Deleted` first), so every test checks that precondition before checking a path: if
//! `std`'s behavior changes, the failure says so instead of showing a wrong expectation.
//!
//! A test that cannot reach the state it checks prints `SKIPPED: <test>: <reason>` and, under
//! `NTFS_READER_REQUIRE_ALL=1` (set by the maintainer's VM run), fails instead.

use std::fs::{self, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use ntfs_reader::{DeletedPath, DeletedPathCache, FileId, FileInfo, Mft, NtfsFileName, Volume};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
};

mod common;
use common::{
    deleted_directory, describe_record, flush_volume, parents_of, skip, test_volume_letter,
    TempDirGuard,
};

const RECORD_NUMBER_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
/// Directories created to take a freed record before giving up.
const MAX_FILLER_DIRECTORIES: usize = 500;

/// The tests share the volume's list of free records, so they cannot overlap.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---- Volume helpers --------------------------------------------------------------------------

fn fixture_dir(name: &str) -> TempDirGuard {
    TempDirGuard::new(format!("{}:\\deleted-path-{name}", test_volume_letter()))
        .expect("create the fixture directory")
}

/// Flushes the volume and loads its `$MFT`.
fn load() -> Mft {
    let letter = test_volume_letter();
    flush_volume(&letter);
    let volume = Volume::new(format!("\\\\.\\{letter}:")).expect("open the volume");
    Mft::new(volume).expect("load the MFT")
}

/// Loads until `ready` is true, for a volume that has not caught up with what was just done to it
/// (a guard, not an expectation: none was seen stale after a flush). `numbers` are the records
/// waited for; if the MFT never gets there, the failure describes each of them (e.g. a
/// delete-pending file in use under `$Extend\$Deleted`).
fn load_where(what: &str, numbers: &[u64], mut ready: impl FnMut(&Mft) -> bool) -> Mft {
    let mut last = Vec::new();
    for _ in 0..6 {
        let mft = load();
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

/// The file reference (record number plus sequence number) Win32 reports for `path`, which may
/// be a directory.
fn reference_of(path: &Path) -> u64 {
    let file = OpenOptions::new()
        .access_mode(0)
        .share_mode(7)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)
        .unwrap_or_else(|e| panic!("open {path:?}: {e}"));
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
        .unwrap_or_else(|e| panic!("file information of {path:?}: {e}"));
    (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow)
}

fn number_of(reference: u64) -> u64 {
    reference & RECORD_NUMBER_MASK
}

/// Whether record `number` is freed in `mft`.
fn is_freed(mft: &Mft, number: u64) -> bool {
    mft.record(number)
        .is_some_and(|file| !file.is_used() && !mft.is_allocated(number))
}

/// The name of the file `reference` named while it was live, whether that record is live or freed
/// now (`record_by_id` finds both).
fn name_of(mft: &Mft, reference: u64, what: &str) -> NtfsFileName {
    let file = mft
        .record_by_id(FileId::from(reference))
        .unwrap_or_else(|| panic!("{what}: the id {reference:#x} names no record"));
    file.best_name()
        .unwrap_or_else(|| panic!("{what}: record {} kept no name", file.number()))
}

/// `path`, a Win32 path on the test volume (`T:\dir\file`), as the path `Mft::resolve_path` and
/// `resolve_deleted_path` give: under the volume path (`\\.\T:\dir\file`).
fn volume_path(path: &Path) -> PathBuf {
    let text = path.to_str().expect("a UTF-8 path");
    let letter = test_volume_letter();
    assert!(
        text.to_ascii_uppercase()
            .starts_with(&format!("{}:\\", letter.to_ascii_uppercase())),
        "{text} is not on the test volume"
    );
    PathBuf::from(format!("\\\\.\\{letter}:{}", &text[2..]))
}

/// The components of `path` under the volume path.
fn below_volume(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

/// Resolves `name` with a warm cache and with none, which must agree.
fn resolve(mft: &Mft, cache: &mut DeletedPathCache, name: &NtfsFileName) -> DeletedPath {
    let resolved = mft.resolve_deleted_path(name, cache);
    assert_eq!(
        resolved,
        mft.resolve_deleted_path(name, &mut DeletedPathCache::new()),
        "a cache changed the deleted path"
    );
    resolved
}

/// `resolved` is `path`, complete or not.
#[track_caller]
fn assert_path(resolved: &DeletedPath, path: PathBuf, complete: bool, what: &str) {
    assert_eq!(
        (&resolved.path, resolved.complete),
        (&path, complete),
        "{what}"
    );
}

/// Checks that `std`'s deletion did what was measured: freed record `number` has a name under
/// `$Extend\$Deleted` (`renamed`: what `remove_dir_all` does to a directory, and what an open
/// file's deferred delete does) or has not (`remove_dir`/`remove_file` leave the name in place).
/// `parent` is the record the names were in before, and must still hold them when not renamed.
///
/// A record expected to stay in place but found under `$Extend\$Deleted` means the delete was
/// deferred: something else (a virus scanner, the indexer) had the file open, so NTFS moved it
/// aside and freed it later. That says nothing about `std`, so this skips and returns `false`
/// instead of failing. Any other difference panics.
fn deletion_style(
    test: &str,
    mft: &Mft,
    number: u64,
    parent: u64,
    renamed: bool,
    what: &str,
) -> bool {
    let deleted = deleted_directory(mft);
    let parents = parents_of(mft, number);
    let under_deleted = deleted.is_some_and(|deleted| parents.contains(&deleted));
    if under_deleted && !renamed {
        skip(
            test,
            &format!(
                "{what}: the delete was deferred, something held record {number} open: its names \
                 moved below $Extend\\$Deleted (record {deleted:?}) instead of staying under \
                 record {parent}"
            ),
        );
        return false;
    }
    let expected = if renamed {
        "moved below $Extend\\$Deleted"
    } else {
        "left in its directory"
    };
    assert!(
        under_deleted == renamed && (renamed || parents == [parent]),
        "{what}: std's delete was expected to leave record {number} {expected}, but its names \
         sit under record(s) {parents:?}; its directory was record {parent} and \
         $Extend\\$Deleted is record {deleted:?}. std may have changed how it deletes"
    );
    true
}

fn write_file(path: &Path) {
    fs::write(path, vec![b'p'; 3000]).unwrap_or_else(|e| panic!("write {path:?}: {e}"));
}

// ---- Tests -----------------------------------------------------------------------------------

/// The common case: a file deleted from a directory that lives on has the path it had, complete,
/// and `FileInfo` shows it.
#[test]
fn a_file_deleted_from_a_live_directory_has_its_win32_path() {
    let _serial = serial();
    let dir = fixture_dir("live-parent");
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    let path = sub.join("victim.txt");
    write_file(&path);
    let reference = reference_of(&path);
    let number = number_of(reference);
    let parent = number_of(reference_of(&sub));

    fs::remove_file(&path).unwrap();
    let mft = load_where("live parent", &[number], |mft| is_freed(mft, number));
    if !deletion_style(
        "a_file_deleted_from_a_live_directory_has_its_win32_path",
        &mft,
        number,
        parent,
        false,
        "the deleted file",
    ) {
        return;
    }

    let name = name_of(&mft, reference, "live parent");
    let resolved = resolve(&mft, &mut DeletedPathCache::new(), &name);
    assert_path(&resolved, volume_path(&path), true, "the deleted file");

    let file = mft.record(number).expect("the record");
    let info = FileInfo::new(&file);
    assert!(info.is_deleted);
    assert_eq!(info.path, Some(volume_path(&path)));
}

/// A tree deleted leaf-first, one entry at a time with `remove_file` and `remove_dir`: every
/// freed directory keeps its real name, so every entry's path is complete and matches what it
/// had.
#[test]
fn a_tree_deleted_one_entry_at_a_time_has_complete_paths() {
    let _serial = serial();
    let dir = fixture_dir("one-by-one");
    let top = dir.path().join("top");
    let l1 = top.join("l1");
    let l2 = l1.join("l2");
    for directory in [&top, &l1, &l2] {
        fs::create_dir(directory).unwrap();
    }
    let files = [
        top.join("top.txt"),
        l1.join("one.txt"),
        l2.join("two-a.txt"),
        l2.join("two-b.txt"),
    ];
    for file in &files {
        write_file(file);
    }

    let order: Vec<(&Path, bool)> = vec![
        (&files[2], false),
        (&files[3], false),
        (&l2, true),
        (&files[1], false),
        (&l1, true),
        (&files[0], false),
        (&top, true),
    ];
    let references: Vec<u64> = order.iter().map(|(path, _)| reference_of(path)).collect();
    let parents: Vec<u64> = order
        .iter()
        .map(|(path, _)| number_of(reference_of(path.parent().unwrap())))
        .collect();
    for (path, is_directory) in &order {
        if *is_directory {
            fs::remove_dir(path).unwrap();
        } else {
            fs::remove_file(path).unwrap();
        }
    }

    let numbers: Vec<u64> = references.iter().map(|r| number_of(*r)).collect();
    let mft = load_where("one by one", &numbers, |mft| {
        numbers.iter().all(|&number| is_freed(mft, number))
    });

    let mut cache = DeletedPathCache::new();
    for (((path, _), reference), parent) in order.iter().zip(&references).zip(&parents) {
        let what = format!("{path:?}");
        // `remove_dir` deletes a directory where it is, so it keeps its name and its parent.
        if !deletion_style(
            "a_tree_deleted_one_entry_at_a_time_has_complete_paths",
            &mft,
            number_of(*reference),
            *parent,
            false,
            &what,
        ) {
            return;
        }
        let name = name_of(&mft, *reference, &what);
        assert_path(
            &resolve(&mft, &mut cache, &name),
            volume_path(path),
            true,
            &what,
        );
    }
    // A freed directory summarised on its own: complete, so it has a path.
    let file = mft.record(number_of(references[2])).expect("l2");
    assert_eq!(
        FileInfo::new(&file).path,
        Some(volume_path(&l2)),
        "FileInfo of a freed directory"
    );
}

/// The same tree deleted with `remove_dir_all`: every directory is renamed to a random name under
/// `$Extend\$Deleted` before it is deleted, so the resolved path is `<deleted>`, the directory's
/// random name, and the file's own name: incomplete. The files themselves keep their real names.
#[test]
fn a_tree_deleted_with_remove_dir_all_ends_at_deleted() {
    let _serial = serial();
    let dir = fixture_dir("remove-dir-all");
    let top = dir.path().join("top");
    let l1 = top.join("l1");
    for directory in [&top, &l1] {
        fs::create_dir_all(directory).unwrap();
    }
    let files = [top.join("top.txt"), l1.join("one.txt"), l1.join("two.txt")];
    for file in &files {
        write_file(file);
    }
    let file_references: Vec<u64> = files.iter().map(|file| reference_of(file)).collect();
    let directory_references: Vec<u64> = [&top, &l1]
        .iter()
        .map(|directory| reference_of(directory))
        .collect();
    let top_number = number_of(directory_references[0]);
    let file_parents = [
        top_number,
        number_of(directory_references[1]),
        number_of(directory_references[1]),
    ];
    let directory_parents = [number_of(reference_of(dir.path())), top_number];

    fs::remove_dir_all(&top).unwrap();

    let numbers: Vec<u64> = file_references
        .iter()
        .chain(&directory_references)
        .map(|r| number_of(*r))
        .collect();
    let mft = load_where("remove_dir_all", &numbers, |mft| {
        numbers.iter().all(|&number| is_freed(mft, number))
    });

    let mut cache = DeletedPathCache::new();
    for ((file, reference), parent) in files.iter().zip(&file_references).zip(file_parents) {
        let what = format!("{file:?}");
        // The files are deleted in place, only their directories are renamed.
        if !deletion_style(
            "a_tree_deleted_with_remove_dir_all_ends_at_deleted",
            &mft,
            number_of(*reference),
            parent,
            false,
            &what,
        ) {
            return;
        }
        let name = name_of(&mft, *reference, &what);
        let resolved = resolve(&mft, &mut cache, &name);
        assert!(!resolved.complete, "{what}");
        let parts = below_volume(&resolved.path);
        let real_directory = file
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy();
        assert_eq!(parts.len(), 3, "{what}: {:?}", resolved.path);
        assert_eq!(parts[0], "<deleted>", "{what}");
        assert_ne!(parts[1], real_directory, "{what}: the real name is gone");
        assert_eq!(
            parts[2],
            file.file_name().unwrap().to_string_lossy(),
            "{what}"
        );
        assert_eq!(
            FileInfo::new(&mft.record(number_of(*reference)).unwrap()).path,
            None,
            "{what}: FileInfo has no path for an incomplete one"
        );
    }
    // The directories themselves: `<deleted>` and the random name.
    for ((directory, reference), parent) in [&top, &l1]
        .iter()
        .zip(&directory_references)
        .zip(directory_parents)
    {
        let what = format!("{directory:?}");
        // `remove_dir_all` renames each directory below `$Extend\$Deleted` before it deletes it.
        // A renamed directory is never a skip: this either holds or panics.
        deletion_style(
            "a_tree_deleted_with_remove_dir_all_ends_at_deleted",
            &mft,
            number_of(*reference),
            parent,
            true,
            &what,
        );
        let name = name_of(&mft, *reference, &what);
        let resolved = resolve(&mft, &mut cache, &name);
        assert!(!resolved.complete, "{what}");
        let parts = below_volume(&resolved.path);
        assert_eq!(parts.len(), 2, "{what}: {:?}", resolved.path);
        assert_eq!(parts[0], "<deleted>", "{what}");
    }
}

/// Creates directories in `dir` until one takes record `number`, which NTFS gives out lowest
/// first. Stops there, so the records above it stay freed. `false` when none did in
/// `MAX_FILLER_DIRECTORIES` tries.
fn take_record(dir: &Path, number: u64) -> bool {
    (0..MAX_FILLER_DIRECTORIES).any(|index| {
        let filler = dir.join(format!("filler-{index}"));
        fs::create_dir(&filler).unwrap();
        number_of(reference_of(&filler)) == number
    })
}

/// A directory record another directory reuses after the delete is not the directory the child's
/// parent reference names (the sequence is the old one plus one, now in use): the chain breaks
/// there with `<lost N>`, and what is below it is kept.
#[test]
fn a_reused_directory_record_breaks_the_path_at_lost() {
    let _serial = serial();
    let dir = fixture_dir("reused");
    let top = dir.path().join("top");
    let mid = top.join("mid");
    fs::create_dir_all(&mid).unwrap();
    let file = mid.join("victim.txt");
    write_file(&file);
    let top_reference = reference_of(&top);
    let mid_reference = reference_of(&mid);
    let file_reference = reference_of(&file);
    let top_number = number_of(top_reference);
    assert!(
        top_number < number_of(mid_reference)
            && number_of(mid_reference) < number_of(file_reference),
        "the records were not handed out in order"
    );

    fs::remove_file(&file).unwrap();
    fs::remove_dir(&mid).unwrap();
    fs::remove_dir(&top).unwrap();
    // Only `top`'s record is taken, so `mid` and the file stay freed.
    if !take_record(dir.path(), top_number) {
        skip(
            "a_reused_directory_record_breaks_the_path_at_lost",
            &format!(
                "no new directory took record {top_number} in {MAX_FILLER_DIRECTORIES} tries \
                 (the ordering of free records is not what the test assumes)"
            ),
        );
        return;
    }

    let mft = load_where(
        "reused",
        &[
            number_of(mid_reference),
            number_of(file_reference),
            top_number,
        ],
        |mft| {
            is_freed(mft, number_of(mid_reference))
                && is_freed(mft, number_of(file_reference))
                && mft
                    .record(top_number)
                    .is_some_and(|record| record.is_used() && record.reference() != top_reference)
        },
    );
    let reused = mft.record(top_number).expect("the reused record");
    assert_eq!(
        reused.reference() >> 48,
        (top_reference >> 48) + 1,
        "reuse gives the freed record's sequence, which is one above the old one"
    );

    // Deleted one entry at a time, so `mid` and the file kept their names and their directories.
    const NAME: &str = "a_reused_directory_record_breaks_the_path_at_lost";
    if !deletion_style(
        NAME,
        &mft,
        number_of(mid_reference),
        top_number,
        false,
        "mid",
    ) {
        return;
    }
    if !deletion_style(
        NAME,
        &mft,
        number_of(file_reference),
        number_of(mid_reference),
        false,
        "victim.txt",
    ) {
        return;
    }

    let mut cache = DeletedPathCache::new();
    let name = name_of(&mft, file_reference, "victim");
    let resolved = resolve(&mft, &mut cache, &name);
    assert_path(
        &resolved,
        PathBuf::from(format!("\\\\.\\{}:", test_volume_letter()))
            .join(format!("<lost {top_number}>"))
            .join("mid")
            .join("victim.txt"),
        false,
        "below the reused directory",
    );
}

/// A file deleted while another handle keeps it open (delete-pending): the name is gone at once
/// and the record stays in use, under a random name below `$Extend\$Deleted`, so it is not in
/// `deleted_files()`. Once the handle closes, the record is freed and its path breaks at
/// `<deleted>`: the original name and directory are lost for good.
#[test]
fn a_file_deleted_while_open_breaks_at_deleted_once_it_is_freed() {
    let _serial = serial();
    let dir = fixture_dir("pending");
    let path = dir.path().join("pending.bin");
    write_file(&path);
    let reference = reference_of(&path);
    let number = number_of(reference);

    let parent = number_of(reference_of(dir.path()));

    let handle = OpenOptions::new()
        .read(true)
        .share_mode(7)
        .open(&path)
        .expect("open the file with FILE_SHARE_DELETE");
    fs::remove_file(&path).unwrap();

    let pending = load();
    let record = pending.record(number).expect("the record");
    assert!(
        record.is_used(),
        "the record must stay in use while open: {}",
        describe_record(&pending, number)
    );
    // The name moved below `$Extend\$Deleted` at once, and so it does once the record is freed.
    deletion_style(
        "a_file_deleted_while_open_breaks_at_deleted_once_it_is_freed",
        &pending,
        number,
        parent,
        true,
        "the file while it is open",
    );
    assert!(pending.deleted_files().all(|f| f.number() != number));
    let name = name_of(&pending, reference, "pending");
    let random = name.to_string();
    assert_ne!(
        random, "pending.bin",
        "the original name is lost at the rename"
    );
    let while_open = resolve(&pending, &mut DeletedPathCache::new(), &name);
    assert!(!while_open.complete);
    assert_eq!(
        below_volume(&while_open.path),
        ["<deleted>".to_string(), random.clone()]
    );

    drop(handle);
    let mft = load_where("pending", &[number], |mft| is_freed(mft, number));
    deletion_style(
        "a_file_deleted_while_open_breaks_at_deleted_once_it_is_freed",
        &mft,
        number,
        parent,
        true,
        "the freed file",
    );
    assert!(mft.deleted_files().any(|f| f.number() == number));
    let name = name_of(&mft, reference, "freed");
    assert_eq!(
        name.to_string(),
        random,
        "the freed record keeps the random name"
    );
    let resolved = resolve(&mft, &mut DeletedPathCache::new(), &name);
    assert!(!resolved.complete);
    assert_eq!(
        below_volume(&resolved.path),
        ["<deleted>".to_string(), random]
    );
    let info = FileInfo::new(&mft.record(number).expect("the record"));
    assert!(info.is_deleted);
    assert_eq!(info.path, None);
}
