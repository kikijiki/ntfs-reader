// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! Lists every file with its path and size. Needs an elevated shell, like every raw volume read.
//!
//! Usage: `read_mft [VOLUME] [--compact | --scan]` (VOLUME defaults to `\\.\C:`: no trailing
//! backslash; `C:` or `C:\` fail with "Access is denied" even elevated).
//!
//! Default: `Mft::new`, the whole `$MFT` in memory. `--compact` uses `Mft::new_compact` instead:
//! same files, each record trimmed to its used size, 30 to 70 percent less memory at some load
//! time. `--scan` uses `MftScan` instead: the same files again, read a few MiB at a time rather
//! than all at once (issue #14), for a volume too big to hold in memory at all. Every mode ends
//! with the file count and `size_in_memory()`, in bytes, of what stayed in memory to produce it.
use std::env;

use ntfs_reader::{DefaultPathCache, FileInfo, Mft, MftScan, NtfsFile, Volume};

mod support;
use support::VOLUME_HINT;

const USAGE: &str = "usage: read_mft [VOLUME] [--compact | --scan]";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Whole,
    Compact,
    Scan,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut volume = String::from("\\\\.\\C:");
    let mut mode = Mode::Whole;
    for arg in env::args().skip(1) {
        match arg.as_str() {
            "--compact" | "--scan" if mode != Mode::Whole => {
                return Err(format!("--compact and --scan are exclusive\n{USAGE}").into())
            }
            "--compact" => mode = Mode::Compact,
            "--scan" => mode = Mode::Scan,
            "--help" | "-h" => {
                println!("{USAGE}\n{VOLUME_HINT}");
                return Ok(());
            }
            _ if arg.starts_with("--") => return Err(format!("{USAGE}\n{VOLUME_HINT}").into()),
            _ => volume = arg,
        }
    }

    let mut cache = DefaultPathCache::new();
    let (count, bytes) = match mode {
        Mode::Whole => {
            let mft = Mft::new(Volume::new(&volume)?)?;
            (print_files(mft.files(), &mut cache), mft.size_in_memory())
        }
        Mode::Compact => {
            let mft = Mft::new_compact(Volume::new(&volume)?)?;
            (print_files(mft.files(), &mut cache), mft.size_in_memory())
        }
        Mode::Scan => {
            let mut scan = MftScan::new(Volume::new(&volume)?)?;
            let mut count = 0;
            while let Some(chunk) = scan.next_chunk()? {
                count += print_files(chunk.files(), &mut cache);
            }
            (count, scan.size_in_memory())
        }
    };
    println!("{count} files, {bytes} bytes in memory");

    Ok(())
}

/// Prints one line per file (path and size) and returns how many there were.
fn print_files<'a>(
    files: impl Iterator<Item = NtfsFile<'a>>,
    cache: &mut DefaultPathCache,
) -> usize {
    let mut count = 0;
    for file in files {
        let info = FileInfo::with_cache(&file, cache);

        // Available fields: name, path, is_directory, size, file_attributes,
        // created, accessed, modified. `path` is None if it cannot be resolved.
        println!(
            "Path: {}, Size: {} bytes, Directory: {}",
            info.path.as_deref().map_or_else(
                || "<unresolved>".to_string(),
                |path| path.display().to_string()
            ),
            info.size,
            info.is_directory
        );
        count += 1;
    }
    count
}
