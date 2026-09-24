//! Shared helpers for integration tests and benches. Not every binary that
//! includes this module uses every helper.
#![allow(dead_code)]

use std::env;
use std::path::{Path, PathBuf};

use ntfs_reader::{Journal, NtfsReaderResult};

/// Return the letter of the NTFS volume the integration tests and benches may write to.
///
/// These tests create large files, and some delete and recreate the volume's USN journal
/// (`fsutil usn deletejournal /D`), so the volume must be a disposable one and is never
/// guessed. It comes from the `NTFS_READER_TEST_VOLUME` environment variable (a drive letter,
/// `T` or `T:`), and this panics with a message saying so when the variable is unset or
/// invalid.
///
/// The system drive (`%SystemDrive%`) is refused too, unless `NTFS_READER_ALLOW_SYSTEM_DRIVE=1`
/// is set, which is meant for disposable CI runners.
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

/// Return the letter of the NTFS volume the Win32 parity test reads (`NTFS_READER_PARITY_VOLUME`,
/// same format and same system drive rule as [`test_volume_letter`]). The parity test only reads
/// the volume, so any volume works, the stress volume `S:` or a real one such as `C:` (with
/// `NTFS_READER_ALLOW_SYSTEM_DRIVE=1`).
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
