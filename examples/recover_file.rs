// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! Copies the data of a deleted file to a new file and says how much of it can be trusted.
//! Needs an elevated shell, like every raw volume read.
//!
//! Usage: `recover_file VOLUME RECORD_OR_NAME DESTINATION [--stream NAME] [--force]`
//!
//! VOLUME is the device path, `\\.\C:`: no trailing backslash; `C:` or `C:\` fail with "Access is
//! denied" even elevated.
//!
//! `RECORD_OR_NAME` is a record number (see `list_deleted`) or, failing that, part of a name.
//! Directories never match. The destination must not exist and nothing else is written, and must
//! be on another volume: writing to the source can overwrite the clusters you want, so a
//! destination with the source's drive letter is refused (compared as `C:\`, `\\?\C:\` and
//! `\\.\C:`; a mounted folder from the source is not detected). `--force` is required for a
//! stream with over 1 GiB of holes (the copy is written in full).
//!
//! Run it right after the delete: on a virtual disk, freed clusters were measured zeroed within
//! seconds; it depends on the disk (another run there saw none after 30 s), so the copy can come
//! back all zeroes.
//!
//! Exit code 0: the whole stream was copied. 1: nothing was written (an error, a stream that lost
//! its data or is fully reused, a refused destination, or a failed write, which removes the
//! destination). 2: a copy was written but is not fully trustworthy: the read stopped early, part
//! of the stream has no known place, or the copy holds reused clusters from other files. An
//! all-zero copy also warns: a free cluster may already be zeroed, and the reader cannot tell.
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, Prefix};
use std::process::ExitCode;
use std::{env, error::Error};

use ntfs_reader::{ClusterBitmap, FileInfo, Mft, NtfsFile, NtfsReaderError, Volume};

mod support;
use support::{printable, VOLUME_HINT};

const USAGE: &str =
    "usage: recover_file VOLUME RECORD_OR_NAME DESTINATION [--stream NAME] [--force]";
const PROGRESS_STEP: u64 = 16 << 20;
/// The most holes (sparse parts and bytes never written) copied without `--force`.
const HOLE_LIMIT: u64 = 1 << 30;

/// The drive letter a path prefix names, upper case. std parses `\\.\C:` as a device namespace
/// path (device `C:`), not a `Disk` prefix.
fn prefix_letter(prefix: Prefix<'_>) -> Option<char> {
    match prefix {
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
            Some(char::from(letter).to_ascii_uppercase())
        }
        Prefix::DeviceNS(device) => match device.as_encoded_bytes() {
            [letter, b':'] if letter.is_ascii_alphabetic() => {
                Some(char::from(*letter).to_ascii_uppercase())
            }
            _ => None,
        },
        _ => None,
    }
}

/// The drive letter of `path`, upper case, if it has one (`T:\x`, `\\?\T:\x`, `\\.\T:`).
fn drive_letter(path: &Path) -> Option<char> {
    let path = std::path::absolute(path).ok()?;
    match path.components().next()? {
        Component::Prefix(prefix) => prefix_letter(prefix.kind()),
        _ => None,
    }
}

/// Flushes the volume so a raw read sees recent writes or deletes: `sync_all` on a volume opened
/// for writing is `FlushFileBuffers`. Nothing is written.
fn flush_volume(volume: &str) -> std::io::Result<()> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(volume)?
        .sync_all()
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// The one deleted file `what` names: a record number, else part of a name (case-insensitive). A
/// number matching no deleted record is tried as a name too (`2024` may be a year).
fn find<'a>(mft: &'a Mft, what: &str) -> Result<NtfsFile<'a>, Box<dyn Error>> {
    let part = what.to_lowercase();
    let files = || mft.deleted_files().filter(|file| !file.is_directory());
    let mut found: Vec<_> = match what.parse::<u64>() {
        Ok(number) => files().filter(|file| file.number() == number).collect(),
        Err(_) => Vec::new(),
    };
    if found.is_empty() {
        found = files()
            .filter(|file| {
                file.names()
                    .any(|name| name.to_string().to_lowercase().contains(&part))
            })
            .collect();
    }
    match found.len() {
        0 => Err("no deleted file matches".into()),
        1 => Ok(found.remove(0)),
        count => {
            for file in found.iter().take(10) {
                let name = file.best_name().map(|name| name.to_string());
                eprintln!(
                    "  record {}: {}",
                    file.number(),
                    printable(&name.unwrap_or_default())
                );
            }
            Err(format!("{count} deleted files match, give a record number").into())
        }
    }
}

/// What to tell about a stream whose data NTFS wiped when the file was deleted.
fn report_data_lost() {
    eprintln!(
        "WARNING: data lost: NTFS wiped the size and data runs of this stream when it was deleted."
    );
    eprintln!("         Where its bytes were is not known: it is gone.");
}

/// The copy loop. Returns the bytes copied, whether they are all zeroes and what stopped the read.
fn copy(
    stream: &mut impl Read,
    out: &mut impl Write,
    size: u64,
) -> std::io::Result<(u64, bool, Option<std::io::Error>)> {
    let (mut buffer, mut copied, mut reported) = (vec![0u8; 1 << 20], 0u64, 0u64);
    let mut all_zero = true;
    let stopped = loop {
        match stream.read(&mut buffer) {
            Ok(0) => break None,
            Ok(count) => {
                out.write_all(&buffer[..count])?;
                all_zero &= buffer[..count].iter().all(|&byte| byte == 0);
                copied += count as u64;
                if copied - reported >= PROGRESS_STEP {
                    reported = copied;
                    eprintln!("copied {copied} of {size} bytes");
                }
            }
            Err(error) => break Some(error),
        }
    };
    Ok((copied, all_zero, stopped))
}

/// The usage text, with what to know about VOLUME.
fn usage() -> String {
    format!("{USAGE}\n{VOLUME_HINT}")
}

fn run() -> Result<ExitCode, Box<dyn Error>> {
    let (mut positional, mut stream_name, mut force) = (Vec::new(), None, false);
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--stream" => stream_name = Some(args.next().ok_or_else(usage)?),
            "--force" => force = true,
            "--help" | "-h" => {
                println!("{}", usage());
                return Ok(ExitCode::SUCCESS);
            }
            _ if arg.starts_with("--") => return Err(usage().into()),
            _ => positional.push(arg),
        }
    }
    let [volume, what, destination] = positional.as_slice() else {
        return Err(usage().into());
    };
    if drive_letter(Path::new(destination))
        .is_some_and(|letter| drive_letter(Path::new(volume)) == Some(letter))
    {
        return Err("the destination is on the volume being recovered: writing there can take the clusters you want".into());
    }

    flush_volume(volume)?;
    let mft = Mft::new(Volume::new(volume)?)?;
    let file = find(&mft, what)?;
    let info = FileInfo::new(&file);
    println!("record {}: {}", file.number(), printable(&info.name));

    let mut stream = match file.open_stream(stream_name.as_deref().map(OsStr::new)) {
        Ok(stream) => stream,
        // The record holding the default stream was reused: no stream to open, though the file
        // did have data once. Other errors pass through unchanged.
        Err(
            NtfsReaderError::StreamNotFound { .. }
            | NtfsReaderError::InvalidDataRun {
                details: "missing VCN-0 extent",
            },
        ) if stream_name.is_none() && info.data_lost => {
            report_data_lost();
            eprintln!("Nothing to recover, nothing written.");
            return Ok(ExitCode::FAILURE);
        }
        Err(error) => return Err(error.into()),
    };
    let allocation = stream.allocation(&ClusterBitmap::new(&mft)?)?;
    let size = stream.size();
    println!(
        "{size} bytes: {} in free clusters, {} in allocated clusters, {} resident, {} sparse, {} missing, {} outside the bitmap, {} beyond the initialized size",
        allocation.in_free_clusters(),
        allocation.in_allocated_clusters(),
        allocation.resident(),
        allocation.sparse(),
        allocation.missing(),
        allocation.outside_bitmap(),
        allocation.beyond_initialized(),
    );
    if stream.data_lost() {
        report_data_lost();
    }
    if allocation.in_allocated_clusters() > 0 {
        eprintln!("WARNING: some clusters were reused: the copy holds other files' bytes there.");
    }
    if allocation.missing() > 0 {
        eprintln!("WARNING: some extents are missing: the copy stops at the first one.");
    }
    if allocation.outside_bitmap() > 0 {
        eprintln!("WARNING: some bytes are in clusters the bitmap does not cover: the volume is not the one the bitmap is of, or the extent is past its end.");
    }
    if allocation.in_free_clusters() > 0 {
        eprintln!("Note: a free cluster may already be zeroed (TRIM). Check the copy.");
        eprintln!("      It is not proof of untouched bytes either: another file may have used");
        eprintln!("      the clusters and been deleted since, and its bytes are in the copy.");
    }
    let unusable =
        allocation.in_allocated_clusters() + allocation.missing() + allocation.outside_bitmap();
    if stream.data_lost() || (size > 0 && unusable >= size) {
        eprintln!("Nothing to recover, nothing written.");
        return Ok(ExitCode::FAILURE);
    }
    let holes = allocation.sparse() + allocation.beyond_initialized();
    if holes > HOLE_LIMIT && !force {
        return Err(format!(
            "{holes} bytes of the copy are holes that would be written as zeroes: use --force"
        )
        .into());
    }

    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| format!("{destination}: {error}"))?;
    let copied = copy(&mut stream, &mut out, size);
    drop(out);
    // A failed write (e.g. disk full) leaves a file of unknown content: remove it, so exit code 1
    // still means nothing was written.
    let (copied, all_zero, stopped) = match copied {
        Ok(result) => result,
        Err(error) => {
            let _ = fs::remove_file(destination);
            return Err(format!("{destination}: {error}, the copy was removed").into());
        }
    };
    println!("copied {copied} of {size} bytes to {destination}");
    if let Some(error) = &stopped {
        eprintln!("WARNING: stopped after {copied} bytes: {error}");
    }
    if copied == 0 && size > 0 {
        fs::remove_file(destination)?;
        return Ok(ExitCode::FAILURE);
    }
    if all_zero && copied > 0 && holes < copied {
        eprintln!("WARNING: the copy is all zeroes: the clusters were probably zeroed by TRIM.");
    }
    // A partial copy, or one holding other files' bytes or unplaced bytes, is neither a clean
    // success nor a failed write.
    if stopped.is_some()
        || copied < size
        || allocation.missing() > 0
        || allocation.in_allocated_clusters() > 0
        || allocation.outside_bitmap() > 0
    {
        return Ok(ExitCode::from(2));
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::io::Cursor;

    #[test]
    fn every_form_of_a_volume_path_names_its_drive_letter() {
        let letter = |prefix| prefix_letter(prefix);
        assert_eq!(letter(Prefix::Disk(b'c')), Some('C'));
        assert_eq!(letter(Prefix::VerbatimDisk(b'C')), Some('C'));
        // What std makes of `\\.\C:` (the form every example takes for a volume).
        assert_eq!(letter(Prefix::DeviceNS(OsStr::new("C:"))), Some('C'));
        assert_eq!(letter(Prefix::DeviceNS(OsStr::new("d:"))), Some('D'));
        // Not a drive letter.
        assert_eq!(letter(Prefix::DeviceNS(OsStr::new("PhysicalDrive0"))), None);
        assert_eq!(letter(Prefix::DeviceNS(OsStr::new("C"))), None);
        assert_eq!(letter(Prefix::DeviceNS(OsStr::new("1:"))), None);
        assert_eq!(letter(Prefix::DeviceNS(OsStr::new("C:x"))), None);
        assert_eq!(
            letter(Prefix::UNC(OsStr::new("server"), OsStr::new("share"))),
            None
        );
        assert_eq!(letter(Prefix::Verbatim(OsStr::new("x"))), None);
    }

    // On Windows std parses the real strings; the three forms of one volume agree.
    #[cfg(windows)]
    #[test]
    fn the_three_windows_forms_of_a_volume_give_the_same_letter() {
        for path in [
            r"\\.\C:",
            r"\\?\C:",
            r"C:\",
            r"c:\dir\file",
            r"\\.\C:\",
            r"\\?\c:\x",
        ] {
            assert_eq!(drive_letter(Path::new(path)), Some('C'), "{path}");
        }
        for path in [r"\\.\PhysicalDrive0", r"\\server\share\x"] {
            assert_eq!(drive_letter(Path::new(path)), None, "{path}");
        }
    }

    /// A writer that accepts `room` bytes and then reports a full disk.
    struct Full {
        room: usize,
        written: usize,
    }

    impl Write for Full {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            if self.written + data.len() > self.room {
                return Err(std::io::Error::other("disk full"));
            }
            self.written += data.len();
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_full_destination_is_an_error_and_not_a_short_copy() {
        let mut source = Cursor::new(vec![7u8; 4096]);
        let mut out = Full {
            room: 100,
            written: 0,
        };
        assert!(copy(&mut source, &mut out, 4096).is_err());

        let mut source = Cursor::new(vec![7u8; 4096]);
        let mut out = Full {
            room: 4096,
            written: 0,
        };
        let (copied, all_zero, stopped) = copy(&mut source, &mut out, 4096).unwrap();
        assert_eq!((copied, all_zero, stopped.is_none()), (4096, false, true));
    }
}
