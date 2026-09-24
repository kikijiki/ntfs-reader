#![cfg(target_os = "windows")]

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Write;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use ntfs_reader::{DefaultPathCache, FileInfo, Mft, NtfsAttributeType, NtfsReaderResult, Volume};

mod common;
use common::{flush_volume, test_volume_letter, TempDirGuard};
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED, HANDLE,
};
use windows::Win32::System::Ioctl::FSCTL_SET_SPARSE;
use windows::Win32::System::IO::DeviceIoControl;

// One load of the real `$MFT`: it has records, record 0 is there, and nothing from `record_count`
// on (including the largest number) exists.
#[test]
fn a_loaded_mft_has_records_and_stops_at_record_count() -> NtfsReaderResult<()> {
    let vol = Volume::new(format!("\\\\.\\{}:", test_volume_letter()))?;
    let mft = Mft::new(vol)?;

    assert!(mft.record_count() > 0);
    assert!(mft.size_in_memory() > 0);
    assert!(mft.is_allocated(0));
    assert!(mft.record(0).is_some());

    assert!(!mft.is_allocated(mft.record_count()));
    assert!(mft.record(mft.record_count()).is_none());
    assert!(!mft.is_allocated(mft.record_count() + 1));
    assert!(mft.record(mft.record_count() + 1).is_none());
    assert!(!mft.is_allocated(u64::MAX));

    Ok(())
}

#[test]
fn files_discovers_temp_artifacts() -> NtfsReaderResult<()> {
    let dir_name = "mft-files".to_string();
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
    let big_key = format!("{}\\{}", dir_name, "big-1G.bin");
    expected.insert(big_key.clone());

    let sparse_path = dir.join("sparse.bin");
    let sparse_file = File::create(&sparse_path)?;
    mark_sparse(&sparse_file)?;
    sparse_file.set_len(2_000_000_000)?;
    expected.insert(format!("{}\\{}", dir_name, "sparse.bin"));
    let sparse_key = format!("{}\\{}", dir_name, "sparse.bin");

    flush_volume(&test_volume_letter());
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
    let mut big_checked = false;
    let mut sparse_checked = false;
    for file in mft.files() {
        if !file.is_directory() {
            let info = FileInfo::new(&file);
            if let Some(key) = info.path.as_deref().and_then(to_rel_key) {
                if key == big_key {
                    assert_eq!(info.size, 1_000_000_000, "big file: FileInfo size");
                    assert_eq!(
                        file.data_streams()
                            .find(|stream| stream.name.is_none())
                            .map(|stream| stream.size),
                        Some(1_000_000_000)
                    );
                    big_checked = true;
                }
                if key == sparse_key {
                    let data_att = file
                        .attributes()
                        .find(|att| att.attribute_type() == Some(NtfsAttributeType::Data))
                        .expect("sparse file missing data attribute");
                    assert!(
                        !data_att.is_resident(),
                        "sparse file data attribute should be non-resident"
                    );
                    // Nothing on the volume can tell a sparse extent from a
                    // dense one but the runs themselves, which only the
                    // `internals` feature exposes.
                    #[cfg(feature = "internals")]
                    {
                        use ntfs_reader::internals::{nonresident_data_runs, DataRun};

                        let (total_size, runs) = nonresident_data_runs(&data_att, mft.volume())
                            .expect("failed to parse sparse data runs");
                        assert_eq!(total_size, 2_000_000_000);
                        assert!(
                            matches!(runs.as_slice(), [DataRun::Sparse { length }] if *length >= total_size),
                            "expected a sparse run covering the file, got {:?}",
                            runs
                        );
                    }
                    assert_eq!(
                        file.data_streams()
                            .find(|stream| stream.name.is_none())
                            .map(|stream| stream.size),
                        Some(2_000_000_000)
                    );
                    sparse_checked = true;
                }
                found.insert(key);
            }
        }
    }

    for key in &expected {
        assert!(
            found.contains(key),
            "Did not find created path '{}' via files() (found: {:?})",
            key,
            found
        );
    }

    assert!(big_checked, "The big file's size was not checked");
    assert!(
        sparse_checked,
        "Sparse file nonresident runs were not validated"
    );

    Ok(())
}

#[test]
fn hard_links_and_alternate_streams() -> NtfsReaderResult<()> {
    let dir_name = "mft-hard-links".to_string();
    let dir = PathBuf::from(format!("\\\\?\\{}:\\{}", test_volume_letter(), dir_name));
    let _cleanup = TempDirGuard::new(&dir)?;

    let original = dir.join("original.txt");
    fs::write(&original, b"contents")?;
    fs::create_dir(dir.join("sub"))?;
    fs::hard_link(&original, dir.join("sub").join("linked.txt"))?;
    fs::write(dir.join("original.txt:extra"), b"stream data")?;

    flush_volume(&test_volume_letter());
    let vol = Volume::new(format!("\\\\.\\{}:", test_volume_letter()))?;
    let mft = Mft::new(vol)?;

    let mut cache = DefaultPathCache::new();
    // Card 016: match on hard_links() rather than best_name(), which
    // depends on $FILE_NAME order and is not guaranteed.
    let file = mft
        .files()
        .find(|file| {
            file.hard_links().any(|link| {
                link.to_string() == "original.txt"
                    && mft
                        .resolve_path(&link, &mut cache)
                        .is_some_and(|path| path.to_string_lossy().contains(&dir_name))
            })
        })
        .expect("original.txt not found");

    let mut links: Vec<String> = file
        .hard_links()
        .filter_map(|link| mft.resolve_path(&link, &mut cache))
        .map(|path| {
            let path = path.to_string_lossy();
            path[path.find(&dir_name).expect("link outside test dir")..].to_string()
        })
        .collect();
    links.sort();
    assert_eq!(
        links,
        [
            format!("{dir_name}\\original.txt"),
            format!("{dir_name}\\sub\\linked.txt"),
        ]
    );

    let streams: Vec<_> = file
        .data_streams()
        .map(|stream| (stream.name, stream.size))
        .collect();
    assert_eq!(streams, [(None, 8), (Some("extra".into()), 11)]);

    Ok(())
}

// Card 013. Names whose $FILE_NAME carries the reparse-point flag (a
// junction, a file symlink, a directory symlink) must still get a best_name
// and a resolvable path.
#[test]
fn reparse_points_get_names_and_paths() -> NtfsReaderResult<()> {
    let dir_name = "mft-reparse".to_string();
    let dir = PathBuf::from(format!("\\\\?\\{}:\\{}", test_volume_letter(), dir_name));
    let _cleanup = TempDirGuard::new(&dir)?;

    let junction_target = dir.join("junction-target");
    fs::create_dir(&junction_target)?;
    fs::write(junction_target.join("child.txt"), b"child")?;
    let junction = dir.join("junction-link");
    let status = std::process::Command::new("cmd")
        .args([
            "/c",
            "mklink",
            "/J",
            junction.to_str().expect("path is valid unicode"),
            junction_target.to_str().expect("path is valid unicode"),
        ])
        .status()?;
    assert!(status.success(), "mklink /J failed with status {status}");

    let file_target = dir.join("file-target.txt");
    fs::write(&file_target, b"target")?;
    let file_symlink = dir.join("file-symlink.txt");
    std::os::windows::fs::symlink_file(&file_target, &file_symlink)?;

    let dir_symlink_target = dir.join("dirlink-target");
    fs::create_dir(&dir_symlink_target)?;
    fs::write(dir_symlink_target.join("child.txt"), b"child")?;
    let dir_symlink = dir.join("dir-symlink");
    std::os::windows::fs::symlink_dir(&dir_symlink_target, &dir_symlink)?;

    flush_volume(&test_volume_letter());
    let vol = Volume::new(format!("\\\\.\\{}:", test_volume_letter()))?;
    let mft = Mft::new(vol)?;

    for expected_name in ["junction-link", "file-symlink.txt", "dir-symlink"] {
        let file = mft.files().find(|file| {
            file.best_name()
                .is_some_and(|name| name.to_string() == expected_name)
        });
        assert!(
            file.is_some(),
            "{expected_name} got no best_name via files()",
        );
        let file = file.expect("checked above");

        let info = FileInfo::new(&file);
        assert_eq!(info.name, expected_name, "{expected_name}: FileInfo name");
        let path = info
            .path
            .as_deref()
            .unwrap_or_else(|| panic!("{expected_name}: FileInfo got no path"));
        assert!(
            path.ends_with(Path::new(&dir_name).join(expected_name)),
            "{expected_name}: path {path:?} does not end in {dir_name}\\{expected_name}",
        );
    }

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
