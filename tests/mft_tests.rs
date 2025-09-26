#![cfg(target_os = "windows")]

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Write;
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;

use ntfs_reader::api::NtfsAttributeType;
use ntfs_reader::attribute::DataRun;
use ntfs_reader::errors::NtfsReaderResult;
use ntfs_reader::file_info::FileInfo;
use ntfs_reader::mft::Mft;
use ntfs_reader::test_utils::{test_volume_letter, TempDirGuard};
use ntfs_reader::volume::Volume;
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED, HANDLE,
};
use windows::Win32::System::Ioctl::FSCTL_SET_SPARSE;
use windows::Win32::System::IO::DeviceIoControl;

#[test]
fn mft_creation() -> NtfsReaderResult<()> {
    let vol = Volume::new(format!("\\\\.\\{}:", test_volume_letter()))?;
    let mft = Mft::new(vol)?;

    assert!(mft.max_record > 0);
    assert!(!mft.data.is_empty());
    assert!(!mft.bitmap.is_empty());

    Ok(())
}

#[test]
fn record_exists_boundaries() -> NtfsReaderResult<()> {
    let vol = Volume::new(format!("\\\\.\\{}:", test_volume_letter()))?;
    let mft = Mft::new(vol)?;

    assert!(mft.record_exists(0));
    assert!(!mft.record_exists(u64::MAX));
    assert!(!mft.record_exists(mft.max_record + 1));

    Ok(())
}

#[test]
fn get_record_bounds() -> NtfsReaderResult<()> {
    let vol = Volume::new(format!("\\\\.\\{}:", test_volume_letter()))?;
    let mft = Mft::new(vol)?;

    assert!(mft.get_record(0).is_some());
    assert!(mft.get_record(mft.max_record + 1).is_none());

    Ok(())
}

#[test]
fn iterate_files_discovers_temp_artifacts() -> NtfsReaderResult<()> {
    let dir_name = format!("mft-iterate-files-{}", std::process::id());
    let dir = PathBuf::from(format!("\\\\?\\{}:\\{}", test_volume_letter(), dir_name));

    let _cleanup = TempDirGuard::new(&dir)?;

    let file_names = ["iter-a.txt", "iter-b.txt", "iter-c.txt"];
    let mut expected: HashSet<String> = HashSet::new();
    for name in &file_names {
        let path = dir.join(name);
        let mut f = File::create(&path)?;
        f.write_all(b"hello")?;
        expected.insert(format!("{}\\{}", dir_name, name));
    }

    let mut cur = dir.clone();
    let mut rel_parts = vec![dir_name.clone()];
    for i in 1..=10u32 {
        let comp = format!("deep_{i:02}");
        cur.push(&comp);
        rel_parts.push(comp);
    }
    fs::create_dir_all(&cur)?;
    let deep_file = cur.join("deep.txt");
    File::create(&deep_file)?.write_all(b"deep")?;
    rel_parts.push("deep.txt".to_string());
    expected.insert(rel_parts.join("\\"));

    let long_stem = "L".repeat(200);
    let long_name = format!("{}.txt", long_stem);
    let long_path = dir.join(&long_name);
    File::create(&long_path)?.write_all(b"long")?;
    expected.insert(format!("{}\\{}", dir_name, long_name));

    let big_path = dir.join("big-1G.bin");
    let big = File::create(&big_path)?;
    big.set_len(1_000_000_000)?;
    expected.insert(format!("{}\\{}", dir_name, "big-1G.bin"));

    let sparse_path = dir.join("sparse.bin");
    let sparse_file = File::create(&sparse_path)?;
    mark_sparse(&sparse_file)?;
    sparse_file.set_len(2_000_000_000)?;
    expected.insert(format!("{}\\{}", dir_name, "sparse.bin"));
    let sparse_key = format!("{}\\{}", dir_name, "sparse.bin");

    let vol = Volume::new(format!("\\\\.\\{}:", test_volume_letter()))?;
    let mft = Mft::new(vol)?;

    let to_rel_key = |p: &std::path::Path| -> Option<String> {
        let mut segs = Vec::new();
        let mut seen_root = false;
        for comp in p.components() {
            if let std::path::Component::Normal(os) = comp {
                if !seen_root {
                    if os.to_string_lossy().eq_ignore_ascii_case(&dir_name) {
                        segs.push(dir_name.clone());
                        seen_root = true;
                    }
                } else {
                    segs.push(os.to_string_lossy().to_string());
                }
            }
        }
        if seen_root {
            Some(segs.join("\\"))
        } else {
            None
        }
    };

    let mut found = HashSet::new();
    let mut sparse_checked = false;
    mft.iterate_files(|file| {
        if !file.is_directory() && file.is_used() {
            let info = FileInfo::new(&mft, file);
            if let Some(key) = to_rel_key(&info.path) {
                if key == sparse_key {
                    let data_att = file
                        .get_attribute(NtfsAttributeType::Data)
                        .expect("sparse file missing data attribute");
                    assert!(
                        data_att.header.is_non_resident != 0,
                        "sparse file data attribute should be non-resident"
                    );
                    let (total_size, runs) = data_att
                        .get_nonresident_data_runs(&mft.volume)
                        .expect("failed to parse sparse data runs");
                    assert_eq!(total_size, 2_000_000_000);
                    assert!(
                        matches!(runs.as_slice(), [DataRun::Sparse { length }] if *length >= total_size),
                        "expected a sparse run covering the file, got {:?}",
                        runs
                    );
                    sparse_checked = true;
                }
                found.insert(key);
            }
        }
    });

    for key in &expected {
        assert!(
            found.contains(key),
            "Did not find created path '{}' via iterate_files (found: {:?})",
            key,
            found
        );
    }

    assert!(
        sparse_checked,
        "Sparse file nonresident runs were not validated"
    );

    Ok(())
}

fn mark_sparse(file: &File) -> std::io::Result<()> {
    let handle = HANDLE(file.as_raw_handle());
    let mut bytes_returned = 0u32;
    unsafe {
        DeviceIoControl(
            handle,
            FSCTL_SET_SPARSE,
            None,
            0,
            None,
            0,
            Some(&mut bytes_returned as *mut u32),
            None,
        )
        .map_err(|err| device_io_error(err, "FSCTL_SET_SPARSE"))?;
    }
    Ok(())
}

fn device_io_error(err: windows::core::Error, op: &str) -> std::io::Error {
    let hresult = err.code();
    let kind = if hresult == ERROR_ACCESS_DENIED.to_hresult() {
        std::io::ErrorKind::PermissionDenied
    } else if hresult == ERROR_INVALID_FUNCTION.to_hresult()
        || hresult == ERROR_NOT_SUPPORTED.to_hresult()
    {
        std::io::ErrorKind::Unsupported
    } else {
        std::io::ErrorKind::Other
    };

    std::io::Error::new(kind, format!("{op} failed: {err}"))
}
