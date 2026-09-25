// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! Watches a volume's USN journal and prints one line per change: time (UTC), event, path and
//! raw reasons. Needs an elevated shell and an active journal.
//!
//! Usage: `monitor_journal [VOLUME] [--seconds N] [--snapshot] [--all] [--from-start]`. VOLUME
//! defaults to `\\.\C:` (no trailing backslash; `C:` or `C:\` fail with "Access is denied" even
//! elevated). Runs until Ctrl-C unless `--seconds` is given, printing `READY` once polling
//! starts. `--from-start` reads from the oldest record (`NextUsn::First`) instead of only new
//! changes.
//!
//! NTFS accumulates one open handle's reason bits into its close record (`CLOSE`): create,
//! write and delete through one handle is one `FILE_CREATE | DATA_EXTEND | FILE_DELETE | CLOSE`
//! record, printed as one line. `--all` prints every record instead, so an operation gaining
//! bits over time appears more than once.
//!
//! A file deleted while open, or removed by `remove_dir_all`, is renamed by Windows into
//! `$Extend\$Deleted` with a random 24-hex-digit name under the live `$Deleted` parent, so
//! `Journal::resolve_path` resolves to `\\.\C:\$Extend\$Deleted\<random>`: the real name is
//! lost, and the line marks it `real name lost` instead of showing the random name as the
//! file's. Control characters in names and paths are escaped as `\u{..}`.
//!
//! `--snapshot` loads an `Mft` first (seconds on a big volume) to name deleted files whose
//! directory was also deleted: `Journal::resolve_path` uses the current parent and returns
//! `None` once it is gone too (issue #2), but the snapshot still knows the parent as it was when
//! taken. It cannot help with a directory created after the snapshot, and shows the old path for
//! one renamed since. Load it before the deletes: NTFS reuses freed records at once, so a later
//! snapshot finds other files there.
#[cfg(windows)]
use std::{collections::HashMap, env, process::ExitCode, time::Duration};

mod support;
#[cfg(windows)]
use support::{
    deadline_after, expired, is_broken_pipe, passes_through_deleted, printable, VOLUME_HINT,
};

#[cfg(windows)]
use ntfs_reader::{
    DefaultPathCache, FileId, Journal, JournalOptions, Mft, NextUsn, Reason, UsnRecord, Volume,
};

/// `FILE_ATTRIBUTE_DIRECTORY`, in `UsnRecord::file_attributes`.
#[cfg(windows)]
const DIRECTORY: u32 = 0x10;

#[cfg(windows)]
struct Monitor {
    snapshot: Option<(Mft, DefaultPathCache)>,
    /// The directory a file was in before its last rename, keyed by file id: only the
    /// old-name/new-name pair says whether it moved.
    old_parents: HashMap<FileId, FileId>,
    /// Print every record, not only the ones of a close.
    all: bool,
}

#[cfg(windows)]
impl Monitor {
    /// The line for one record, or `None` when it says nothing new: not a handle's close (its
    /// bits repeat there) or a bare close. Raw reasons are printed too, since one record can
    /// carry several.
    fn line(&mut self, journal: &Journal, record: &UsnRecord) -> Option<String> {
        let reason = record.reason;
        // The old-name record has the pre-rename directory; the close record repeats the bit
        // with the new name and directory.
        if reason.contains(Reason::RENAME_OLD_NAME) && !reason.contains(Reason::CLOSE) {
            self.old_parents.insert(record.file_id, record.parent_id);
        }
        if !self.all && !reason.contains(Reason::CLOSE) {
            return None;
        }
        let mut events = Vec::new();
        let is_dir = record.file_attributes & DIRECTORY != 0;
        // Reason groups with a name here; anything else is "other change".
        let groups = [
            (Reason::FILE_DELETE, "deleted"),
            (Reason::FILE_CREATE, "created"),
            (
                Reason::DATA_OVERWRITE | Reason::DATA_EXTEND | Reason::DATA_TRUNCATION,
                "modified",
            ),
            (
                Reason::BASIC_INFO_CHANGE | Reason::SECURITY_CHANGE | Reason::EA_CHANGE,
                "attributes changed",
            ),
            (Reason::HARD_LINK_CHANGE, "hard link changed"),
            (Reason::RENAME_OLD_NAME | Reason::RENAME_NEW_NAME, ""),
        ];
        for (reasons, event) in groups {
            if !event.is_empty() && reason.intersects(reasons) {
                events.push(event.to_string());
            }
        }
        // NAMED_DATA_*, STREAM_CHANGE, REPARSE_POINT_CHANGE, COMPRESSION_CHANGE, ENCRYPTION_CHANGE,
        // OBJECT_ID_CHANGE, INDEXABLE_CHANGE and the rest: reported, not dropped.
        let known = groups
            .iter()
            .fold(Reason::CLOSE, |all, (reasons, _)| all | *reasons);
        if reason.bits() & !known.bits() != 0 {
            events.push("other change".to_string());
        }
        if reason.contains(Reason::RENAME_NEW_NAME) {
            // With `--all` the new-name record precedes its close, which still needs the old
            // directory: only the close removes it.
            let old = if reason.contains(Reason::CLOSE) {
                self.old_parents.remove(&record.file_id)
            } else {
                self.old_parents.get(&record.file_id).copied()
            };
            let moved = old.map(|old| old != record.parent_id);
            let mut event = match moved {
                Some(true) => "moved",
                Some(false) => "renamed",
                None => "renamed or moved",
            }
            .to_string();
            if let Some(old) = journal.match_rename(record) {
                event += &format!(" (was {})", printable(&old.to_string_lossy()));
            }
            events.push(event);
        }
        if events.is_empty() {
            return None;
        }
        let kind = if is_dir && reason.contains(Reason::FILE_DELETE) {
            " dir"
        } else {
            ""
        };
        Some(format!(
            "{}  {}{kind}  {}  [{reason}]",
            record.timestamp.time(),
            events.join(", "),
            self.path(journal, record)
        ))
    }

    fn path(&mut self, journal: &Journal, record: &UsnRecord) -> String {
        if let Some(path) = journal.resolve_path(record) {
            let text = printable(&path.display().to_string());
            // `$Extend\$Deleted` is live, so the path resolves, but the name is the random one
            // Windows gave the file: the real name is gone.
            return match (
                passes_through_deleted(&path),
                record.file_attributes & DIRECTORY != 0,
            ) {
                (false, _) => text,
                (true, false) => format!("{text}  (deleted while open, real name lost)"),
                (true, true) => {
                    format!("{text}  (renamed by Windows before the delete, real name lost)")
                }
            };
        }
        // The file and its parent are gone. The snapshot knows the parent by its id.
        if let Some((mft, cache)) = &mut self.snapshot {
            let parent = mft
                .record_by_id(record.parent_id)
                .and_then(|dir| dir.best_name());
            if let Some(parent) = parent.and_then(|name| mft.resolve_path(&name, cache)) {
                return printable(&parent.join(&record.name).display().to_string());
            }
        }
        format!(
            "{} (directory unknown)",
            printable(&record.name.to_string_lossy())
        )
    }
}

#[cfg(windows)]
const USAGE: &str =
    "usage: monitor_journal [VOLUME] [--seconds N] [--snapshot] [--all] [--from-start]";

#[cfg(windows)]
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        // `monitor_journal | head`: the reader left, which is not an error.
        Err(error) if is_broken_pipe(error.as_ref()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Prints one line. A closed pipe returns `Err` (`main` handles it quietly) instead of the panic
/// `println!` would give.
#[cfg(windows)]
fn say(line: &str) -> std::io::Result<()> {
    use std::io::Write;
    writeln!(std::io::stdout().lock(), "{line}")
}

#[cfg(windows)]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut volume = String::from("\\\\.\\C:");
    let mut seconds = None;
    let mut snapshot = false;
    let mut all = false;
    let mut from_start = false;
    let mut args = env::args().skip(1);
    let usage = || format!("{USAGE}\n{VOLUME_HINT}");
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                say(&usage())?;
                return Ok(());
            }
            "--snapshot" => snapshot = true,
            "--all" => all = true,
            "--from-start" => from_start = true,
            "--seconds" => {
                seconds = Some(
                    args.next()
                        .and_then(|n| n.parse::<u64>().ok())
                        .ok_or("--seconds needs a number")?,
                )
            }
            _ if arg.starts_with('-') => return Err(usage().into()),
            _ => volume = arg,
        }
    }

    // To resume after a restart, save `journal.position()` (journal id and USN) and reopen with
    // `JournalOptions { next_usn: NextUsn::Custom(saved), .. }`. `JournalIdMismatch`: the journal
    // was deleted and recreated, start over from `NextUsn::Next`. `JournalEntryDeleted`: the
    // saved USN wrapped out of the journal, reopen with `NextUsn::First` instead.
    let options = JournalOptions {
        next_usn: if from_start {
            NextUsn::First
        } else {
            NextUsn::Next
        },
        ..JournalOptions::default()
    };
    let mut journal = Journal::new(Volume::new(&volume)?, options)?;
    let snapshot = if snapshot {
        say("loading a snapshot of the MFT, this takes seconds on a big volume...")?;
        // A raw read sees the disk, not the file cache: flush the volume first (`sync_all` on a
        // volume opened for writing is `FlushFileBuffers`; nothing is written).
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&volume)?
            .sync_all()?;
        let mft = Mft::new(Volume::new(&volume)?)?;
        Some((mft, DefaultPathCache::new()))
    } else {
        None
    };
    let mut monitor = Monitor {
        snapshot,
        old_parents: HashMap::new(),
        all,
    };
    say(&format!("READY: watching {volume} (times are UTC)"))?;
    // Counted from here: the snapshot above can take a while, and too many seconds to fit an
    // `Instant` means no deadline.
    let deadline = deadline_after(seconds);

    while !expired(deadline) {
        loop {
            let page = journal.read()?;
            for record in &page.records {
                if let Some(line) = monitor.line(&journal, record) {
                    say(&line)?;
                }
            }
            // Also while there is more to read: `--from-start` on a big journal must stop on time.
            if page.caught_up || expired(deadline) {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "monitor_journal: windows-only (Journal needs DeviceIoControl); skipped on this platform"
    );
}
