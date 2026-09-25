#![cfg(target_os = "windows")]

//! What `ClusterBitmap` and `StreamReader::allocation` say about a deleted file on a real volume,
//! checked against the clusters' real contents. Streams here are tagged sector by sector, so the
//! bytes read back say whose they are: the victim's, a filler's, or zeroes.
//!
//! Measured: a free `$Bitmap` bit says nothing about the bytes, and on a disk that receives TRIM,
//! NTFS zeroes freed clusters within about ten seconds of a delete. Tests needing the victim's
//! bytes to survive disable delete notifications (`fsutil behavior set DisableDeleteNotify`,
//! system wide) for their run and always restore them. One test wants the trimmed state and runs
//! with notifications on.
//!
//! Tests take a lock and run one at a time (the setting is global; a file another test creates
//! could take a just-freed record) with `cargo test --test-threads=1`, not nextest, which runs
//! each test in its own process.
//!
//! Changing `DisableDeleteNotify` needs `NTFS_READER_ALLOW_FSUTIL=1` (set on the maintainer's VM)
//! or the test skips. Before changing anything, the guard saves the original values to
//! `%TEMP%\ntfs-reader-notify-original.txt`; the next guard restores from there if a previous run
//! was killed.
//!
//! Cluster placement is unpredictable, so the overwrite cases write a filler, poll the bitmap,
//! retry once, and skip if the volume never gets there. `NTFS_READER_REQUIRE_ALL=1` turns a skip
//! into a failure.

use std::ffi::{c_void, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ntfs_reader::{
    AllocationState, ClusterBitmap, DefaultPathCache, ExtentLocation, Mft, NtfsFile,
    StreamAllocation, StreamExtent, StreamReader, Volume,
};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_NO_BUFFERING,
};
use windows::Win32::System::Ioctl::{FSCTL_MOVE_FILE, FSCTL_SET_SPARSE, MOVE_FILE_DATA};
use windows::Win32::System::IO::DeviceIoControl;

mod common;
use common::{
    allow_fsutil, describe_record, flush_volume, skip, skip_environment, test_volume_letter,
    TempDirGuard,
};

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const RECORD_NUMBER_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
const SECTOR: usize = 512;
/// The victim: 4 MiB, a whole number of clusters at any cluster size NTFS formats with.
const VICTIM_SECTORS: u64 = 4 * MIB / SECTOR as u64;
/// Filler write piece size (small, so polling can stop close to the goal).
const PIECE: u64 = 256 * KIB;
/// The bitmap is read again after this many pieces.
const PIECES_PER_POLL: u64 = 4;
/// Cap on filler growth across both fill tries before giving up.
const MAX_FILLER: u64 = 2 * 1024 * MIB;
/// Files created before the victim and deleted right after it (see [`Sacrificials`]).
const SACRIFICIALS: usize = 32;
const VICTIM_TAG: &[u8; 8] = b"VICTIM!!";
const FILLER_TAG: &[u8; 8] = b"FILLER!!";

/// The tests share the volume's list of free records and one system-wide setting.
static SERIAL: Mutex<()> = Mutex::new(());

/// Set by the first test to take [`serial`], so the trimmed-state test knows whether it ran
/// before the fills (its result depends on a quiet disk, see there).
static A_TEST_HAS_RUN: AtomicBool = AtomicBool::new(false);

fn serial() -> std::sync::MutexGuard<'static, ()> {
    serial_and_first().0
}

/// [`serial`], and whether this is the first test of the process to take it.
fn serial_and_first() -> (std::sync::MutexGuard<'static, ()>, bool) {
    let guard = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    (guard, !A_TEST_HAS_RUN.swap(true, Ordering::SeqCst))
}

// ---- Delete notifications --------------------------------------------------------------------

/// `fsutil behavior ... DisableDeleteNotify`: with notifications on (0, default) NTFS sends TRIM
/// for a deleted file's clusters. Sets it for NTFS and ReFS, and restores the found values on
/// drop, even on panic, since the setting is system wide. A killed process cannot restore it, so
/// the original values also go to [`marker_path`] before anything changes, for the next guard to
/// restore.
struct NotifyGuard {
    original: Vec<(&'static str, u8)>,
}

const FILESYSTEMS: [&str; 2] = ["NTFS", "ReFS"];

/// Where the original `DisableDeleteNotify` values are kept while a guard is alive, one
/// `KEY=value` line each (`%TEMP%` is shared by every process the maintainer's VM run starts).
fn marker_path() -> PathBuf {
    std::env::var_os("TEMP")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("ntfs-reader-notify-original.txt")
}

fn fsutil(args: &[&str]) -> String {
    let output = Command::new("fsutil.exe")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run fsutil {args:?}: {e}"));
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "fsutil {args:?} failed ({}): {text} {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    text
}

/// The `DisableDeleteNotify` value of each file system the query lists.
fn query_notify() -> Vec<(&'static str, u8)> {
    let text = fsutil(&["behavior", "query", "DisableDeleteNotify"]);
    FILESYSTEMS
        .iter()
        .filter_map(|&key| {
            let line = text.lines().find(|line| {
                line.trim_start()
                    .starts_with(&format!("{key} DisableDeleteNotify"))
            })?;
            let value = line.split('=').nth(1)?.trim_start().chars().next()?;
            Some((key, value.to_digit(10)? as u8))
        })
        .collect()
}

fn set_notify(key: &str, value: u8) -> String {
    fsutil(&[
        "behavior",
        "set",
        "DisableDeleteNotify",
        key,
        &value.to_string(),
    ])
}

/// Restores the values a killed process left in the marker file, then deletes it. A line other
/// than `KEY=0`/`KEY=1` is ignored: the marker is written before any change, so a torn one means
/// nothing had changed yet.
fn restore_leftover() {
    let Ok(text) = fs::read_to_string(marker_path()) else {
        return;
    };
    for line in text.lines() {
        let restore = line.split_once('=').and_then(|(key, value)| {
            let key = FILESYSTEMS.iter().find(|&&known| known == key)?;
            Some((key, value.trim().parse::<u8>().ok().filter(|v| *v <= 1)?))
        });
        if let Some((key, value)) = restore {
            set_notify(key, value);
        }
    }
    eprintln!("restored DisableDeleteNotify from a killed run: {text:?}");
    fs::remove_file(marker_path()).expect("delete the restored marker file");
}

impl NotifyGuard {
    /// Turns delete notifications off (`disabled`: freed clusters stay as they are) or on (TRIM),
    /// for `test`. `None` after a skip if `NTFS_READER_ALLOW_FSUTIL=1` is unset.
    fn set(test: &str, disabled: bool) -> Option<Self> {
        if !allow_fsutil() {
            skip(
                test,
                "it changes DisableDeleteNotify system wide and NTFS_READER_ALLOW_FSUTIL=1 is not set",
            );
            return None;
        }
        restore_leftover();
        let original = query_notify();
        assert!(
            original.iter().any(|(key, _)| *key == "NTFS"),
            "could not read NTFS DisableDeleteNotify from `fsutil behavior query`"
        );
        let marker: String = original
            .iter()
            .map(|(key, value)| format!("{key}={value}\n"))
            .collect();
        fs::write(marker_path(), marker).expect("write the marker file of the original values");
        // The guard exists before anything is changed, so a failure half way still restores.
        let guard = NotifyGuard { original };
        for (key, _) in &guard.original {
            set_notify(key, u8::from(disabled));
        }
        for (key, value) in query_notify() {
            assert_eq!(value, u8::from(disabled), "{key} DisableDeleteNotify");
        }
        Some(guard)
    }
}

impl Drop for NotifyGuard {
    fn drop(&mut self) {
        // Nothing here may panic: this also runs while a failed test unwinds.
        let restore = std::panic::AssertUnwindSafe(|| {
            for (key, value) in &self.original {
                set_notify(key, *value);
            }
            let now = query_notify();
            if now == self.original {
                // Only now: a marker that stays is a promise to the next guard to try again.
                let _ = fs::remove_file(marker_path());
            } else {
                eprintln!(
                    "WARNING: DisableDeleteNotify was not restored to {:?}, it is {now:?}",
                    self.original
                );
            }
        });
        if std::panic::catch_unwind(restore).is_err() {
            eprintln!(
                "WARNING: restoring DisableDeleteNotify to {:?} failed, check it with `fsutil \
                 behavior query DisableDeleteNotify`",
                self.original
            );
        }
    }
}

// ---- Volume helpers --------------------------------------------------------------------------

fn fixture_dir(name: &str) -> TempDirGuard {
    TempDirGuard::new(format!("{}:\\recoverability-{name}", test_volume_letter()))
        .expect("create the fixture directory")
}

/// Flushes the volume and loads its `$MFT`.
fn load() -> Mft {
    let letter = test_volume_letter();
    flush_volume(&letter);
    let volume = Volume::new(format!("\\\\.\\{letter}:")).expect("open the volume");
    Mft::new(volume).expect("load the MFT")
}

/// Whether record `number` is freed in `mft`.
fn is_freed(mft: &Mft, number: u64) -> bool {
    mft.record(number)
        .is_some_and(|file| !file.is_used() && !mft.is_allocated(number))
}

/// Loads until record `number` is freed, for a volume that has not caught up with what was just
/// done to it, or a delete that is deferred because something else holds the file open. When it
/// never is, the failure says what the record is like (a delete-pending file is in use, under
/// `$Extend\$Deleted`).
fn load_freed(number: u64) -> Mft {
    let mut last = String::new();
    for _ in 0..8 {
        let mft = load();
        if is_freed(&mft, number) {
            return mft;
        }
        last = describe_record(&mft, number);
        std::thread::sleep(Duration::from_millis(1500));
    }
    panic!("record {number} was never freed: {last}");
}

/// The deleted file in record `number`, which must be listed by `deleted_files()`.
fn deleted(mft: &Mft, number: u64) -> NtfsFile<'_> {
    mft.deleted_files()
        .find(|file| file.number() == number)
        .unwrap_or_else(|| panic!("record {number} is not in deleted_files()"))
}

/// The live file `name` in the fixture directory `dir_name`.
fn find<'m>(mft: &'m Mft, dir_name: &str, name: &str) -> NtfsFile<'m> {
    let mut cache = DefaultPathCache::new();
    let tail = Path::new(dir_name).join(name);
    mft.files()
        .find(|file| {
            file.hard_links().any(|link| {
                link.to_string() == name
                    && mft
                        .resolve_path(&link, &mut cache)
                        .is_some_and(|path| path.ends_with(&tail))
            })
        })
        .unwrap_or_else(|| panic!("{dir_name}\\{name} is not in the MFT"))
}

/// The record number Win32 reports for `path`.
fn record_number_of(path: &Path) -> u64 {
    let file = OpenOptions::new()
        .access_mode(0)
        .share_mode(7)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)
        .unwrap_or_else(|e| panic!("open {path:?}: {e}"));
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
        .unwrap_or_else(|e| panic!("file information of {path:?}: {e}"));
    ((u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow)) & RECORD_NUMBER_MASK
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

// ---- Tagged content --------------------------------------------------------------------------

/// One sector: the tag, its own index, and bytes depending on both, so no sector equals another
/// one, of the same file or of another kind.
fn sector(tag: &[u8; 8], index: u64) -> [u8; SECTOR] {
    let mut bytes = [0u8; SECTOR];
    bytes[..8].copy_from_slice(tag);
    bytes[8..16].copy_from_slice(&index.to_le_bytes());
    let mut state = (index.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ u64::from_le_bytes(*tag)) | 1;
    for chunk in bytes[16..].chunks_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
    }
    bytes
}

/// `count` tagged sectors starting at sector index `first`.
fn tagged(tag: &[u8; 8], first: u64, count: u64) -> Vec<u8> {
    (first..first + count)
        .flat_map(|index| sector(tag, index))
        .collect()
}

/// Whose bytes `read` holds, sector by sector, against the victim's own (`expected`): its own,
/// the filler's tag, zeroes, or other. A raw comparison, independent of the crate.
#[derive(Debug, Default, PartialEq, Eq)]
struct Whose {
    old: u64,
    filler: u64,
    zero: u64,
    other: u64,
}

fn whose(read: &[u8], expected: &[u8]) -> Whose {
    assert_eq!(
        read.len(),
        expected.len(),
        "the read is as long as the file"
    );
    let mut counts = Whose::default();
    for (got, want) in read.chunks(SECTOR).zip(expected.chunks(SECTOR)) {
        let bytes = got.len() as u64;
        if got == want {
            counts.old += bytes;
        } else if got.starts_with(FILLER_TAG) {
            counts.filler += bytes;
        } else if got.iter().all(|&byte| byte == 0) {
            counts.zero += bytes;
        } else {
            counts.other += bytes;
        }
    }
    counts
}

fn read_all(stream: &mut StreamReader) -> Vec<u8> {
    stream.seek(SeekFrom::Start(0)).unwrap();
    let mut data = Vec::new();
    stream.read_to_end(&mut data).expect("read the stream");
    data
}

/// The parts of an allocation add up to the size of the stream.
#[track_caller]
fn assert_parts_add_up(allocation: &StreamAllocation) {
    let parts = allocation.in_free_clusters()
        + allocation.in_allocated_clusters()
        + allocation.resident()
        + allocation.sparse()
        + allocation.missing()
        + allocation.beyond_initialized()
        + allocation.outside_bitmap();
    assert_eq!(parts, allocation.size(), "{allocation:?}");
}

// ---- The victim ------------------------------------------------------------------------------

/// A file of `VICTIM_SECTORS` tagged sectors, on disk and flushed, and its record number.
fn write_victim(dir: &Path, name: &str) -> (PathBuf, u64, Vec<u8>) {
    let path = dir.join(name);
    let content = tagged(VICTIM_TAG, 0, VICTIM_SECTORS);
    let mut file = File::create(&path).unwrap();
    file.write_all(&content).unwrap();
    file.sync_all().unwrap();
    drop(file);
    flush_volume(&test_volume_letter());
    let number = record_number_of(&path);
    (path, number, content)
}

/// Deletes the victim and reloads the `Mft`, in which its record is freed.
fn delete_victim(path: &Path, number: u64) -> Mft {
    fs::remove_file(path).unwrap();
    load_freed(number)
}

/// Small files created BEFORE the victim and deleted right after it. NTFS hands a new record to
/// the lowest free one at once, and a filler that grows in many pieces needs records of its own
/// (an `$ATTRIBUTE_LIST` and extension records): without these, the filler's first extension takes
/// the victim's freed record (seen: record 513 was one). Created earlier, these files have lower
/// record numbers than the victim, so they are handed out first. No file may be created after the
/// victim is deleted.
struct Sacrificials(Vec<PathBuf>);

impl Sacrificials {
    fn create(dir: &Path) -> Self {
        let files = (0..SACRIFICIALS)
            .map(|index| {
                let path = dir.join(format!("sacrificial-{index:02}.txt"));
                fs::write(&path, b"sacrificial").unwrap();
                path
            })
            .collect();
        Sacrificials(files)
    }

    /// Deletes them, and reports whether their records are below the victim's (what makes them go
    /// first).
    fn delete(self, victim: u64) {
        let numbers: Vec<u64> = self.0.iter().map(|path| record_number_of(path)).collect();
        let above = numbers.iter().filter(|&&number| number > victim).count();
        eprintln!(
            "{} sacrificial files, records {}..={}, {above} above the victim's {victim}",
            numbers.len(),
            numbers.iter().min().unwrap(),
            numbers.iter().max().unwrap()
        );
        for path in self.0 {
            fs::remove_file(path).unwrap();
        }
    }
}

/// [`delete_victim`], then the sacrificial files, so the freed records stay the lower ones.
fn delete_victim_protected(path: &Path, number: u64, sacrificials: Sacrificials) -> Mft {
    fs::remove_file(path).unwrap();
    sacrificials.delete(number);
    load_freed(number)
}

/// The deleted file `number`'s stream, its allocation against a freshly read bitmap, and the
/// bytes it reads.
fn look_at(mft: &Mft, number: u64) -> (StreamAllocation, Vec<u8>) {
    let file = deleted(mft, number);
    assert!(!file.stream_data_lost(), "the victim has no attribute list");
    let mut stream = file.open_stream(None).expect("open the deleted stream");
    let bitmap = ClusterBitmap::new(mft).expect("read the cluster bitmap");
    let allocation = stream
        .allocation(&bitmap)
        .expect("the bitmap of the same volume");
    assert_parts_add_up(&allocation);
    (allocation, read_all(&mut stream))
}

/// Appends tagged 256 KiB pieces to `filler` (created before the victim was deleted, so it takes
/// the freed record) and, every few pieces, closes it, flushes, and rereads the bitmap, until
/// `done` is true of the victim's allocation. `None` if the time or size runs out first.
///
/// Closed before every bitmap look, because NTFS grows a file's allocation ahead of what was
/// written and gives the excess back only when the last handle closes; an open file's bitmap read
/// would count clusters that are only temporarily the filler's. Pieces continue where the filler
/// ended, so a repeat call is a retry that keeps what the first attempt wrote.
fn fill_until(
    filler: &Path,
    mft: &Mft,
    stream: &StreamReader,
    deadline: Instant,
    max_len: u64,
    done: &impl Fn(&StreamAllocation) -> bool,
) -> Option<StreamAllocation> {
    let started = Instant::now();
    let piece_sectors = PIECE / SECTOR as u64;
    // Where the filler ends, so that a retry goes on with the next sectors of the tag.
    let first = fs::metadata(filler).unwrap().len();
    let mut written = first;
    loop {
        let mut file = OpenOptions::new().append(true).open(filler).unwrap();
        for _ in 0..PIECES_PER_POLL {
            file.write_all(&tagged(FILLER_TAG, written / SECTOR as u64, piece_sectors))
                .unwrap();
            file.sync_all().unwrap();
            written += PIECE;
        }
        drop(file);
        flush_volume(&test_volume_letter());
        let bitmap = ClusterBitmap::new(mft).expect("read the cluster bitmap");
        let allocation = stream
            .allocation(&bitmap)
            .expect("the bitmap of the same volume");
        if done(&allocation) {
            eprintln!(
                "filled {} MiB in {:?}: {allocation:?}",
                (written - first) / MIB,
                started.elapsed()
            );
            return Some(allocation);
        }
        if Instant::now() > deadline || written >= max_len {
            eprintln!(
                "gave up after {} MiB in {:?} (the filler is {} MiB): {allocation:?}. {}",
                (written - first) / MIB,
                started.elapsed(),
                written / MIB,
                placement(filler, stream)
            );
            return None;
        }
    }
}

/// `(first cluster, cluster count)` of every run of the extents that sit on the volume.
fn cluster_runs(extents: &[StreamExtent], cluster_size: u64) -> Vec<(u64, u64)> {
    extents
        .iter()
        .filter_map(|extent| match extent.location {
            ExtentLocation::Volume { offset } => {
                Some((offset / cluster_size, extent.length.div_ceil(cluster_size)))
            }
            _ => None,
        })
        .collect()
}

/// Where the victim's clusters sit relative to the filler's runs, for a fill-failure message
/// (the allocator decides placement). The filler's runs come from a fresh load, since its extents
/// change as it grows.
fn placement(filler: &Path, victim: &StreamReader) -> String {
    let victim_runs = cluster_runs(victim.extents(), victim.cluster_size());
    let mft = load();
    let filler_file = mft
        .files()
        .find(|file| file.number() == record_number_of(filler));
    let Some(runs) = filler_file
        .and_then(|file| file.open_stream(None).ok())
        .map(|stream| cluster_runs(stream.extents(), stream.cluster_size()))
    else {
        return "the filler's runs could not be read".to_string();
    };
    let (Some(low), Some(high)) = (
        runs.iter().map(|run| run.0).min(),
        runs.iter().map(|run| run.0 + run.1).max(),
    ) else {
        return "the filler has no runs".to_string();
    };
    let shown = |runs: &[(u64, u64)]| {
        let head: Vec<_> = runs.iter().take(6).collect();
        format!("{} runs, first {head:?}", runs.len())
    };
    let position = victim_runs.first().map_or("nowhere", |&(start, _)| {
        if start < low {
            "BELOW the filler's lowest cluster"
        } else if start >= high {
            "ABOVE the filler's highest cluster"
        } else {
            "INSIDE the filler's span (the gaps between its runs are not ours)"
        }
    });
    format!(
        "victim's clusters ({}) are {position}; the filler spans clusters {low}..{high} in {}",
        shown(&victim_runs),
        shown(&runs)
    )
}

/// [`fill_until`], tried twice within one budget: placement is unpredictable, and a second stretch
/// of the same filler often reaches what the first missed (seen: 948 MiB without reaching the
/// victim, then 74 MiB more reached it). The first try gets two thirds of `limit`, the retry the
/// rest; the filler never grows past `MAX_FILLER` in all.
fn fill_twice(
    filler: &Path,
    mft: &Mft,
    stream: &StreamReader,
    limit: Duration,
    done: impl Fn(&StreamAllocation) -> bool,
) -> Option<StreamAllocation> {
    let start = Instant::now();
    let max_len = fs::metadata(filler).unwrap().len() + MAX_FILLER;
    let deadlines = [start + limit * 2 / 3, start + limit];
    deadlines
        .into_iter()
        .enumerate()
        .find_map(|(attempt, deadline)| {
            if attempt > 0 {
                eprintln!("retrying the fill");
            }
            fill_until(filler, mft, stream, deadline, max_len, &done)
        })
}

/// Creates the filler file, empty, and closes it: its record only needs to exist before the
/// victim's is freed.
fn create_filler(dir: &Path) -> PathBuf {
    let path = dir.join("filler.bin");
    File::create(&path).unwrap();
    path
}

// ---- Cases -----------------------------------------------------------------------------------

// Notifications off, nothing written after the delete: the bitmap says free and the bytes are the
// file's, all of them.
#[test]
fn a_deleted_file_nothing_has_touched_is_free_and_reads_intact() {
    let _serial = serial();
    let Some(_notify) = NotifyGuard::set(
        "a_deleted_file_nothing_has_touched_is_free_and_reads_intact",
        true,
    ) else {
        return;
    };
    let dir = fixture_dir("full");
    let (path, number, content) = write_victim(dir.path(), "victim.bin");

    let mft = delete_victim(&path, number);
    let (allocation, read) = look_at(&mft, number);
    let size = content.len() as u64;
    assert_eq!(allocation.size(), size);
    assert_eq!(allocation.state(), AllocationState::Free, "{allocation:?}");
    assert_eq!(allocation.in_free_clusters(), size);
    assert_eq!(allocation.in_allocated_clusters(), 0);
    assert!(
        read == content,
        "the bytes of a free, untouched file are its own"
    );
}

/// Moves `clusters` clusters of `file` from its VCN `vcn` onto free clusters starting at LCN
/// `lcn`, via `FSCTL_MOVE_FILE` (the defragmentation API) on a volume handle. Frees the file's old
/// clusters; the target ones now hold its bytes. NTFS hands out a delete's freed clusters only
/// once the delete is committed, so the caller retries.
fn move_file_data(file: &Path, vcn: u64, lcn: u64, clusters: u64) -> std::io::Result<()> {
    let volume_path = format!("\\\\.\\{}:", test_volume_letter());
    let volume = OpenOptions::new()
        .read(true)
        .share_mode(7)
        .open(&volume_path)
        .map_err(|e| std::io::Error::other(format!("open {volume_path}: {e}")))?;
    let target = OpenOptions::new()
        .read(true)
        .share_mode(7)
        .open(file)
        .map_err(|e| std::io::Error::other(format!("open {file:?}: {e}")))?;
    let input = MOVE_FILE_DATA {
        FileHandle: HANDLE(target.as_raw_handle()),
        StartingVcn: vcn as i64,
        StartingLcn: lcn as i64,
        ClusterCount: clusters as u32,
    };
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            HANDLE(volume.as_raw_handle()),
            FSCTL_MOVE_FILE,
            Some(&input as *const MOVE_FILE_DATA as *const c_void),
            std::mem::size_of::<MOVE_FILE_DATA>() as u32,
            None,
            0,
            Some(&mut returned),
            None,
        )
    }
    .map_err(|e| {
        std::io::Error::other(format!(
            "FSCTL_MOVE_FILE of {clusters} clusters from VCN {vcn} of {file:?} to LCN {lcn}: {e}"
        ))
    })
}

/// What a partly overwritten victim must look like: a fresh load's bitmap says some clusters are
/// allocated and some free, and the DISK bytes (a raw read, not the cache) are the filler's in the
/// allocated ones, the victim's own in the free ones, sector by sector. `exact` is the byte count
/// the test itself gave the filler, when known.
fn assert_partly_overwritten(number: u64, content: &[u8], exact: Option<u64>) {
    let mft = load_freed(number);
    let (allocation, read) = look_at(&mft, number);
    let stream = deleted(&mft, number).open_stream(None).unwrap();
    let disk = raw_disk_read(&stream);
    let counts = whose(&disk, content);
    eprintln!("partial: {allocation:?} disk {counts:?}");
    let size = content.len() as u64;
    assert_eq!(allocation.state(), AllocationState::PartlyAllocated);
    // What the bitmap calls free is what still holds the victim's bytes, and what it calls
    // allocated is the filler's, by a comparison of the bytes themselves.
    assert_eq!(allocation.in_free_clusters(), counts.old, "{counts:?}");
    assert_eq!(
        allocation.in_allocated_clusters(),
        counts.filler,
        "{counts:?}"
    );
    assert_eq!((counts.zero, counts.other), (0, 0), "{counts:?}");
    assert!(counts.old > 0 && counts.filler > 0);
    if let Some(moved) = exact {
        assert_eq!(allocation.in_allocated_clusters(), moved, "{allocation:?}");
        assert_eq!(
            allocation.in_free_clusters(),
            size - moved,
            "{allocation:?}"
        );
    }
    assert!(
        read == disk,
        "the crate reads other bytes than the disk holds: {:?} against {counts:?}",
        whose(&read, content)
    );
}

// Part of the deleted file's clusters are given to another file: partly allocated, with the
// allocated clusters' bytes the other file's and the free ones the victim's.
//
// The allocator does not decide which clusters the other file gets: a filler left to grow over
// the victim's freed clusters spanned them with gaps, taking 55 MiB and 90 s in one run and
// missing them in another. `FSCTL_MOVE_FILE` decides instead: a 2 MiB filler made BEFORE the
// delete (one made after would take the freed record, reused at once) has a third of the victim's
// clusters, the first ones, moved onto the victim's first freed clusters, so the answer is exact:
// that many bytes allocated, the rest free. A refused move fails under
// `NTFS_READER_REQUIRE_ALL=1`; otherwise the filler grows until it reaches a third of the victim.
#[test]
fn a_partly_overwritten_deleted_file_is_partly_allocated() {
    const NAME: &str = "a_partly_overwritten_deleted_file_is_partly_allocated";
    /// Written to the filler before the delete: more than a third of the victim at any cluster size.
    const PREFILL_SECTORS: u64 = 2 * MIB / SECTOR as u64;
    /// The move is tried this often, this far apart: NTFS gives out freed clusters once the delete
    /// is committed.
    const MOVE_TRIES: u32 = 6;
    let _serial = serial();
    let Some(_notify) = NotifyGuard::set(NAME, true) else {
        return;
    };
    let dir = fixture_dir("partial");
    // Created before the delete: NTFS gives a new file the lowest free record at once, and it must
    // not be the victim's.
    let filler = create_filler(dir.path());
    let mut file = OpenOptions::new().append(true).open(&filler).unwrap();
    file.write_all(&tagged(FILLER_TAG, 0, PREFILL_SECTORS))
        .unwrap();
    file.sync_all().unwrap();
    drop(file);
    let sacrificials = Sacrificials::create(dir.path());
    let (path, number, content) = write_victim(dir.path(), "victim.bin");
    let mft = delete_victim_protected(&path, number, sacrificials);
    let stream = deleted(&mft, number).open_stream(None).unwrap();
    let size = content.len() as u64;
    let cluster_size = stream.cluster_size();

    // A third of the victim, but no more than its first run: the moved clusters are the first ones.
    let runs = cluster_runs(stream.extents(), cluster_size);
    let &(first_lcn, run_clusters) = runs.first().expect("the victim has clusters on the volume");
    let clusters = (size / cluster_size / 3).clamp(1, run_clusters);
    assert!(clusters * cluster_size <= PREFILL_SECTORS * SECTOR as u64);
    let mut moved = Ok(());
    for attempt in 0..MOVE_TRIES {
        flush_volume(&test_volume_letter());
        moved = move_file_data(&filler, 0, first_lcn, clusters);
        match &moved {
            Ok(()) => break,
            Err(err) => eprintln!("move attempt {}: {err}", attempt + 1),
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    match moved {
        Ok(()) => {
            flush_volume(&test_volume_letter());
            eprintln!(
                "moved {clusters} of the filler's clusters ({} bytes) onto the victim's first \
                 clusters at LCN {first_lcn}; {}",
                clusters * cluster_size,
                placement(&filler, &stream)
            );
            assert_partly_overwritten(number, &content, Some(clusters * cluster_size));
        }
        Err(err) => {
            skip(
                NAME,
                &format!("the move was refused {MOVE_TRIES} times: {err}"),
            );
            // Only without NTFS_READER_REQUIRE_ALL: the allocator's way, with the same filler.
            let Some(reached) = fill_twice(&filler, &mft, &stream, Duration::from_secs(90), |a| {
                a.in_allocated_clusters() * 3 >= size
            }) else {
                skip(
                    NAME,
                    "a third of the victim's clusters were not reached within 90 s (two tries, see the placement above)",
                );
                return;
            };
            if reached.state() == AllocationState::Allocated {
                skip(
                    NAME,
                    "the filler overshot: every cluster of the victim was taken at once",
                );
                return;
            }
            // Looked at while the filler still exists (the fixture directory removes it at the end).
            assert_partly_overwritten(number, &content, None);
        }
    }
}

/// Moves the filler's clusters onto every run of the victim, in turn: the filler's first `n`
/// clusters (from VCN 0) onto the victim's first run, the next onto the second, and so on. Each
/// move is tried `tries` times, 2 s apart, flushing first (NTFS hands out freed clusters only once
/// the delete is committed). `Err` names the failed step: run, try, error.
fn move_filler_onto(filler: &Path, runs: &[(u64, u64)], tries: u32) -> Result<(), String> {
    let mut vcn = 0;
    for (index, &(lcn, clusters)) in runs.iter().enumerate() {
        let mut last = String::new();
        let mut done = false;
        for attempt in 1..=tries {
            flush_volume(&test_volume_letter());
            match move_file_data(filler, vcn, lcn, clusters) {
                Ok(()) => {
                    done = true;
                    break;
                }
                Err(err) => {
                    eprintln!("move of run {} attempt {attempt}: {err}", index + 1);
                    last = err.to_string();
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        if !done {
            return Err(format!(
                "the move of run {} of {} (LCN {lcn}, {clusters} clusters) was refused {tries} times: {last}",
                index + 1,
                runs.len()
            ));
        }
        vcn += clusters;
    }
    Ok(())
}

/// What a victim whose every cluster was given away must look like: allocated, all its bytes in
/// allocated clusters, and the DISK bytes (a raw read) the filler's, nothing else.
fn assert_all_reused(number: u64, content: &[u8]) {
    let mft = load_freed(number);
    let (allocation, read) = look_at(&mft, number);
    let stream = deleted(&mft, number).open_stream(None).unwrap();
    let disk = raw_disk_read(&stream);
    let counts = whose(&disk, content);
    eprintln!("all reused: {allocation:?} disk {counts:?}");
    let size = content.len() as u64;
    assert_eq!(
        allocation.state(),
        AllocationState::Allocated,
        "{allocation:?}"
    );
    assert_eq!(allocation.in_allocated_clusters(), size, "{allocation:?}");
    assert_eq!(allocation.in_free_clusters(), 0, "{allocation:?}");
    assert_eq!(counts.filler, size, "{counts:?}");
    assert_eq!(
        (counts.old, counts.zero, counts.other),
        (0, 0, 0),
        "only the filler's bytes are on the disk: {counts:?}"
    );
    assert!(
        read == disk,
        "the crate reads other bytes than the disk holds: {:?} against {counts:?}",
        whose(&read, content)
    );
}

// Every cluster of the deleted file is given to another file: allocated, all bytes the other
// file's.
//
// As in the partly overwritten test, the allocator is not left to choose: a growing filler spread
// over 2.4 GiB with holes inside the victim's region, and "every cluster allocated" never came
// true. The filler is written BEFORE the delete, as long as the victim; a chunk of it is then
// moved onto each of the victim's runs with `FSCTL_MOVE_FILE`. A refused move fails on the step
// (under `NTFS_READER_REQUIRE_ALL=1`); otherwise the filler grows until it covers the victim.
#[test]
fn a_deleted_file_whose_clusters_were_all_reused_is_allocated_and_reads_the_new_bytes() {
    const NAME: &str =
        "a_deleted_file_whose_clusters_were_all_reused_is_allocated_and_reads_the_new_bytes";
    const MOVE_TRIES: u32 = 6;
    let _serial = serial();
    let Some(_notify) = NotifyGuard::set(NAME, true) else {
        return;
    };
    let dir = fixture_dir("none");
    // Created and written before the delete: a new file would take the freed record, and the
    // clusters moved from it must exist. As long as the victim, so every cluster has a source.
    let filler = create_filler(dir.path());
    let mut file = OpenOptions::new().append(true).open(&filler).unwrap();
    file.write_all(&tagged(FILLER_TAG, 0, VICTIM_SECTORS))
        .unwrap();
    file.sync_all().unwrap();
    drop(file);
    let sacrificials = Sacrificials::create(dir.path());
    let (path, number, content) = write_victim(dir.path(), "victim.bin");
    let mft = delete_victim_protected(&path, number, sacrificials);
    let stream = deleted(&mft, number).open_stream(None).unwrap();
    let size = content.len() as u64;

    let runs = cluster_runs(stream.extents(), stream.cluster_size());
    let victim_clusters: u64 = runs.iter().map(|run| run.1).sum();
    assert_eq!(
        victim_clusters * stream.cluster_size(),
        size,
        "the victim is whole clusters on the volume: {runs:?}"
    );
    match move_filler_onto(&filler, &runs, MOVE_TRIES) {
        Ok(()) => {
            flush_volume(&test_volume_letter());
            eprintln!(
                "moved the filler onto all {} run(s) of the victim, {victim_clusters} clusters; {}",
                runs.len(),
                placement(&filler, &stream)
            );
            assert_all_reused(number, &content);
        }
        Err(step) => {
            skip(NAME, &step);
            // Only without NTFS_READER_REQUIRE_ALL: the allocator's way, with the same filler.
            let Some(_) = fill_twice(&filler, &mft, &stream, Duration::from_secs(180), |a| {
                a.state() == AllocationState::Allocated
            }) else {
                skip(
                    NAME,
                    "the move failed (above) and every cluster of the victim was not reached by writing within 180 s (two tries, see the placement above)",
                );
                return;
            };
            // Looked at while the filler still exists: deleting it would free the clusters again.
            assert_all_reused(number, &content);
        }
    }
}

/// The stream's bytes as the DISK holds them, bypassing the system cache: `stream`'s extents read
/// through a volume handle opened with `FILE_FLAG_NO_BUFFERING`. That needs sector-aligned
/// offsets, lengths, and buffer addresses, given by the victim's whole-cluster extents and a 4096
/// aligned buffer. A normal volume handle (what the crate reads through) can be served from the
/// cache, so after a delete or TRIM it can still return bytes the disk no longer has; this is the
/// oracle for what the disk actually holds.
fn raw_disk_read(stream: &StreamReader) -> Vec<u8> {
    const CHUNK: usize = MIB as usize;
    const ALIGN: usize = 4096;
    let volume_path = format!("\\\\.\\{}:", test_volume_letter());
    let mut volume = OpenOptions::new()
        .read(true)
        .share_mode(3)
        .custom_flags(FILE_FLAG_NO_BUFFERING.0)
        .open(&volume_path)
        .unwrap_or_else(|e| panic!("open {volume_path} without buffering: {e}"));
    let mut backing = vec![0u8; CHUNK + ALIGN];
    let start = backing.as_ptr().align_offset(ALIGN);
    let buf = &mut backing[start..start + CHUNK];
    let mut bytes = Vec::with_capacity(stream.size() as usize);
    for extent in stream.extents() {
        assert!(
            extent.length.is_multiple_of(ALIGN as u64),
            "an extent of {} bytes is not a whole number of blocks: {extent:?}",
            extent.length
        );
        let ExtentLocation::Volume { offset } = extent.location else {
            panic!("the victim has an extent that is not on the volume: {extent:?}");
        };
        let mut done = 0u64;
        while done < extent.length {
            let n = (extent.length - done).min(CHUNK as u64) as usize;
            volume.seek(SeekFrom::Start(offset + done)).unwrap();
            volume
                .read_exact(&mut buf[..n])
                .unwrap_or_else(|e| panic!("raw read of {n} bytes at {}: {e}", offset + done));
            bytes.extend_from_slice(&buf[..n]);
            done += n as u64;
        }
    }
    bytes.truncate(stream.size() as usize);
    bytes
}

// Notifications on (the default): NTFS TRIMs the freed clusters itself, seconds after the delete.
// The bitmap says free, exactly as for intact bytes, but the disk bytes are zeroes. This
// documents why "free" is not "intact": no API of this crate can tell the two apart from the
// bitmap.
//
// Whether the disk gets that TRIM is a property of the disk and its load, not the crate. Measured
// on the test VM: a lone delete followed by raw reads sees the freed clusters zeroed at 10 s
// (1011 of 1024, notifications on); after heavy fills in the same binary, no zeroing in 30 s
// (raw NO_BUFFERING reads, 64- and 32-bit builds, twice). This only shows that `Free` can hide
// zeroes WHEN TRIM happens, so it runs FIRST in the binary (its name sorts first, and the run is
// `--test-threads=1`, in name order). It prints the disk state at 5, 10, and 20 s as `MEASURED`
// lines; a disk that stays quiet is an ENVIRONMENT skip (`[env]` marker), not a failure under
// `NTFS_READER_REQUIRE_ALL=1` (see `common::skip_environment`).
//
// The oracle for "zeroed" is an independent raw `FILE_FLAG_NO_BUFFERING` read of the victim's
// extents, polled every second. The crate's own read, through a normal (cacheable) volume handle,
// returned old bytes for 90 s on the test VM after the disk was already zero at 10 s, so it is
// only PRINTED (`MEASURED`), never asserted.
#[test]
fn a_deleted_file_after_a_trim_is_free_and_reads_as_zeroes() {
    const NAME: &str = "a_deleted_file_after_a_trim_is_free_and_reads_as_zeroes";
    /// How long after the delete the disk must show zeroes (measured: 10 s on the VM, none at 5 s).
    const ZEROING_WAIT: Duration = Duration::from_secs(30);
    /// The times, after the delete, at which the state of the disk is printed.
    const MARKS: [Duration; 3] = [
        Duration::from_secs(5),
        Duration::from_secs(10),
        Duration::from_secs(20),
    ];
    let (_serial, first) = serial_and_first();
    let Some(_notify) = NotifyGuard::set(NAME, false) else {
        return;
    };
    // What the machine says about delete notifications, before and after the guard set them: this
    // tells apart a run that sees no zeroing from one where TRIM was never switched on.
    let now = query_notify();
    let settings = format!(
        "fsutil says {now:?} after the guard set delete notifications on (0); the guard restores {:?}; \
         this test ran {} the other tests of the binary",
        _notify.original,
        if first { "before" } else { "AFTER" }
    );
    eprintln!("{NAME}: {settings}");
    // The test configured TRIM itself, so it must not go on (and then skip) on a disk where the
    // setting did not take: that would blame the disk for the machine's configuration.
    assert!(
        now.iter().all(|&(_, value)| value == 0),
        "DisableDeleteNotify is not 0 although the guard set it: {settings}"
    );
    let dir = fixture_dir("trimmed");
    let (path, number, content) = write_victim(dir.path(), "victim.bin");
    let deleted_at = Instant::now();
    let mft = delete_victim(&path, number);
    // Free already: allocation does not wait for the zeroing.
    let (early, _) = look_at(&mft, number);
    assert_eq!(early.state(), AllocationState::Free);
    let stream = deleted(&mft, number).open_stream(None).unwrap();
    let crate_counts = |when: &str| {
        let counts = whose(
            &read_all(&mut deleted(&mft, number).open_stream(None).unwrap()),
            &content,
        );
        eprintln!("MEASURED ({when}): the crate's read of the victim: {counts:?}");
    };

    // Polled until the marks are all past and zeroing was seen, or the wait is over.
    let mut marks = MARKS.iter().peekable();
    let mut seen = None;
    loop {
        let disk = whose(&raw_disk_read(&stream), &content);
        let elapsed = deleted_at.elapsed();
        while let Some(mark) = marks.next_if(|mark| elapsed >= **mark) {
            eprintln!(
                "MEASURED: the disk {elapsed:?} after the delete (mark {mark:?}, {}): {disk:?}",
                if first {
                    "test ran first"
                } else {
                    "test ran after other tests"
                }
            );
        }
        if seen.is_none() && disk.zero > 0 {
            eprintln!("zeroing seen on the disk after {elapsed:?}: {disk:?}");
            crate_counts("when the raw read first shows zeroes");
            seen = Some(disk);
        }
        if (seen.is_some() && marks.peek().is_none()) || elapsed > ZEROING_WAIT {
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    let Some(seen) = seen else {
        skip_environment(
            NAME,
            &format!(
                "no zeroing on the disk in {ZEROING_WAIT:?} (raw NO_BUFFERING reads), so no TRIM \
                 reached it: a property of the disk and its load ({settings})"
            ),
        );
        return;
    };
    // The disk may still be at it: read once more, a moment later.
    std::thread::sleep(Duration::from_secs(3));
    let counts = whose(&raw_disk_read(&stream), &content);
    crate_counts("3 s after that");
    let mft = load_freed(number);
    let (allocation, _) = look_at(&mft, number);
    eprintln!("trimmed: {allocation:?} raw {counts:?} (first seen {seen:?})");
    let size = content.len() as u64;
    // The bitmap still says free, exactly as for a file with intact bytes.
    assert_eq!(allocation.state(), AllocationState::Free, "{allocation:?}");
    assert_eq!(allocation.in_free_clusters(), size);
    // The disk discards whole aligned units, so a few clusters at the edges of a free extent can
    // keep their old bytes (13 of 1024 were seen): nearly everything is zero, and the rest is
    // still the victim's, nothing else.
    assert_eq!(counts.filler + counts.other, 0, "{counts:?}");
    assert!(
        counts.zero * 100 >= size * 95,
        "only {} of {size} bytes were zeroed on the disk: {counts:?}",
        counts.zero
    );
}

// ---- What a deleted stream is compared with what it was -----------------------------------------

/// A stream as a crate user sees it: its reported size, how much was ever written, where its
/// bytes are, and the bytes it reads.
struct Content {
    size: u64,
    initialized_size: u64,
    extents: Vec<StreamExtent>,
    bytes: Vec<u8>,
}

fn content_of(file: &NtfsFile, stream: Option<&str>) -> Content {
    let mut reader = file
        .open_stream(stream.map(OsStr::new))
        .unwrap_or_else(|e| panic!("open stream {stream:?}: {e}"));
    Content {
        size: reader.size(),
        initialized_size: reader.initialized_size(),
        extents: reader.extents().to_vec(),
        bytes: read_all(&mut reader),
    }
}

/// The first offset at which `a` and `b` differ, or where the shorter one ends when one is a prefix
/// of the other; `None` when they are equal.
fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    a.iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .or((a.len() != b.len()).then_some(a.len().min(b.len())))
}

/// Records what `streams` of the live file at `path` are, deletes it, reloads, and checks the
/// deleted file's streams match in every respect (nothing legitimately differs for a file without
/// an attribute list, read before anything else writes to the volume). `wrote` is what the test
/// wrote to each stream in full (holes as zeroes): both the live and deleted reads are compared
/// against it, so a crate reading the same wrong bytes both times still fails. Returns the live
/// contents for the caller's own checks.
fn deleted_streams_are_what_they_were(
    what: &str,
    path: &Path,
    streams: &[Option<&str>],
    wrote: &[&[u8]],
) -> Vec<Content> {
    assert_eq!(streams.len(), wrote.len());
    let number = record_number_of(path);
    let live = load();
    let file = live.record(number).expect("the live file");
    let before: Vec<Content> = streams
        .iter()
        .map(|&stream| content_of(&file, stream))
        .collect();
    drop(live);
    for ((&stream, was), &wrote) in streams.iter().zip(&before).zip(wrote) {
        assert_eq!(
            first_difference(&was.bytes, wrote),
            None,
            "{what}, stream {stream:?}: the live read is not what the test wrote ({} bytes read, {} \
             written)",
            was.bytes.len(),
            wrote.len()
        );
    }

    fs::remove_file(path).unwrap();
    let mft = load_freed(number);
    let file = deleted(&mft, number);
    assert!(!file.stream_data_lost(), "{what}: no attribute list");
    for ((&stream, was), &wrote) in streams.iter().zip(&before).zip(wrote) {
        let now = content_of(&file, stream);
        let which = format!("{what}, stream {stream:?}");
        assert_eq!(
            first_difference(&now.bytes, wrote),
            None,
            "{which}: the bytes read after the delete are not what the test wrote ({} bytes read, {} \
             written)",
            now.bytes.len(),
            wrote.len()
        );
        assert_eq!(now.size, was.size, "{which}: size");
        assert_eq!(
            now.initialized_size, was.initialized_size,
            "{which}: initialized size"
        );
        assert_eq!(now.extents, was.extents, "{which}: extents");
        let differ = now
            .bytes
            .iter()
            .zip(&was.bytes)
            .position(|(after, before)| after != before);
        assert!(
            now.bytes.len() == was.bytes.len() && differ.is_none(),
            "{which}: the bytes read after the delete differ from the live ones (first difference \
             at {differ:?}, {} bytes then, {} now)",
            was.bytes.len(),
            now.bytes.len()
        );
    }
    before
}

// The user's question about a deleted file, asked of real NTFS: with delete notifications off
// and the delete followed at once by the read, the extents, initialized size, and bytes of each
// stream must come back exactly as they were while the file was live. One file of each shape that
// stores its data differently: fragmented, sparse with a hole and an initialized size below the
// size, a non-resident alternate stream next to a resident default one, and a resident file.
#[test]
fn a_deleted_file_reads_the_extents_sizes_and_bytes_it_had_while_live() {
    const NAME: &str = "a_deleted_file_reads_the_extents_sizes_and_bytes_it_had_while_live";
    let _serial = serial();
    let Some(_notify) = NotifyGuard::set(NAME, true) else {
        return;
    };
    let dir = fixture_dir("content");

    // Fragmented: two files grown in turn, each piece flushed, so the allocator interleaves them.
    let (path, other) = (
        dir.path().join("fragmented.bin"),
        dir.path().join("other.bin"),
    );
    {
        let (mut a, mut b) = (File::create(&path).unwrap(), File::create(&other).unwrap());
        for piece in 0..96u64 {
            a.write_all(&tagged(VICTIM_TAG, piece * 128, 128)).unwrap();
            a.sync_all().unwrap();
            b.write_all(&tagged(FILLER_TAG, piece * 128, 128)).unwrap();
            b.sync_all().unwrap();
        }
    }
    fs::remove_file(&other).unwrap();
    let wrote = tagged(VICTIM_TAG, 0, 96 * 128);
    let live = deleted_streams_are_what_they_were("fragmented", &path, &[None], &[&wrote]);
    if live[0].extents.len() < 2 {
        skip(NAME, "the fragmented file was not fragmented (one extent)");
    }

    // Sparse, with a hole and data after it, extended past what was written.
    let path = dir.path().join("sparse.bin");
    let piece = tagged(VICTIM_TAG, 0, MIB / SECTOR as u64);
    let mut wrote = vec![0u8; 96 * MIB as usize];
    wrote[..piece.len()].copy_from_slice(&piece);
    wrote[32 * MIB as usize..][..piece.len()].copy_from_slice(&piece);
    {
        let mut file = File::create(&path).unwrap();
        mark_sparse(&file);
        file.write_all(&piece).unwrap();
        file.seek(SeekFrom::Start(32 * MIB)).unwrap();
        file.write_all(&piece).unwrap();
        file.set_len(96 * MIB).unwrap();
        file.sync_all().unwrap();
    }
    let live = deleted_streams_are_what_they_were("sparse", &path, &[None], &[&wrote]);
    assert!(
        live[0].initialized_size < live[0].size,
        "the sparse file was fully initialized"
    );
    assert!(
        live[0]
            .extents
            .iter()
            .any(|extent| extent.location == ExtentLocation::Sparse),
        "the sparse file has no hole: {:?}",
        live[0].extents
    );

    // A resident default stream with a non-resident alternate one.
    let path = dir.path().join("streams.txt");
    let (default_bytes, big_bytes) = (b"the resident default stream", tagged(VICTIM_TAG, 0, 600));
    fs::write(&path, default_bytes).unwrap();
    fs::write(PathBuf::from(format!("{}:big", path.display())), &big_bytes).unwrap();
    let live = deleted_streams_are_what_they_were(
        "streams",
        &path,
        &[None, Some("big")],
        &[default_bytes, &big_bytes],
    );
    assert_eq!(live[0].extents.len(), 1);
    assert_eq!(live[0].extents[0].location, ExtentLocation::Resident);
    assert!(
        matches!(live[1].extents[0].location, ExtentLocation::Volume { .. }),
        "the alternate stream is not stored in clusters: {:?}",
        live[1].extents
    );

    // A resident file: its 100 bytes are in the record.
    let path = dir.path().join("resident.txt");
    fs::write(&path, vec![b'r'; 100]).unwrap();
    let live = deleted_streams_are_what_they_were("resident", &path, &[None], &[&[b'r'; 100]]);
    assert_eq!(live[0].bytes, vec![b'r'; 100]);
    assert_eq!(live[0].extents[0].location, ExtentLocation::Resident);
}

const STREAMS: usize = 30;
const LINKS: usize = 30;

// A file with an `$ATTRIBUTE_LIST` (30 streams, 30 hard links) loses the size and the runs of its
// non-resident streams when it is deleted. Such a stream opens as an empty one, which is what
// `stream_data_lost` is for; the resident streams are still there.
#[test]
fn a_deleted_file_with_an_attribute_list_says_its_data_is_lost() {
    let _serial = serial();
    let dir = fixture_dir("attribute-list");
    let target = dir.path().join("target.bin");
    fs::write(&target, tagged(VICTIM_TAG, 0, 128)).unwrap();
    for index in 0..STREAMS {
        fs::write(
            PathBuf::from(format!("{}:stream{index:02}", target.display())),
            vec![b'x'; 100],
        )
        .unwrap();
    }
    let links: Vec<PathBuf> = (0..LINKS)
        .map(|index| {
            let link = dir
                .path()
                .join(format!("hardlink-{index:02}-{}", "n".repeat(100)));
            fs::hard_link(&target, &link).unwrap();
            link
        })
        .collect();
    let number = record_number_of(&target);
    let live = load();
    assert!(!live.record(number).unwrap().stream_data_lost());
    let live_size = live
        .record(number)
        .unwrap()
        .open_stream(None)
        .unwrap()
        .size();
    assert_eq!(live_size, 128 * SECTOR as u64);
    drop(live);

    fs::remove_file(&target).unwrap();
    for link in links.iter().skip(1) {
        fs::remove_file(link).unwrap();
    }
    fs::remove_file(&links[0]).unwrap();

    let mft = load_freed(number);
    let file = deleted(&mft, number);
    assert!(file.stream_data_lost());
    // The default stream opens (it is not "not found"), empty, and says its data was lost.
    let stream = file
        .open_stream(None)
        .expect("the default stream of a file that lost its runs still opens");
    assert_eq!(stream.size(), 0, "the data of the stream was zeroed");
    assert!(stream.extents().is_empty());
    assert!(stream.data_lost());
    let bitmap = ClusterBitmap::new(&mft).unwrap();
    assert_eq!(
        stream.allocation(&bitmap).unwrap().state(),
        AllocationState::Lost
    );
    // The resident streams of the same file are intact and read as they were.
    let resident: Vec<_> = file
        .data_streams()
        .filter_map(|stream| stream.name.filter(|_| stream.size == 100))
        .collect();
    eprintln!(
        "attribute list: {} resident streams readable",
        resident.len()
    );
    assert!(!resident.is_empty(), "no resident stream survived");
    for name in resident {
        let mut stream = file.open_stream(Some(&name)).unwrap();
        assert_eq!(stream.extents().len(), 1);
        assert_eq!(stream.extents()[0].location, ExtentLocation::Resident);
        assert_eq!(read_all(&mut stream), vec![b'x'; 100], "{name:?}");
    }
}

// Live files report the same shape, everything allocated, and the other kinds of bytes are
// counted apart: resident, holes, and the part past the initialized size.
#[test]
fn live_sparse_resident_and_uninitialized_streams_are_counted_by_kind() {
    let _serial = serial();
    let dir = fixture_dir("live");

    let mut whole = File::create(dir.path().join("whole.bin")).unwrap();
    whole
        .write_all(&tagged(VICTIM_TAG, 0, VICTIM_SECTORS))
        .unwrap();
    whole.sync_all().unwrap();
    fs::write(dir.path().join("small.txt"), b"a resident file").unwrap();

    let mut sparse = File::create(dir.path().join("sparse.bin")).unwrap();
    mark_sparse(&sparse);
    sparse
        .write_all(&tagged(VICTIM_TAG, 0, 2 * MIB / SECTOR as u64))
        .unwrap();
    sparse.seek(SeekFrom::Start(32 * MIB)).unwrap();
    sparse
        .write_all(&tagged(VICTIM_TAG, 0, 2 * MIB / SECTOR as u64))
        .unwrap();
    sparse.set_len(96 * MIB).unwrap();
    sparse.sync_all().unwrap();

    // SetEndOfFile allocates clusters past what was written, which read as zeroes.
    let mut extended = File::create(dir.path().join("extended.bin")).unwrap();
    extended
        .write_all(&tagged(VICTIM_TAG, 0, MIB / SECTOR as u64))
        .unwrap();
    extended.set_len(10 * MIB).unwrap();
    extended.sync_all().unwrap();
    drop((whole, sparse, extended));

    let mft = load();
    let bitmap = ClusterBitmap::new(&mft).unwrap();
    let volume = mft.volume();
    // The cluster count is checked against the volume's own figures in `tests/bitmap_tests.rs`.
    assert_eq!(bitmap.cluster_size(), volume.cluster_size());
    assert!(!bitmap.is_allocated(bitmap.cluster_count()));
    let dir_name = "recoverability-live";
    let open = |name: &str| {
        find(&mft, dir_name, name)
            .open_stream(None)
            .unwrap_or_else(|e| panic!("{name}: {e}"))
    };

    // A live file: every stored byte is in an allocated cluster, and so is every run the bitmap
    // is asked about directly.
    let whole = open("whole.bin");
    let allocation = whole
        .allocation(&bitmap)
        .expect("the bitmap of the same volume");
    assert_parts_add_up(&allocation);
    assert_eq!(
        allocation.state(),
        AllocationState::Allocated,
        "{allocation:?}"
    );
    assert_eq!(allocation.in_allocated_clusters(), whole.size());
    assert_eq!(allocation.in_free_clusters(), 0);
    for extent in whole.extents() {
        let ExtentLocation::Volume { offset } = extent.location else {
            panic!("a written file has its bytes in clusters: {extent:?}");
        };
        let first = offset / volume.cluster_size();
        let last = (offset + extent.length - 1) / volume.cluster_size();
        for cluster in first..=last {
            assert!(bitmap.is_allocated(cluster), "cluster {cluster}");
        }
    }

    // A small file lives in its record.
    let small = open("small.txt");
    let allocation = small
        .allocation(&bitmap)
        .expect("the bitmap of the same volume");
    assert_parts_add_up(&allocation);
    assert_eq!(allocation.resident(), small.size());
    assert_eq!(allocation.state(), AllocationState::NoStoredData);

    // Holes are counted as such, the written parts are allocated. The file was written at 0..2 MiB
    // and 32..34 MiB and then extended to 96 MiB, so its initialized size is 34 MiB: the hole in
    // the middle is sparse, and everything from the initialized size on is `beyond_initialized`,
    // which takes precedence over `sparse` (the two add up to the 92 MiB that are not data).
    let sparse = open("sparse.bin");
    let allocation = sparse
        .allocation(&bitmap)
        .expect("the bitmap of the same volume");
    assert_parts_add_up(&allocation);
    assert_eq!(allocation.size(), 96 * MIB);
    assert!(
        allocation.sparse() >= 28 * MIB,
        "the hole in the middle: {allocation:?}"
    );
    assert!(
        allocation.sparse() + allocation.beyond_initialized() >= 90 * MIB,
        "{allocation:?}"
    );
    assert!(
        allocation.in_allocated_clusters() >= 4 * MIB,
        "{allocation:?}"
    );
    assert_eq!(allocation.in_free_clusters(), 0);
    assert_eq!(allocation.state(), AllocationState::Allocated);

    // Past the initialized size the clusters are allocated but not the file's data.
    let extended = open("extended.bin");
    let allocation = extended
        .allocation(&bitmap)
        .expect("the bitmap of the same volume");
    assert_parts_add_up(&allocation);
    assert_eq!(allocation.size(), 10 * MIB);
    assert!(extended.initialized_size() < extended.size());
    assert_eq!(
        allocation.in_allocated_clusters(),
        extended.initialized_size()
    );
    assert_eq!(
        allocation.beyond_initialized(),
        extended.size() - extended.initialized_size()
    );
    assert_eq!(allocation.state(), AllocationState::Allocated);
}
