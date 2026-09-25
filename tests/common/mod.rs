//! Shared helpers for integration tests and benches. Not every binary that
//! includes this module uses every helper.
#![allow(dead_code)]

use std::env;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use ntfs_reader::{Journal, Mft, NtfsReaderResult};

/// Letter of the NTFS volume the integration tests and benches may write to.
///
/// Must be disposable: tests create large files and some recreate the USN journal (`fsutil usn
/// deletejournal /D`). Comes from `NTFS_READER_TEST_VOLUME` (a drive letter, `T` or `T:`);
/// panics if unset or invalid. The system drive is refused too, unless
/// `NTFS_READER_ALLOW_SYSTEM_DRIVE=1` is set (for disposable CI runners).
pub fn test_volume_letter() -> String {
    volume_letter(
        "NTFS_READER_TEST_VOLUME",
        "The integration tests write large files and recreate the USN journal, so they only run \
         on a volume you name explicitly, for example `set NTFS_READER_TEST_VOLUME=T` for a \
         disposable NTFS volume T:",
        "the tests would delete and recreate its USN journal. Use a disposable volume, or set \
         NTFS_READER_ALLOW_SYSTEM_DRIVE=1 on a throwaway machine",
    )
}

/// Letter of the NTFS volume the Win32 parity test reads (`NTFS_READER_PARITY_VOLUME`, same
/// format and system-drive rule as [`test_volume_letter`]). Read-only, so any volume works: the
/// stress volume `S:`, or a real one like `C:` with `NTFS_READER_ALLOW_SYSTEM_DRIVE=1`.
pub fn parity_volume_letter() -> String {
    volume_letter(
        "NTFS_READER_PARITY_VOLUME",
        "The Win32 parity test compares the crate with Win32 on the volume you name, for example \
         `set NTFS_READER_PARITY_VOLUME=S`",
        "the parity test only reads it, but the system drive changes while it runs. Set \
         NTFS_READER_ALLOW_SYSTEM_DRIVE=1 to accept that",
    )
}

fn volume_letter(variable: &str, unset_hint: &str, system_drive_hint: &str) -> String {
    let raw = env::var(variable).unwrap_or_else(|_| panic!("{variable} is not set. {unset_hint}"));
    let letter = raw.trim().trim_end_matches(':');
    let mut chars = letter.chars();
    let (Some(letter), None) = (chars.next(), chars.next()) else {
        panic!("{variable}={raw:?} is not a drive letter (expected e.g. `T`)");
    };
    if !letter.is_ascii_alphabetic() {
        panic!("{variable}={raw:?} is not a drive letter (expected e.g. `T`)");
    }
    let letter = letter.to_ascii_uppercase();

    let system_drive = env::var("SystemDrive")
        .ok()
        .and_then(|s| s.chars().next())
        .map(|c| c.to_ascii_uppercase());
    let allowed = env::var("NTFS_READER_ALLOW_SYSTEM_DRIVE").is_ok_and(|v| v == "1");
    if system_drive == Some(letter) && !allowed {
        panic!("{variable}={letter} is the system drive; {system_drive_hint}");
    }

    letter.to_string()
}

/// Whether `NTFS_READER_REQUIRE_ALL=1` is set: a test that cannot reach the state it checks then
/// fails instead of reporting a skip (the maintainer's VM run sets it for the normal pass).
pub fn require_all() -> bool {
    env::var("NTFS_READER_REQUIRE_ALL").is_ok_and(|v| v == "1")
}

/// Whether `NTFS_READER_ALLOW_FSUTIL=1` is set: the test may make a persistent change outside the
/// test volume. Two things do: `fsutil behavior set DisableDeleteNotify` (system-wide, survives
/// reboots) and `EncryptFileW` (creates an EFS certificate and key in the user profile if none
/// exists). Set on the maintainer's VM; unset on a dev machine, where those tests skip instead.
pub fn allow_fsutil() -> bool {
    env::var("NTFS_READER_ALLOW_FSUTIL").is_ok_and(|v| v == "1")
}

/// Appends `line` to the file `NTFS_READER_SKIP_LOG` names, when it names one.
fn append_to_skip_log(line: &str) {
    use std::io::Write;

    if let Some(log) = env::var_os("NTFS_READER_SKIP_LOG") {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
        {
            let _ = writeln!(file, "{line}");
        }
    }
}

/// Reports that `test` could not check what it is for, then returns so the test ends. Prints
/// `SKIPPED: <test>: <reason>` and, if `NTFS_READER_SKIP_LOG` names a file, appends the line
/// there too (libtest hides a passing test's output, so the maintainer's VM run counts these
/// lines instead). Panics under `NTFS_READER_REQUIRE_ALL=1`: a skip must not look like a pass.
///
/// Use [`skip_environment`] instead for a test that cannot run because of the machine, not a
/// missing fixture or an unreached state.
#[track_caller]
pub fn skip(test: &str, reason: &str) {
    let line = format!("SKIPPED: {test}: {reason}");
    eprintln!("{line}");
    append_to_skip_log(&line);
    if require_all() {
        panic!("{test} could not run and NTFS_READER_REQUIRE_ALL=1 turns that into a failure: {reason}");
    }
}

/// Reports that `test` could not check what it is for because of the ENVIRONMENT, then returns so
/// the test ends. Prints `SKIPPED (environment): <test>: <reason>` and appends it to
/// `NTFS_READER_SKIP_LOG` with an `[env]` marker, counted apart from other skips. Unlike [`skip`],
/// does NOT fail under `NTFS_READER_REQUIRE_ALL=1`.
///
/// For a machine property the crate cannot control and a retry will not change: whether TRIM
/// reaches a virtual disk depends on the disk and its load (measured on the test VM: a lone delete
/// followed by raw reads saw freed clusters zeroed at 10s, 1011 of 1024; the same wait after
/// heavy fills in one process saw none in 30s on the same disk). Such a test only shows crate
/// behavior when the environment cooperates (here: that `Free` can hide zeroes), so a quiet disk
/// must not fail it, though the skip stays visible in the log. A skip caused by the test itself
/// (a missing fixture, a fill that missed the victim) goes through [`skip`] instead.
pub fn skip_environment(test: &str, reason: &str) {
    let line = format!("SKIPPED (environment): {test}: {reason}");
    eprintln!("{line}");
    append_to_skip_log(&format!("[env] {line}"));
}

/// The record number of `$Extend` (a fixed system record).
const EXTEND_RECORD: u64 = 11;

/// The record number of `$Extend\$Deleted`, where NTFS moves a file deleted while something
/// holds it open, and where `remove_dir_all` renames a directory before it deletes it. `None`
/// when the volume has not needed the directory yet.
pub fn deleted_directory(mft: &Mft) -> Option<u64> {
    (EXTEND_RECORD + 1..mft.record_count()).find(|&number| {
        mft.record(number).is_some_and(|file| {
            file.is_used()
                && file.is_directory()
                && file.names().any(|name| {
                    name.parent_number() == EXTEND_RECORD && name.to_string() == "$Deleted"
                })
        })
    })
}

/// The parent directory record numbers of the names of record `number` (sorted, without repeats,
/// so a DOS alias does not count twice). Empty when the record is not in `mft` or has no name.
pub fn parents_of(mft: &Mft, number: u64) -> Vec<u64> {
    let mut parents: Vec<u64> = mft
        .record(number)
        .map(|file| file.names().map(|name| name.parent_number()).collect())
        .unwrap_or_default();
    parents.sort_unstable();
    parents.dedup();
    parents
}

/// The best name of the file whose base record is `number`, for a message.
fn name_of(mft: &Mft, number: u64) -> String {
    mft.record(number)
        .and_then(|file| file.best_name())
        .map_or_else(
            || "<no name>".to_string(),
            |name| format!("{:?}", name.to_string()),
        )
}

/// What `mft` says about record `number`, for a failure message: whether it is in use and which
/// directories its names sit in. A record in use whose parent is `$Extend\$Deleted` is a file that
/// was deleted while something held it open (delete-pending): it is freed only when that handle
/// closes.
pub fn describe_record(mft: &Mft, number: u64) -> String {
    let Some(file) = mft.record(number) else {
        return format!("record {number} is not in the MFT");
    };
    let parents = parents_of(mft, number);
    let deleted = deleted_directory(mft);
    let pending = deleted.is_some_and(|deleted| parents.contains(&deleted));
    // The file this record belongs to, and whether it is only an extension record (a file that
    // grows in many pieces takes extra records for its extents).
    let owner = match file.base_number() {
        Some(base) => format!(
            "an EXTENSION record of record {base} ({})",
            name_of(mft, base)
        ),
        None => format!("the base record of {}", name_of(mft, number)),
    };
    format!(
        "record {number} ({owner}): in use {}, allocated in $MFT's bitmap {}, its names sit under \
         record(s) {parents:?} ($Extend\\$Deleted is record {deleted:?}{})",
        file.is_used(),
        mft.is_allocated(number),
        if pending && file.is_used() {
            ": delete-pending, something still holds the file open"
        } else {
            ""
        }
    )
}

/// The record number part of a file reference or id.
pub const RECORD_NUMBER_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// The file reference (sequence number in the high 16 bits, record number below) Win32 reports for
/// `path`, which may be a directory.
pub fn reference_of(path: &Path) -> u64 {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
    };

    let file = std::fs::OpenOptions::new()
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

/// Flush the volume's write cache so a raw read of the MFT that follows sees files just
/// created. Call it after writing fixtures and before `Mft::new`.
pub fn flush_volume(letter: &str) {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FlushFileBuffers, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };

    let path = HSTRING::from(format!("\\\\.\\{letter}:"));
    unsafe {
        let handle = CreateFileW(
            &path,
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
        .unwrap_or_else(|e| panic!("open volume {letter}: to flush it: {e}"));
        let flushed = FlushFileBuffers(handle);
        let _ = CloseHandle(handle);
        flushed.unwrap_or_else(|e| panic!("flush volume {letter}: {e}"));
    }
}

/// Read until the journal reports it has reached its end, discarding the records: the position
/// is then the current end. Stops on `caught_up`, not on an empty page (a page can be empty
/// because the reason mask filtered everything in its window).
pub fn drain(journal: &mut Journal) -> NtfsReaderResult<()> {
    while !journal.read()?.caught_up {}
    Ok(())
}

/// Installs a trace-level `tracing_subscriber::FmtSubscriber` as the global default, for tests
/// that want to see the crate's `tracing` output on failure. Safe to call from more than one
/// test in the same binary: `set_global_default` only ever takes effect once, and later calls
/// are ignored.
pub fn init_tracing() {
    let subscriber = tracing_subscriber::FmtSubscriber::builder()
        .with_max_level(tracing::Level::TRACE)
        .without_time()
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
}

pub struct TempDirGuard(pub PathBuf);

impl TempDirGuard {
    pub fn new<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        let p = path.as_ref().to_path_buf();
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p)?;
        Ok(TempDirGuard(p))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
