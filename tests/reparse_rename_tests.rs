#![cfg(target_os = "windows")]

// A fresh reparse point's $FILE_NAME may not carry the reparse flag yet; a rename or move may
// refresh it. Either way, a junction, file symlink, and directory symlink must keep a best_name
// and a path that resolves to where the item ends up after a rename and then a move.

use std::fs;
use std::path::{Path, PathBuf};

use ntfs_reader::{FileInfo, Mft, NtfsReaderResult, Volume};

mod common;
use common::{flush_volume, test_volume_letter, TempDirGuard};

#[test]
fn reparse_points_keep_names_and_paths_after_rename_and_move() -> NtfsReaderResult<()> {
    let dir_name = "mft-reparse-rename";
    let dir = PathBuf::from(format!("\\\\?\\{}:\\{}", test_volume_letter(), dir_name));
    let _cleanup = TempDirGuard::new(&dir)?;

    let moved_dir = dir.join("moved");
    fs::create_dir(&moved_dir)?;

    // Directory junction, with a child in its target.
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
    let junction_renamed = dir.join("junction-link-renamed");
    fs::rename(&junction, &junction_renamed)?;
    let junction_moved = moved_dir.join("junction-link-moved");
    fs::rename(&junction_renamed, &junction_moved)?;

    // File symlink.
    let file_target = dir.join("file-target.txt");
    fs::write(&file_target, b"target")?;
    let file_symlink = dir.join("file-symlink.txt");
    std::os::windows::fs::symlink_file(&file_target, &file_symlink)?;
    let file_symlink_renamed = dir.join("file-symlink-renamed.txt");
    fs::rename(&file_symlink, &file_symlink_renamed)?;
    let file_symlink_moved = moved_dir.join("file-symlink-moved.txt");
    fs::rename(&file_symlink_renamed, &file_symlink_moved)?;

    // Directory symlink, with a child in its target.
    let dir_symlink_target = dir.join("dirlink-target");
    fs::create_dir(&dir_symlink_target)?;
    fs::write(dir_symlink_target.join("child.txt"), b"child")?;
    let dir_symlink = dir.join("dir-symlink");
    std::os::windows::fs::symlink_dir(&dir_symlink_target, &dir_symlink)?;
    let dir_symlink_renamed = dir.join("dir-symlink-renamed");
    fs::rename(&dir_symlink, &dir_symlink_renamed)?;
    let dir_symlink_moved = moved_dir.join("dir-symlink-moved");
    fs::rename(&dir_symlink_renamed, &dir_symlink_moved)?;

    flush_volume(&test_volume_letter());
    let vol = Volume::new(format!("\\\\.\\{}:", test_volume_letter()))?;
    let mft = Mft::new(vol)?;

    for expected_name in [
        "junction-link-moved",
        "file-symlink-moved.txt",
        "dir-symlink-moved",
    ] {
        let file = mft
            .files()
            .find(|file| file.names().any(|name| name.to_string() == expected_name))
            .unwrap_or_else(|| panic!("{expected_name}: no record found by any name"));

        let best_name = file
            .best_name()
            .unwrap_or_else(|| panic!("{expected_name}: names exist but best_name is None"));
        assert_eq!(
            best_name.to_string(),
            expected_name,
            "{expected_name}: best_name"
        );

        let info = FileInfo::new(&file);
        assert_eq!(info.name, expected_name, "{expected_name}: FileInfo name");
        let path = info
            .path
            .as_deref()
            .unwrap_or_else(|| panic!("{expected_name}: FileInfo got no path"));
        assert!(
            path.ends_with(Path::new(dir_name).join("moved").join(expected_name)),
            "{expected_name}: path {:?} does not end in {dir_name}\\moved\\{expected_name}",
            info.path,
        );
    }

    Ok(())
}
