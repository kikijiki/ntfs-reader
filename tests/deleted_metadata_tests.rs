#![cfg(target_os = "windows")]

//! What a deleted file's record still says, on a real volume. Each test makes a file of one
//! shape, records its `FileInfo` while live, deletes it through Win32, reloads the `Mft`, and
//! checks the freed record is in `deleted_files()` exactly once, its old id still names it
//! (`record_by_id`), and its `FileInfo` (name, size, times, attributes, directory bit) is
//! unchanged. The path is not compared; that is deleted path resolution's job.
//!
//! Measured on NTFS 3.1: a delete keeps everything but the in-use flag, the sequence number
//! (plus one), and the log sequence number, so names, times, and sizes survive, except for a
//! file that had an `$ATTRIBUTE_LIST`.
//!
//! NTFS hands a new file the lowest free record at once, so another test's file can take a
//! just-freed record. Tests here take a lock and run one at a time; nothing else may create files
//! on the volume while they run.
//!
//! A test that cannot reach the state it checks prints `SKIPPED: <test>: <reason>` and, under
//! `NTFS_READER_REQUIRE_ALL=1` (set by the maintainer's VM run), fails instead.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use ntfs_reader::{FileId, FileInfo, Mft, NtfsFile, Volume, FIRST_NORMAL_RECORD};
use time::OffsetDateTime;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_HANDLE_EOF, HANDLE};
use windows::Win32::Storage::FileSystem::{
    FindClose, FindFirstFileNameW, FindNextFileNameW, GetFileInformationByHandle,
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
};
use windows::Win32::System::Ioctl::FSCTL_SET_SPARSE;
use windows::Win32::System::IO::DeviceIoControl;

mod common;
use common::{describe_record, flush_volume, skip, test_volume_letter, TempDirGuard};

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
const RECORD_NUMBER_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
/// Files created beyond the free records below the target before `take_record` gives up.
const FILLER_MARGIN: u64 = 64;
/// The most files `take_record` creates, whatever the number of free records below the target.
const MAX_FILLER_FILES: u64 = 20_000;

/// The tests share the volume's list of free records, so they cannot overlap.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---- Volume helpers --------------------------------------------------------------------------

fn fixture_dir(name: &str) -> TempDirGuard {
    TempDirGuard::new(format!("{}:\\deleted-meta-{name}", test_volume_letter()))
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

fn sequence_of(reference: u64) -> u16 {
    (reference >> 48) as u16
}

/// How many names Win32 lists for `path` (`FindFirstFileNameW`): the hard links, never a DOS alias.
fn win32_link_count(path: &Path) -> usize {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut buffer = vec![0u16; 2048];
    let mut length = buffer.len() as u32;
    let mut count = 0;
    unsafe {
        let find = FindFirstFileNameW(
            PCWSTR(wide.as_ptr()),
            0,
            &mut length,
            PWSTR(buffer.as_mut_ptr()),
        )
        .unwrap_or_else(|e| panic!("FindFirstFileNameW {path:?}: {e}"));
        count += 1;
        loop {
            length = buffer.len() as u32;
            match FindNextFileNameW(find, &mut length, PWSTR(buffer.as_mut_ptr())) {
                Ok(()) => count += 1,
                Err(e) if e.code() == ERROR_HANDLE_EOF.to_hresult() => break,
                Err(e) => panic!("FindNextFileNameW {path:?}: {e}"),
            }
        }
        let _ = FindClose(find);
    }
    count
}

fn mark_sparse(file: &File) {
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            HANDLE(file.as_raw_handle()),
            FSCTL_SET_SPARSE,
            None,
            0,
            None,
            0,
            Some(&mut returned as *mut u32),
            None,
        )
    }
    .expect("FSCTL_SET_SPARSE");
}

/// The times and attributes of a file that a delete leaves alone, `mft_modified` excluded.
fn times(file: &NtfsFile) -> Option<(OffsetDateTime, OffsetDateTime, OffsetDateTime, u32)> {
    let info = file.standard_information()?;
    Some((
        info.created(),
        info.modified(),
        info.accessed(),
        info.file_attributes(),
    ))
}

fn pattern(size: usize) -> Vec<u8> {
    (0..size).map(|index| (index % 251) as u8).collect()
}

/// Writes `size` bytes of a recognisable pattern.
fn write_file(path: &Path, size: usize) {
    fs::write(path, pattern(size)).unwrap_or_else(|e| panic!("write {path:?}: {e}"));
}

// ---- What is compared ------------------------------------------------------------------------

/// What must survive a delete of a file whose record is intact: everything of a `FileInfo` but
/// its path.
#[derive(Debug, PartialEq, Eq)]
struct Meta {
    name: String,
    size: u64,
    is_directory: bool,
    file_attributes: u32,
    created: Option<OffsetDateTime>,
    accessed: Option<OffsetDateTime>,
    modified: Option<OffsetDateTime>,
}

impl Meta {
    fn of(file: &NtfsFile) -> Self {
        let info = FileInfo::new(file);
        Meta {
            name: info.name,
            size: info.size,
            is_directory: info.is_directory,
            file_attributes: info.file_attributes,
            created: info.created,
            accessed: info.accessed,
            modified: info.modified,
        }
    }
}

/// The live file with record number `number`: loaded, checked to be live and listed by `files()`.
fn live_meta(mft: &Mft, number: u64) -> Meta {
    let file = mft
        .record(number)
        .unwrap_or_else(|| panic!("record {number} does not hold a file"));
    assert!(
        file.is_used(),
        "record {number} is not in use before the delete"
    );
    assert!(
        mft.files().any(|f| f.number() == number),
        "record {number} is not in files() before the delete"
    );
    Meta::of(&file)
}

/// Whether record `number` is freed in `mft`.
fn is_freed(mft: &Mft, number: u64) -> bool {
    mft.record(number)
        .is_some_and(|file| !file.is_used() && !mft.is_allocated(number))
}

/// Checks the freed record `number`, which was `reference` while live, against what it was.
fn check_deleted(mft: &Mft, number: u64, reference: u64, before: &Meta, what: &str) {
    let file = mft
        .record(number)
        .unwrap_or_else(|| panic!("{what}: record {number} is gone"));
    assert!(!file.is_used(), "{what}: record {number} is still in use");
    assert!(
        !mft.files().any(|f| f.number() == number),
        "{what}: record {number} is still listed by files()"
    );
    let listed = mft.deleted_files().filter(|f| f.number() == number).count();
    assert_eq!(listed, 1, "{what}: record {number} in deleted_files()");

    let named = mft
        .record_by_id(FileId::from(reference))
        .unwrap_or_else(|| panic!("{what}: the id {reference:#x} the file had names no record"));
    assert_eq!(named.number(), number, "{what}: record_by_id");
    assert_eq!(
        file.reference(),
        reference + (1 << 48),
        "{what}: freeing adds one to the sequence number"
    );
    assert_eq!(
        file.file_id(),
        FileId::from(reference),
        "{what}: file_id() is the id the file had while it was live"
    );
    assert!(file.is_deleted(), "{what}: is_deleted");

    assert_eq!(&Meta::of(&file), before, "{what}: FileInfo");
}

/// The whole cycle for one file or directory: its `FileInfo` while live, then `delete` (which
/// removes it through Win32), a reload and the checks of [`check_deleted`].
fn delete_and_check(what: &str, path: &Path, delete: impl FnOnce()) -> Mft {
    let reference = reference_of(path);
    let number = number_of(reference);
    let before = live_meta(&load(), number);
    delete();
    let mft = load_where(what, &[number], |mft| is_freed(mft, number));
    check_deleted(&mft, number, reference, &before, what);
    mft
}

// ---- Shapes ----------------------------------------------------------------------------------

#[test]
fn a_deleted_resident_file_keeps_its_metadata() {
    let _serial = serial();
    let dir = fixture_dir("resident");
    let path = dir.path().join("resident-100.txt");
    write_file(&path, 100);
    let number = number_of(reference_of(&path));
    let mft = delete_and_check("resident", &path, || fs::remove_file(&path).unwrap());
    // What the record holds is what the user gets back: the 100 bytes.
    let file = mft
        .deleted_files()
        .find(|file| file.number() == number)
        .expect("the file is in deleted_files()");
    assert_eq!(file.resident_data(), Some(pattern(100).as_slice()));
    let mut stream = file.open_stream(None).expect("open the deleted stream");
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, pattern(100));
}

#[test]
fn a_deleted_contiguous_file_keeps_its_metadata() {
    let _serial = serial();
    let dir = fixture_dir("contiguous");
    let path = dir.path().join("contiguous-1mib.bin");
    write_file(&path, MIB);
    delete_and_check("contiguous", &path, || fs::remove_file(&path).unwrap());
}

#[test]
fn a_deleted_fragmented_file_keeps_its_metadata() {
    let _serial = serial();
    let dir = fixture_dir("fragmented");
    let path = dir.path().join("fragmented-a.bin");
    let other = dir.path().join("interleaved-b.bin");
    // Two files written a piece at a time, each flushed, so the allocator interleaves them.
    {
        let mut a = File::create(&path).unwrap();
        let mut b = File::create(&other).unwrap();
        let piece = pattern(64 * KIB);
        for _ in 0..(12 * MIB / (64 * KIB)) {
            a.write_all(&piece).unwrap();
            a.sync_all().unwrap();
            b.write_all(&piece).unwrap();
            b.sync_all().unwrap();
        }
    }
    fs::remove_file(&other).unwrap();
    delete_and_check("fragmented", &path, || fs::remove_file(&path).unwrap());
}

#[test]
fn a_deleted_sparse_file_keeps_its_metadata() {
    let _serial = serial();
    let dir = fixture_dir("sparse");
    let path = dir.path().join("sparse.bin");
    {
        let mut file = File::create(&path).unwrap();
        mark_sparse(&file);
        file.write_all(&pattern(64 * KIB)).unwrap();
        file.seek(SeekFrom::Start(64 * MIB as u64)).unwrap();
        file.write_all(&pattern(64 * KIB)).unwrap();
        file.set_len(100 * MIB as u64).unwrap();
        file.sync_all().unwrap();
    }
    delete_and_check("sparse", &path, || fs::remove_file(&path).unwrap());
}

#[test]
fn a_deleted_file_with_alternate_streams_keeps_them() {
    let _serial = serial();
    let dir = fixture_dir("streams");
    let path = dir.path().join("streams.txt");
    fs::write(&path, b"main stream").unwrap();
    let stream = |name: &str| PathBuf::from(format!("{}:{name}", path.display()));
    write_file(&stream("small1"), 50);
    write_file(&stream("small2"), 300);
    write_file(&stream("small3"), 0);
    write_file(&stream("large1"), 200 * KIB);
    write_file(&stream("large2"), MIB);

    let reference = reference_of(&path);
    let number = number_of(reference);
    let mft = load();
    let live = mft.record(number).expect("the file");
    let streams_before: Vec<_> = live
        .data_streams()
        .map(|stream| (stream.name, stream.size))
        .collect();
    assert_eq!(streams_before.len(), 6, "{streams_before:?}");
    let before = Meta::of(&live);
    drop(mft);

    fs::remove_file(&path).unwrap();

    let mft = load_where("streams", &[number], |mft| is_freed(mft, number));
    check_deleted(&mft, number, reference, &before, "streams");
    let streams_after: Vec<_> = mft
        .record(number)
        .expect("the record")
        .data_streams()
        .map(|stream| (stream.name, stream.size))
        .collect();
    assert_eq!(streams_after, streams_before);
}

/// A tree deleted leaf-first, one entry at a time: `remove_dir_all` renames every directory
/// before deleting it (the freed record would keep the random name), and a directory's times
/// change when its last child goes, so its `FileInfo` is taken just before its own delete.
#[test]
fn a_deleted_directory_tree_keeps_its_metadata() {
    let _serial = serial();
    let dir = fixture_dir("tree");
    let top = dir.path().join("top");
    let l1 = top.join("l1");
    let l2 = l1.join("l2");
    let l3 = l2.join("l3");
    for directory in [&top, &l1, &l2, &l3] {
        fs::create_dir(directory).unwrap();
    }
    let files = [
        top.join("top.txt"),
        l1.join("one.txt"),
        l2.join("two.txt"),
        l3.join("three-a.txt"),
        l3.join("three-b.txt"),
    ];
    for (index, file) in files.iter().enumerate() {
        write_file(file, 100 + index * 1000);
    }

    // Leaf to root: each directory after everything in it.
    let order: Vec<(&Path, bool)> = vec![
        (&files[3], false),
        (&files[4], false),
        (&l3, true),
        (&files[2], false),
        (&l2, true),
        (&files[1], false),
        (&l1, true),
        (&files[0], false),
        (&top, true),
    ];
    let references: Vec<u64> = order.iter().map(|(path, _)| reference_of(path)).collect();

    let mut before = Vec::new();
    for ((path, is_directory), reference) in order.iter().zip(&references) {
        before.push(live_meta(&load(), number_of(*reference)));
        if *is_directory {
            fs::remove_dir(path).unwrap();
        } else {
            fs::remove_file(path).unwrap();
        }
    }

    let numbers: Vec<u64> = references
        .iter()
        .map(|reference| number_of(*reference))
        .collect();
    let mft = load_where("tree", &numbers, |mft| {
        numbers.iter().all(|&n| is_freed(mft, n))
    });
    for (((path, is_directory), reference), meta) in order.iter().zip(&references).zip(&before) {
        let what = format!("{path:?}");
        check_deleted(&mft, number_of(*reference), *reference, meta, &what);
        // A freed directory is a directory still.
        let file = mft.record(number_of(*reference)).expect("the record");
        assert_eq!(file.is_directory(), *is_directory, "{what}");
    }
}

// ---- The shape that loses its run information --------------------------------------------

const STREAMS: usize = 30;
const LINKS: usize = 30;

/// A file with `STREAMS` alternate streams and `LINKS` hard links with long names: enough
/// attributes to need an `$ATTRIBUTE_LIST` and extension records. Returns the file and its links.
fn make_many_attributes(dir: &Path) -> (PathBuf, Vec<PathBuf>) {
    let target = dir.join("target.bin");
    write_file(&target, 64 * KIB);
    for index in 0..STREAMS {
        fs::write(
            PathBuf::from(format!("{}:stream{index:02}", target.display())),
            vec![b'x'; 100],
        )
        .unwrap();
    }
    let links = (0..LINKS)
        .map(|index| {
            let link = dir.join(format!("hardlink-{index:02}-{}", "n".repeat(100)));
            fs::hard_link(&target, &link).unwrap();
            link
        })
        .collect();
    (target, links)
}

/// Deletes the file with all its links, the last one being `links[0]`.
fn delete_all_links(target: &Path, links: &[PathBuf]) {
    fs::remove_file(target).unwrap();
    for link in links.iter().skip(1) {
        fs::remove_file(link).unwrap();
    }
    fs::remove_file(&links[0]).unwrap();
}

/// A file with an `$ATTRIBUTE_LIST` (many streams and links) that is deleted. NTFS was measured
/// to truncate every non-resident attribute of such a file (size 0, no runs), keep the resident
/// ones, and leave the base record with no name of its own: the last link removed keeps its name
/// in an extension record. What a user gets: the times and attributes as they were (`mft_modified`
/// moves, since truncation is a write), at least one name, size 0 for the default stream, readable
/// resident streams, and `stream_data_lost` saying the size is not the file's.
#[test]
fn a_deleted_file_with_an_attribute_list_is_listed_and_reports_what_its_record_holds() {
    let _serial = serial();
    let dir = fixture_dir("attribute-list");
    let (target, links) = make_many_attributes(dir.path());
    let reference = reference_of(&target);
    let number = number_of(reference);

    let live = load();
    let file = live.record(number).expect("the file");
    let live_records = file.records().count();
    assert!(
        live_records > 1,
        "the fixture did not need extension records"
    );
    assert_eq!(file.hard_links().count(), LINKS + 1);
    let live_streams = file.data_streams().count();
    assert_eq!(live_streams, STREAMS + 1);
    let live_size = FileInfo::new(&file).size;
    assert_eq!(live_size, (64 * KIB) as u64);
    assert!(!file.stream_data_lost(), "a live file has all its data");
    let live_times = times(&file);
    assert!(
        live_times.is_some(),
        "the live file has no standard information"
    );
    drop(live);

    delete_all_links(&target, &links);

    let mft = load_where("attribute list", &[number], |mft| is_freed(mft, number));
    let listed = mft.deleted_files().filter(|f| f.number() == number).count();
    assert_eq!(
        listed, 1,
        "the deleted file is in deleted_files() exactly once"
    );
    let named = mft
        .record_by_id(FileId::from(reference))
        .expect("its old id");
    assert_eq!(named.number(), number);

    let file = mft.record(number).expect("the record");
    assert!(!file.is_used());
    assert!(
        file.records().count() > 1,
        "the deleted file lost its extension records: {} record(s), {live_records} while live",
        file.records().count()
    );
    // The created, modified and accessed times and attributes are what they were. `mft_modified`
    // is left out on purpose: for this shape the delete truncates the streams, moving it (about
    // 50 ms later, measured on the VM), while a plain delete does not touch it.
    assert_eq!(times(&file), live_times);
    // Only the last name removed survives: removing a link empties the extension record that
    // held only that name.
    let names: Vec<String> = file.names().map(|name| name.to_string()).collect();
    assert!(!names.is_empty(), "the deleted file has no name at all");
    assert!(
        names.iter().all(|name| name.starts_with("hardlink-")),
        "a name that is none of the file's: {names:?}"
    );
    assert!(
        file.stream_data_lost(),
        "the file lost its non-resident data and does not say so"
    );

    // Size 0 is what the record holds, and `stream_data_lost` says it is not the file's size.
    let info = FileInfo::new(&file);
    eprintln!(
        "attribute list shape: {} of {live_records} records, {} names, {} of {live_streams} \
         streams, default stream size {} (live {live_size})",
        file.records().count(),
        names.len(),
        file.data_streams().count(),
        info.size,
    );
    assert_eq!(info.size, 0, "the live size was {live_size}");
    let streams: Vec<_> = file.data_streams().collect();
    let default = streams
        .iter()
        .find(|stream| stream.name.is_none())
        .expect("the default stream is still listed");
    assert_eq!(default.size, 0);
    // The named streams are 100 bytes of resident data, kept, or a truncated non-resident one.
    let mut resident = 0;
    for stream in streams.iter().filter(|stream| stream.name.is_some()) {
        let name = stream.name.as_ref().unwrap();
        assert!(
            stream.size == 0 || stream.size == 100,
            "stream {name:?} has size {}, which it never had",
            stream.size
        );
        if stream.size == 100 {
            resident += 1;
            let mut reader = file.open_stream(Some(name)).unwrap();
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, vec![b'x'; 100], "stream {name:?}");
        }
    }
    assert!(resident > 0, "no resident stream survived: {streams:?}");
}

// ---- Reuse -----------------------------------------------------------------------------------

/// The free record numbers of the volume from `FIRST_NORMAL_RECORD` up to `limit`, from a fresh
/// load. Records 16 to 23 are reserved (free in the bitmap, but NTFS never hands them out) and
/// are excluded.
fn free_records_below(limit: u64) -> Vec<u64> {
    let mft = load();
    (FIRST_NORMAL_RECORD..limit)
        .filter(|&n| !mft.is_allocated(n))
        .collect()
}

/// Creates small files in `dir` until one takes record `number`, which NTFS is expected to hand
/// out lowest-free-first (so every free record below it goes first). Returns that file, or a
/// description of how it went if the record was not reached.
///
/// Bounded by the free records below `number` in a fresh load's bitmap, plus `FILLER_MARGIN`
/// (earlier tests leave many), never more than `MAX_FILLER_FILES`. Each new file's record is
/// remembered; if the target is not reached, the description says whether NTFS handed records out
/// lowest-first (a free record below the target still free afterward, or non-rising numbers, is a
/// finding about NTFS, not the crate).
fn take_record(dir: &Path, number: u64) -> Result<PathBuf, String> {
    let free = free_records_below(number);
    let limit = (free.len() as u64 + FILLER_MARGIN).min(MAX_FILLER_FILES);
    let mut handed_out = Vec::new();
    for index in 0..limit {
        let path = dir.join(format!("filler-{index:05}.txt"));
        fs::write(&path, b"filler").unwrap();
        let got = number_of(reference_of(&path));
        if got == number {
            return Ok(path);
        }
        handed_out.push(got);
    }
    let still_free = free_records_below(number);
    // Who has the target and the record after it, when the target is not ours.
    let fresh = load();
    let occupants = format!(
        "{}; {}",
        describe_record(&fresh, number),
        describe_record(&fresh, number + 1)
    );
    let rising = handed_out.windows(2).all(|pair| pair[0] < pair[1]);
    let shown = |numbers: &[u64]| format!("{:?}", &numbers[..numbers.len().min(20)]);
    Err(format!(
        "record {number} was not reused after {limit} new files. Free records below it before: {} \
         (first 20 {}); still free after: {} (first 20 {}). Records the new files got, first 20 {}, \
         last 10 {:?}: {}. NTFS {} hand out the lowest free record first. Occupants: {occupants}",
        free.len(),
        shown(&free),
        still_free.len(),
        shown(&still_free),
        shown(&handed_out),
        &handed_out[handed_out.len().saturating_sub(10)..],
        if rising { "rising" } else { "not rising" },
        if still_free.is_empty() && rising {
            "did seem to"
        } else {
            "did NOT"
        },
    ))
}

/// [`take_record`], or a skip (a failure under `NTFS_READER_REQUIRE_ALL=1`) that says how it went.
fn take_record_or_skip(test: &str, dir: &Path, number: u64) -> Option<PathBuf> {
    match take_record(dir, number) {
        Ok(path) => Some(path),
        Err(how) => {
            skip(test, &how);
            None
        }
    }
}

/// The record of a deleted file is reused by the next file that needs one. From then on the old
/// file is out of `deleted_files()`, its old id names nothing, and the new file (starting at the
/// freed record's sequence) owns the record.
#[test]
fn a_reused_record_is_no_longer_the_deleted_file() {
    let _serial = serial();
    let dir = fixture_dir("reuse");
    let old = dir.path().join("old.txt");
    write_file(&old, 100);
    let old_reference = reference_of(&old);
    let number = number_of(old_reference);
    fs::remove_file(&old).unwrap();

    let mft = load_where("reuse, before", &[number], |mft| is_freed(mft, number));
    assert_eq!(
        mft.deleted_files().filter(|f| f.number() == number).count(),
        1
    );
    assert!(mft.record_by_id(FileId::from(old_reference)).is_some());

    let Some(new) = take_record_or_skip(
        "a_reused_record_is_no_longer_the_deleted_file",
        dir.path(),
        number,
    ) else {
        return;
    };
    drop(mft);
    let new_reference = reference_of(&new);
    assert_eq!(
        sequence_of(new_reference),
        sequence_of(old_reference).wrapping_add(1),
        "a reused record starts at the sequence its freed self had"
    );

    let mft = load_where("reuse, after", &[number], |mft| {
        mft.record(number).is_some_and(|file| file.is_used())
    });
    assert!(
        !mft.deleted_files().any(|f| f.number() == number),
        "the reused record is still listed as deleted"
    );
    assert!(
        mft.record_by_id(FileId::from(old_reference)).is_none(),
        "the id of the deleted file names the file that replaced it"
    );
    let live = mft
        .record_by_id(FileId::from(new_reference))
        .expect("the new file");
    assert!(live.is_used());
    assert_eq!(live.number(), number);
}

/// Regression guard for the 0.4.5 bug, on a real volume: a live file must not pick up the freed
/// extension records a deleted file left next to the record it reused. The base record of a file
/// with an `$ATTRIBUTE_LIST` is reused by a new file, which then gets hard links of its own: the
/// crate must report exactly the links Win32 does, and none of its records may be a freed one.
/// This only proves something while freed extension records of the old file sit next to the new
/// one; when the volume's free records happen not to leave any, the test skips.
#[test]
fn a_live_file_in_a_reused_record_ignores_the_freed_extensions_left_there() {
    let _serial = serial();
    let dir = fixture_dir("reuse-live");
    let old_dir = dir.path().join("old");
    fs::create_dir(&old_dir).unwrap();
    let (target, links) = make_many_attributes(&old_dir);
    let number = number_of(reference_of(&target));
    // Every directory the test needs is made before the delete: the freed record is reused at once
    // by the next new file or directory (seen: a `new` directory made here took the record, and a
    // directory cannot get hard links). After the delete, only fillers create anything; the one
    // that lands on `number` is this test's live file.
    let new_dir = dir.path().join("new");
    fs::create_dir(&new_dir).unwrap();
    delete_all_links(&target, &links);
    let freed_mft = load_where("reuse, live, before", &[number], |mft| {
        is_freed(mft, number)
    });

    let Some(file) = take_record_or_skip(
        "a_live_file_in_a_reused_record_ignores_the_freed_extensions_left_there",
        &new_dir,
        number,
    ) else {
        return;
    };
    drop(freed_mft);
    assert_eq!(
        number_of(reference_of(&file)),
        number,
        "the file that took the freed record is the live file under test"
    );
    let first_link = new_dir.join("link-1.txt");
    let second_link = new_dir.join("link-2.txt");
    fs::hard_link(&file, &first_link).unwrap();
    fs::hard_link(&file, &second_link).unwrap();
    let win32_links = win32_link_count(&file);
    assert_eq!(win32_links, 3);

    let mft = load_where("reuse, live", &[number], |mft| {
        mft.record(number).is_some_and(|file| file.is_used())
    });
    let live = mft.record(number).expect("the live file");
    assert!(live.is_used());
    assert_eq!(live.hard_links().count(), win32_links);
    assert_eq!(
        live.names().filter(|name| !name.is_dos_alias()).count(),
        win32_links
    );
    for record in live.records() {
        assert!(
            record.is_used(),
            "record {} of the live file is freed",
            record.number()
        );
    }
    // Three short names fit the base record, so any further record is one of the old file's.
    assert_eq!(
        live.records().count(),
        1,
        "the live file has records besides its own: {:?}",
        live.records()
            .map(|record| record.number())
            .collect::<Vec<_>>()
    );

    // The fixture is only meaningful if the old file's extension records are still there.
    let leftovers = (0..mft.record_count())
        .filter_map(|n| mft.record(n))
        .filter(|record| {
            !record.is_used() && record.is_extension() && record.base_number() == Some(number)
        })
        .count();
    if leftovers == 0 {
        // Nothing to ignore: the check would prove nothing. This depends on which free records the
        // volume hands out first, not on the crate.
        skip(
            "a_live_file_in_a_reused_record_ignores_the_freed_extensions_left_there",
            &format!("no freed extension record of record {number} is left"),
        );
    }
}

// ---- A freed extension record that was freed while its base lived -----------------------------

/// `(record number, in use, attribute types, attribute count)` of the records of the file at
/// `number` other than its base, as they are in `mft`: the extension records the base's
/// `$ATTRIBUTE_LIST` points at, whether or not the file still lists them.
fn extension_states(mft: &Mft, number: u64) -> Vec<(u64, bool, Vec<String>)> {
    (FIRST_NORMAL_RECORD..mft.record_count())
        .filter_map(|n| mft.record(n))
        .filter(|record| record.is_extension() && record.base_number() == Some(number))
        .map(|record| {
            let types = record
                .record_attributes()
                .map(|attribute| format!("{:?}", attribute.attribute_type()))
                .collect();
            (record.number(), record.is_used(), types)
        })
        .collect()
}

// The deleted-file rules attribute a freed extension record to a deleted file when its base
// reference names the file's incarnation, assuming one freed while its base was alive is EMPTY
// (measured only for records that held a name: unlinking a name empties the record). Records that
// held `$DATA` runs were not measured. Here a fragmented default stream's runs sit in extension
// records; the file is shrunk to one block, so NTFS frees those extension records while the base
// still lives, then the file is deleted. What the records hold afterward is PRINTED (`MEASURED`).
// Asserted is only what a user needs: the deleted file must not report the stale runs as its
// data, so its default stream is either flagged lost or no bigger than it was when deleted.
#[test]
fn a_deleted_file_shrunk_after_its_runs_moved_to_extension_records_does_not_report_them() {
    const NAME: &str =
        "a_deleted_file_shrunk_after_its_runs_moved_to_extension_records_does_not_report_them";
    const RUNS: u64 = 1500;
    const STEP: u64 = 128 * KIB as u64;
    const SHRUNK: u64 = 4096;
    let _serial = serial();
    let dir = fixture_dir("shrunk");
    let path = dir.path().join("shrunk.bin");

    let mut file = File::create(&path).unwrap();
    mark_sparse(&file);
    for run in 0..RUNS {
        file.seek(SeekFrom::Start(run * STEP)).unwrap();
        file.write_all(&pattern(4096)).unwrap();
    }
    file.set_len(RUNS * STEP).unwrap();
    file.sync_all().unwrap();
    let number = number_of(reference_of(&path));

    let mft = load();
    let live = mft.record(number).expect("the file");
    let runs = live.open_stream(None).unwrap().extents().len();
    let records: Vec<u64> = live.records().map(|record| record.number()).collect();
    eprintln!("MEASURED: live: {runs} extents in records {records:?}");
    if records.len() < 2 {
        skip(NAME, "the runs fit in the base record");
        return;
    }
    let extension_records: Vec<u64> = records[1..].to_vec();
    drop(mft);

    // Shrunk to one block: the runs beyond it are gone, and their records are no longer needed.
    file.set_len(SHRUNK).unwrap();
    file.sync_all().unwrap();
    drop(file);
    let mft = load();
    let shrunk = mft.record(number).expect("the shrunk file");
    let states = extension_states(&mft, number);
    eprintln!(
        "MEASURED: shrunk to {SHRUNK} bytes while live: records now {:?}, extension records that name it {states:?}",
        shrunk.records().map(|record| record.number()).collect::<Vec<_>>()
    );
    let freed_by_shrinking: Vec<u64> = extension_records
        .iter()
        .copied()
        .filter(|&n| {
            mft.record(n)
                .is_some_and(|record| !record.is_used() && record.base_number() == Some(number))
        })
        .collect();
    eprintln!("MEASURED: extension records freed by shrinking, base alive: {freed_by_shrinking:?}");
    let live_size = FileInfo::new(&shrunk).size;
    assert_eq!(live_size, SHRUNK, "the shrunk file's size while live");
    drop(mft);
    if freed_by_shrinking.is_empty() {
        skip(
            NAME,
            "shrinking did not free an extension record: nothing to measure",
        );
        return;
    }

    fs::remove_file(&path).unwrap();
    let mft = load_where("shrunk and deleted", &[number], |mft| is_freed(mft, number));
    let file = mft
        .deleted_files()
        .find(|file| file.number() == number)
        .expect("the file is in deleted_files()");
    let states = extension_states(&mft, number);
    eprintln!(
        "MEASURED: after the delete: extension records that name it {states:?}; freed by shrinking: {:?}",
        freed_by_shrinking
            .iter()
            .map(|&n| (n, mft.record(n).map(|record| record.record_attributes().count())))
            .collect::<Vec<_>>()
    );
    let view: Vec<u64> = file.records().map(|record| record.number()).collect();
    let info = FileInfo::new(&file);
    let streams: Vec<_> = file.data_streams().collect();
    eprintln!(
        "MEASURED: the deleted view: records {view:?}, stream_data_lost {}, FileInfo size {} data_lost {}, streams {:?}",
        file.stream_data_lost(),
        info.size,
        info.data_lost,
        streams
            .iter()
            .map(|stream| (stream.name.clone(), stream.size, stream.data_lost))
            .collect::<Vec<_>>()
    );
    if let Ok(reader) = file.open_stream(None) {
        eprintln!(
            "MEASURED: default stream of the deleted file: size {}, {} extents, first {:?}",
            reader.size(),
            reader.extents().len(),
            reader.extents().first()
        );
        assert!(
            file.stream_data_lost() || reader.size() <= SHRUNK,
            "the deleted file reports a default stream of {} bytes ({} extents) although it was {SHRUNK} \
             bytes when deleted and does not say its data is lost: stale runs from a freed extension \
             record",
            reader.size(),
            reader.extents().len()
        );
        let stored: u64 = reader.extents().iter().map(|extent| extent.length).sum();
        assert_eq!(stored, reader.size(), "the extents tile the stream");
    }
    for stream in &streams {
        assert!(
            info.data_lost || stream.data_lost || stream.size <= SHRUNK,
            "stream {:?} of {} bytes is reported as the file's data",
            stream.name,
            stream.size
        );
    }
}
