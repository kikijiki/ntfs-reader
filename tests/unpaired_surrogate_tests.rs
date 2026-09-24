#![cfg(target_os = "windows")]

//! Card 034. NTFS filenames are raw UTF-16 code unit sequences with no
//! requirement that they be *valid* UTF-16 - Windows accepts an unpaired
//! surrogate (0xD800..=0xDFFF), which `str::encode_utf16` could never
//! produce but a real file can have. This creates such a file directly
//! with `CreateFileW`, then confirms the crate finds it via the MFT and
//! resolves it to a path that actually reopens - not a lossy, corrupted
//! one `Display`/`to_string` would have produced.
//!
//! See `path::tests::resolve_path_preserves_a_name_with_an_unpaired_surrogate`
//! (`src/path.rs`) for the synthetic, VM-free version of this same check.

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

use ntfs_reader::{DefaultPathCache, FileInfo, Mft, Volume};

use windows::core::{Owned, PCWSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem;
use windows::Win32::Storage::FileSystem::{CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL};

mod common;
use common::{flush_volume, test_volume_letter, TempDirGuard};

#[test]
fn finds_and_reopens_a_file_with_an_unpaired_surrogate_name() {
    let letter = test_volume_letter();
    let dir = TempDirGuard::new(format!("{letter}:\\lone-surrogate-fixture"))
        .expect("create fixture directory");

    // "surrogate-" + an unpaired high surrogate + "-name.txt".
    let name_units: Vec<u16> = "surrogate-"
        .encode_utf16()
        .chain([0xD800u16])
        .chain("-name.txt".encode_utf16())
        .collect();
    let name = OsString::from_wide(&name_units);
    let full_path: PathBuf = dir.path().join(&name);

    let mut wide_path: Vec<u16> = full_path.as_os_str().encode_wide().collect();
    wide_path.push(0);

    let handle: Owned<HANDLE> = unsafe {
        Owned::new(
            FileSystem::CreateFileW(
                PCWSTR::from_raw(wide_path.as_ptr()),
                FileSystem::FILE_GENERIC_READ.0 | FileSystem::FILE_GENERIC_WRITE.0,
                FileSystem::FILE_SHARE_READ,
                None,
                CREATE_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
            .expect(
                "CreateFileW must accept a name with an unpaired surrogate - if this fails, \
                 Windows itself rejects such names and card 034's premise doesn't hold",
            ),
        )
    };
    drop(handle); // Closes the handle; the file stays on disk.

    // `resolve_path` prefixes paths with the volume path it was given, not the drive letter.
    let volume_path = format!("\\\\.\\{letter}:");
    let expected_path = PathBuf::from(&volume_path)
        .join(dir.path().strip_prefix(format!("{letter}:\\")).unwrap())
        .join(&name);

    flush_volume(&letter);
    let volume = Volume::new(&volume_path).expect("open volume");
    let mft = Mft::new(volume).expect("read MFT");

    let mut cache = DefaultPathCache::new();
    let found = mft.files().find_map(|file| {
        let info = FileInfo::with_cache(&file, &mut cache);
        (info.path.as_deref() == Some(expected_path.as_path())).then_some(info)
    });
    let info = found.unwrap_or_else(|| {
        let mut cache = DefaultPathCache::new();
        let nearby: Vec<_> = mft
            .files()
            .filter_map(|file| FileInfo::with_cache(&file, &mut cache).path)
            .filter(|path| path.to_string_lossy().contains("lone-surrogate-fixture"))
            .collect();
        panic!(
            "no file resolved to {expected_path:?} via the MFT - resolve_path likely still uses \
             a lossy conversion somewhere for a name with an unpaired surrogate; paths under the \
             fixture directory: {nearby:?}"
        )
    });

    // The real proof: the resolved path must point at the actual file, not
    // a lossy, non-existent lookalike.
    let path = info.path.expect("the file resolved to a path above");
    std::fs::File::open(&path)
        .unwrap_or_else(|e| panic!("resolved path {path:?} did not reopen: {e}"));
}
