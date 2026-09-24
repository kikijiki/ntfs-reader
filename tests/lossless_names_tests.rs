#![cfg(target_os = "windows")]

//! Card 036. Stream names and USN journal names are raw UTF-16 code unit
//! sequences like file names (card 034), so they can hold an unpaired
//! surrogate, which `String` cannot represent. These tests create such names
//! on a real volume and check that the crate reports them exactly: a
//! caller must be able to reopen `path:stream` with the reported stream name,
//! and to match a journal record's name against the name the file really has.
//!
//! Each test first creates the file or stream with `CreateFileW` directly, with
//! a message that says so if Windows itself rejects the name (then the card's
//! premise would not hold).
//!
//! See `file::tests::data_streams_report_a_stream_name_with_an_unpaired_surrogate_losslessly`
//! and `usn::tests::a_name_with_an_unpaired_surrogate_is_kept` for the synthetic, VM-free
//! versions of these checks.

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use ntfs_reader::{DefaultPathCache, FileInfo, Journal, JournalOptions, Mft, Reason, Volume};

use windows::core::{Owned, PCWSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    self, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_READ,
};

mod common;
use common::{drain, flush_volume, test_volume_letter, TempDirGuard};

/// `prefix` + an unpaired high surrogate + `suffix`: a name `str::encode_utf16` can never
/// produce, but NTFS accepts.
fn name_with_unpaired_surrogate(prefix: &str, suffix: &str) -> OsString {
    let units: Vec<u16> = prefix
        .encode_utf16()
        .chain([0xD800u16])
        .chain(suffix.encode_utf16())
        .collect();
    OsString::from_wide(&units)
}

/// Create (or truncate) `path`, which may name an alternate data stream. `what` names the
/// thing created, for the failure message when Windows refuses. The handle closes on return;
/// callers flush the volume (`flush_volume`) before a raw MFT read.
fn create(path: &Path, what: &str) {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let _handle: Owned<HANDLE> = unsafe {
        Owned::new(
            FileSystem::CreateFileW(
                PCWSTR::from_raw(wide.as_ptr()),
                FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
                FILE_SHARE_READ,
                None,
                CREATE_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
            .unwrap_or_else(|e| {
                panic!(
                    "CreateFileW rejected {what} {path:?}: {e}. If Windows itself refuses a name \
                     with an unpaired surrogate here, card 036's premise does not hold for this \
                     kind of name"
                )
            }),
        )
    };
}

#[test]
fn finds_and_reopens_a_stream_with_an_unpaired_surrogate_name() {
    let letter = test_volume_letter();
    let dir =
        TempDirGuard::new(format!("{letter}:\\lossless-names-ads")).expect("create fixture dir");

    let host = dir.path().join("host.txt");
    let stream_name = name_with_unpaired_surrogate("ads-", "-stream");
    let mut stream_path = host.clone().into_os_string();
    stream_path.push(":");
    stream_path.push(&stream_name);

    create(&host, "the host file");
    create(Path::new(&stream_path), "the alternate data stream");

    // `resolve_path` prefixes paths with the volume path it was given, not the drive letter.
    let volume_path = format!("\\\\.\\{letter}:");
    let expected_path = PathBuf::from(&volume_path)
        .join(dir.path().strip_prefix(format!("{letter}:\\")).unwrap())
        .join("host.txt");

    flush_volume(&letter);
    let volume = Volume::new(&volume_path).expect("open volume");
    let mft = Mft::new(volume).expect("read MFT");

    let mut cache = DefaultPathCache::new();
    let (file, info) = mft
        .files()
        .find_map(|file| {
            let info = FileInfo::with_cache(&file, &mut cache);
            (info.path.as_deref() == Some(expected_path.as_path())).then_some((file, info))
        })
        .unwrap_or_else(|| panic!("no file resolved to {expected_path:?} via the MFT"));

    let streams: Vec<_> = file.data_streams().collect();
    let named: Vec<_> = streams.iter().filter_map(|s| s.name.as_ref()).collect();
    assert!(
        named.contains(&&stream_name),
        "data_streams() did not report the stream named {stream_name:?}; named streams found: \
         {named:?} (all streams: {streams:?}) - the reported name is likely lossy (U+FFFD for \
         the unpaired surrogate)"
    );

    // The real proof: `path:stream` built from the reported name must reopen the stream.
    let reported = named
        .iter()
        .find(|name| name.to_string_lossy().starts_with("ads-"))
        .expect("a stream with the fixture's prefix");
    let mut reopen = info.path.clone().expect("path").into_os_string();
    reopen.push(":");
    reopen.push(reported);
    std::fs::File::open(&reopen)
        .unwrap_or_else(|e| panic!("stream path {reopen:?} did not reopen: {e}"));
}

/// Open a journal that only reports `reason_mask`, positioned at its current end.
fn journal_at_end(reason_mask: Reason) -> Journal {
    let options = JournalOptions {
        reason_mask,
        ..JournalOptions::default()
    };
    let volume = Volume::new(format!("\\\\?\\{}:", test_volume_letter())).expect("open volume");
    let mut journal = Journal::new(volume, options).expect("open journal");
    drain(&mut journal).expect("drain journal");
    journal
}

#[test]
fn journal_record_name_is_the_real_name_for_an_unpaired_surrogate() {
    let letter = test_volume_letter();
    let dir = TempDirGuard::new(format!("{letter}:\\lossless-names-usn-create"))
        .expect("create fixture dir");
    let mut journal = journal_at_end(Reason::FILE_CREATE);

    let name = name_with_unpaired_surrogate("usn-", "-name.txt");
    create(&dir.path().join(&name), "the file");

    let mut seen = Vec::new();
    for _ in 0..10 {
        for record in journal.read().expect("read journal").records {
            if record.name == name {
                return;
            }
            seen.push(record.name);
        }
    }
    panic!(
        "no journal record named {name:?}; record names seen: {seen:?} - the record name is \
         likely lossy (U+FFFD for the unpaired surrogate)"
    );
}

#[test]
fn match_rename_returns_the_real_old_name_for_an_unpaired_surrogate() {
    let letter = test_volume_letter();
    let dir = TempDirGuard::new(format!("{letter}:\\lossless-names-usn-rename"))
        .expect("create fixture dir");

    let old_name = name_with_unpaired_surrogate("old-", "-name.txt");
    let old_path = dir.path().join(&old_name);
    let new_path = dir.path().join("renamed.txt");
    create(&old_path, "the file");

    let mut journal = journal_at_end(Reason::RENAME_OLD_NAME | Reason::RENAME_NEW_NAME);
    std::fs::rename(&old_path, &new_path).expect("rename");

    let mut matched = Vec::new();
    for _ in 0..10 {
        for record in journal.read().expect("read journal").records {
            if record.reason.contains(Reason::RENAME_NEW_NAME) && record.name == "renamed.txt" {
                matched.push(journal.match_rename(&record));
            }
        }
        if !matched.is_empty() {
            break;
        }
    }

    // NTFS writes RENAME_NEW_NAME twice for one rename: at the rename, and
    // again with CLOSE when the handle closes. Each must give the old name.
    assert!(
        !matched.is_empty(),
        "no RENAME_NEW_NAME record for renamed.txt"
    );
    for old in matched {
        assert_eq!(
            old,
            Some(old_name.clone()),
            "match_rename must return the file's real old name"
        );
    }
}
