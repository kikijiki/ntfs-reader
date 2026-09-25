// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! Lists the deleted files of a volume: one line per file with its record number, name, size,
//! modified time and path. Needs an elevated shell, like every raw volume read.
//!
//! Usage: `list_deleted [VOLUME] [--allocation] [--limit N]` (VOLUME defaults to `\\.\C:`: no
//! trailing backslash; `C:` or `C:\` fail with "Access is denied" even elevated).
//!
//! A path starting with `?` is incomplete: a directory on the way could not be identified, and
//! its place holds a marker like `<lost 1234>` (names record 1234) or `<deleted>`. A file
//! deleted while open, or inside a `remove_dir_all` tree, is renamed by Windows into
//! `$Extend\$Deleted` with a random name; the line adds `(renamed by Windows on delete)` when
//! that marker is its own parent. A file inside such a tree keeps its real name; only its
//! directories were renamed. `(data lost)` after a size means NTFS wiped the default stream on
//! delete, or the record was reused: the size shown is not real. Directories print `<dir>`.
//! `--allocation` adds what the cluster bitmap says about the file's data. Control characters in
//! names and paths are escaped as `\u{..}`.
use std::env;
use std::fs::OpenOptions;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::process::ExitCode;

use ntfs_reader::{
    AllocationState, ClusterBitmap, DeletedPathCache, FileInfo, Mft, NtfsFile, Volume,
};

mod support;
use support::{is_broken_pipe, printable, VOLUME_HINT};

const USAGE: &str = "usage: list_deleted [VOLUME] [--allocation] [--limit N]";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        // `list_deleted | head`: the reader left, which is not an error.
        Err(error) if is_broken_pipe(error.as_ref()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Whether the file's own name is in `$Extend\$Deleted`: the marker is the leaf's parent. A path
/// that only passes through the marker (a file inside a renamed `remove_dir_all` tree) says
/// nothing about the file's own name.
fn renamed_on_delete(path: &Path, complete: bool) -> bool {
    !complete
        && path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|parent| parent == "<deleted>")
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut volume = String::from("\\\\.\\C:");
    let mut allocation = false;
    let mut limit = usize::MAX;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--allocation" => allocation = true,
            "--limit" => {
                limit = args
                    .next()
                    .and_then(|n| n.parse().ok())
                    .ok_or_else(|| format!("{USAGE}\n{VOLUME_HINT}"))?
            }
            "--help" | "-h" => {
                println!("{USAGE}\n{VOLUME_HINT}");
                return Ok(());
            }
            _ if arg.starts_with("--") => return Err(format!("{USAGE}\n{VOLUME_HINT}").into()),
            _ => volume = arg,
        }
    }

    // A raw read sees the disk, not the file cache: flush first so a recent delete is listed.
    // `sync_all` on a volume opened for writing is `FlushFileBuffers`; nothing is written.
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&volume)?
        .sync_all()?;
    let mft = Mft::new(Volume::new(&volume)?)?;
    // The bitmap costs one bit per cluster of the volume: read only on request.
    let bitmap = if allocation {
        Some(ClusterBitmap::new(&mft)?)
    } else {
        None
    };
    let mut cache = DeletedPathCache::new();
    // Buffered and locked once: plain `println!` would lock and flush per line.
    let mut out = BufWriter::new(io::stdout().lock());

    for file in mft.deleted_files().take(limit) {
        // `info` and the path walk share `cache`, so each directory is walked only once.
        let info = FileInfo::with_caches(&file, &mut (), &mut cache);
        let size = if info.is_directory {
            "<dir>".to_string()
        } else {
            format!("{} bytes", info.size)
        };
        let lost = if info.data_lost { " (data lost)" } else { "" };
        let modified = info.modified.map(|time| time.to_string());

        let (path, renamed) = match file.best_name() {
            Some(name) => {
                let found = mft.resolve_deleted_path(&name, &mut cache);
                let mark = if found.complete { "" } else { "? " };
                (
                    format!("{mark}{}", found.path.display()),
                    renamed_on_delete(&found.path, found.complete),
                )
            }
            None => (String::from("? <no name>"), false),
        };

        write!(
            out,
            "{:>8}  {}  {size}{lost}  {}",
            file.number(),
            printable(&info.name),
            modified.unwrap_or_default()
        )?;
        write!(out, "  {}", printable(&path))?;
        if renamed {
            write!(out, "  (renamed by Windows on delete)")?;
        }
        if let Some(bitmap) = &bitmap {
            write!(out, "  [{}]", allocation_text(&file, bitmap))?;
        }
        writeln!(out)?;
    }
    out.flush()?;
    Ok(())
}

/// What the bitmap says about the default stream. Allocation is not recoverability: a free
/// cluster can already be zeroed by TRIM.
fn allocation_text(file: &NtfsFile, bitmap: &ClusterBitmap) -> String {
    if file.is_directory() {
        return "directory".into();
    }
    let stream = match file.open_stream(None) {
        Ok(stream) => stream,
        // No default stream at all: its record was freed and reused, or the file has none.
        Err(_) if file.stream_data_lost() => return "lost".into(),
        Err(error) => return format!("error: {error}"),
    };
    let allocation = match stream.allocation(bitmap) {
        Ok(allocation) => allocation,
        Err(error) => return format!("error: {error}"),
    };
    match allocation.state() {
        AllocationState::Free => "free",
        AllocationState::PartlyAllocated => "partly allocated",
        AllocationState::Allocated => "allocated",
        // A stream that lost its runs opens as an empty one and says so; a resident stream of
        // the same file is intact and reports its own state.
        AllocationState::Lost => "lost",
        AllocationState::NoStoredData => "no stored data",
        AllocationState::Incomplete => "incomplete",
        _ => "unknown",
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_name_directly_under_the_deleted_marker_is_renamed_by_windows() {
        let under = |parts: &[&str]| {
            parts
                .iter()
                .fold(Path::new(r"\\.\C:").to_path_buf(), |p, c| p.join(c))
        };
        // A file deleted while open, or a directory of a `remove_dir_all` tree: its own name.
        assert!(renamed_on_delete(&under(&["<deleted>", "02BD0000"]), false));
        // A file inside that tree keeps its real name: the marker is its grandparent.
        assert!(!renamed_on_delete(
            &under(&["<deleted>", "02BD0000", "one.txt"]),
            false
        ));
        assert!(!renamed_on_delete(
            &under(&["<deleted>", "a", "b", "c.txt"]),
            false
        ));
        // A complete path never has the marker, whatever a directory is called.
        assert!(!renamed_on_delete(&under(&["<deleted>", "x"]), true));
        assert!(!renamed_on_delete(&under(&["<lost 5>", "x"]), false));
        assert!(!renamed_on_delete(&under(&["x"]), false));
    }
}
