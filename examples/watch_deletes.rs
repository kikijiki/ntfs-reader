// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! Prints one line for every file or directory deleted on a volume, as it happens. Needs an
//! elevated shell and an active journal. Usage: `watch_deletes [VOLUME]` (default `\\.\C:`, the
//! device path: no trailing backslash; `C:` or `C:\` fail with "Access is denied" even elevated).
//!
//! Prints the close record of the handle that deleted the file: NTFS accumulates one open's
//! reason bits there, so even a file created, written and deleted through one handle is one
//! line.
//!
//! Best effort, no MFT: a journal record has the deleted file's name, its parent's id and
//! attributes, but not its path. `Journal::resolve_path` builds the path from the current
//! parent, so it works while that parent exists; once it too is gone (a whole tree removed) the
//! line falls back to `<parent unknown>\name`, whether it is a directory, and the parent's id:
//! the parent's name and path are gone with it. `monitor_journal --snapshot` recovers more.
//!
//! A file deleted while open, or removed by `remove_dir_all`, is first renamed by Windows into
//! `$Extend\$Deleted`. Its record then has a random 24-hex-digit name under the live `$Deleted`
//! parent, so the path resolves to `\\.\C:\$Extend\$Deleted\<random>`; the line marks it
//! `real name lost`. Control characters in names and paths are escaped as `\u{..}`.
#[cfg(windows)]
use std::{env, process::ExitCode, thread::sleep, time::Duration};

#[cfg(windows)]
use ntfs_reader::{Journal, JournalOptions, Reason, Volume};

mod support;
#[cfg(windows)]
use support::{is_broken_pipe, passes_through_deleted, printable, VOLUME_HINT};

#[cfg(windows)]
const USAGE: &str = "usage: watch_deletes [VOLUME]";

#[cfg(windows)]
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        // `watch_deletes | head`: the reader left, which is not an error.
        Err(error) if is_broken_pipe(error.as_ref()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Prints one line. A closed pipe is an `Err`, which `main` ends quietly on; `println!` would panic.
#[cfg(windows)]
fn say(line: &str) -> std::io::Result<()> {
    use std::io::Write;
    writeln!(std::io::stdout().lock(), "{line}")
}

#[cfg(windows)]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut volume = String::from("\\\\.\\C:");
    for arg in env::args().skip(1) {
        match arg.as_str() {
            "--help" | "-h" => {
                say(&format!("{USAGE}\n{VOLUME_HINT}"))?;
                return Ok(());
            }
            _ if arg.starts_with('-') => return Err(format!("{USAGE}\n{VOLUME_HINT}").into()),
            _ => volume = arg,
        }
    }
    // Only FILE_DELETE records, and only the ones from now on (the default `NextUsn::Next`).
    let options = JournalOptions {
        reason_mask: Reason::FILE_DELETE,
        ..JournalOptions::default()
    };
    let mut journal = Journal::new(Volume::new(&volume)?, options)?;
    say(&format!(
        "READY: watching deletes on {volume} (times are UTC)"
    ))?;

    loop {
        let page = journal.read()?;
        // The delete bit stays set from delete to close, which carries it too: print only the
        // close, so a deletion is one line.
        for record in page
            .records
            .iter()
            .filter(|r| r.reason.contains(Reason::CLOSE))
        {
            let time = record.timestamp.time();
            match journal.resolve_path(record) {
                Some(path) => {
                    let lost = if passes_through_deleted(&path) {
                        "  (real name lost: deleted while open or renamed by Windows first)"
                    } else {
                        ""
                    };
                    say(&format!(
                        "{time}  {}{lost}",
                        printable(&path.display().to_string())
                    ))?
                }
                None => say(&format!(
                    "{time}  <parent unknown>\\{}{}  (parent id {:?})",
                    printable(&record.name.to_string_lossy()),
                    if record.file_attributes & 0x10 != 0 {
                        " (directory)"
                    } else {
                        ""
                    },
                    record.parent_id
                ))?,
            }
        }
        if page.caught_up {
            sleep(Duration::from_millis(250));
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "watch_deletes: windows-only (Journal needs DeviceIoControl); skipped on this platform"
    );
}
