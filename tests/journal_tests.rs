#![cfg(target_os = "windows")]

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use ntfs_reader::{
    Journal, JournalOptions, Mft, NextUsn, NtfsReaderError, NtfsReaderResult, Reason, UsnRecord,
    Volume,
};

mod common;
use common::{drain, test_volume_letter, TempDirGuard};

/// A `Volume` good enough for tests that only exercise `Journal::new`'s path
/// handling and never reach real volume I/O (that happens through
/// `aligned_reader`, not through any field read here).
#[cfg(feature = "internals")]
fn fake_volume(path: impl Into<PathBuf>) -> Volume {
    Volume::synthetic(path, 4096, 0, 1024, 0)
}

// The manual `Debug` names the volume and where the journal stands, and skips the handle.
#[test]
fn a_journal_debug_names_the_volume_and_the_position() -> NtfsReaderResult<()> {
    let volume_path = format!("\\\\?\\{}:", test_volume_letter());
    let options = JournalOptions {
        reason_mask: Reason::FILE_CREATE,
        ..JournalOptions::default()
    };
    let journal = Journal::new(Volume::new(&volume_path)?, options)?;

    let debug = format!("{journal:?}");

    assert!(debug.starts_with("Journal {"), "{debug}");
    for expected in [
        format!("volume: {:?}", PathBuf::from(&volume_path)),
        format!("journal_id: {}", journal.journal_id()),
        format!("next_usn: {}", journal.next_usn()),
        format!("reason_mask: {:?}", Reason::FILE_CREATE),
        "history_len: 0".to_string(),
        "..".to_string(),
    ] {
        assert!(debug.contains(&expected), "{expected:?} is not in {debug}");
    }
    Ok(())
}

#[test]
fn file_create() -> NtfsReaderResult<()> {
    common::init_tracing();

    let options = JournalOptions {
        reason_mask: Reason::FILE_CREATE,
        ..JournalOptions::default()
    };
    let volume = Volume::new(format!("\\\\?\\{}:", test_volume_letter()))?;
    let mut journal = Journal::new(volume, options)?;
    drain(&mut journal)?;

    let mut files = Vec::new();
    let mut found = Vec::new();

    let dir = PathBuf::from(format!(
        "\\\\?\\{}:\\{}",
        test_volume_letter(),
        "usn-journal-test-create"
    ));
    let _cleanup = TempDirGuard::new(&dir)?;

    for x in 0..10 {
        let path = dir.join(format!("usn-journal-test-create-{}.txt", x));
        File::create(&path)?.write_all(b"test")?;
        files.push(path);
    }

    for _ in 0..10 {
        for result in journal.read()?.records {
            found.extend(journal.resolve_path(&result));
        }

        if files.iter().all(|f| found.contains(f)) {
            return Ok(());
        }
    }

    panic!("The file creation was not detected");
}

#[test]
fn file_move() -> NtfsReaderResult<()> {
    common::init_tracing();

    let dir = PathBuf::from(format!(
        "\\\\?\\{}:\\{}",
        test_volume_letter(),
        "usn-journal-test-move"
    ));
    let _cleanup = TempDirGuard::new(&dir)?;

    let path_old = dir.join("usn-journal-test-move.old");
    let path_new = path_old.with_extension("new");

    File::create(&path_old)?.write_all(b"test")?;

    let options = JournalOptions {
        reason_mask: Reason::RENAME_OLD_NAME | Reason::RENAME_NEW_NAME,
        ..JournalOptions::default()
    };
    let volume = Volume::new(format!("\\\\?\\{}:", test_volume_letter()))?;
    let mut journal = Journal::new(volume, options)?;
    drain(&mut journal)?;

    std::fs::rename(&path_old, &path_new)?;

    let old_name = path_old.file_name().unwrap().to_os_string();

    for _ in 0..10 {
        for result in journal.read()?.records {
            let path = journal.resolve_path(&result);
            if path.as_deref() == Some(path_new.as_path())
                && result.reason.contains(Reason::RENAME_NEW_NAME)
            {
                if let Some(name) = journal.match_rename(&result) {
                    assert_eq!(name, old_name);
                    return Ok(());
                } else {
                    panic!("No old name found for {path:?}");
                }
            }
        }
    }

    panic!("The file move was not detected");
}

#[test]
fn file_delete() -> NtfsReaderResult<()> {
    common::init_tracing();

    let dir = PathBuf::from(format!(
        "\\\\?\\{}:\\{}",
        test_volume_letter(),
        "usn-journal-test-delete"
    ));
    let _cleanup = TempDirGuard::new(&dir)?;
    let file_path = dir.join("usn-journal-test-delete.txt");
    File::create(&file_path)?.write_all(b"test")?;

    let options = JournalOptions {
        reason_mask: Reason::FILE_DELETE,
        ..JournalOptions::default()
    };
    let volume = Volume::new(format!("\\\\?\\{}:", test_volume_letter()))?;
    let mut journal = Journal::new(volume, options)?;
    drain(&mut journal)?;

    std::fs::remove_file(&file_path)?;

    for _ in 0..10 {
        for result in journal.read()?.records {
            if journal.resolve_path(&result).as_deref() == Some(file_path.as_path()) {
                return Ok(());
            }
        }
    }

    panic!("The file deletion was not detected");
}

// --- Card 002: journal handles leak on error, and the path goes through the ANSI API ---

// Needs `Volume::synthetic`, which only the `internals` feature builds.
#[cfg(feature = "internals")]
#[test]
fn journal_new_returns_an_error_for_a_non_utf8_path_instead_of_panicking() {
    use std::os::windows::ffi::OsStringExt;

    // A no-panic regression guard, not a test of which error comes back. Fixed (card 002):
    // Journal::new now encodes the path with OsStrExt::encode_wide(), which round-trips any
    // OsString Windows can produce (including ill-formed UTF-16), instead of to_str().unwrap()
    // (which used to panic here). The path isn't a real device, so CreateFileW is expected to
    // fail; the assertion is only that it fails as an `Err`. A panic fails the test itself.
    let wide: Vec<u16> = "\\\\?\\T:"
        .encode_utf16()
        .chain(std::iter::once(0xD800u16))
        .collect();
    let path = PathBuf::from(std::ffi::OsString::from_wide(&wide));
    let volume = fake_volume(path);

    let result = Journal::new(volume, JournalOptions::default());

    assert!(
        result.is_err(),
        "expected Journal::new to return an error for a non-UTF-8 path"
    );
}

/// Restores the USN journal on `volume_arg` (for example `"T:"`) when dropped, so a
/// test that deactivates the journal to force `Journal::new` to fail after `CreateFileW`
/// leaves the volume in the same state it found it, even if an assertion above fails.
struct JournalRestoreGuard {
    volume_arg: String,
}

impl Drop for JournalRestoreGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("fsutil")
            .args(["usn", "createjournal", "m=1000", "a=100", &self.volume_arg])
            .status();
    }
}

// Deletes the volume's USN journal (`fsutil usn deletejournal`), which races every other journal
// test in the default parallel run and breaks indexers and backup tools on a machine that cares
// about its journal. Run with `--ignored`, alone (`--test-threads=1`), on a scratch volume.
#[test]
#[ignore = "deletes and recreates the volume's USN journal; run with --ignored on a scratch volume"]
fn journal_new_does_not_leak_the_volume_handle_when_it_fails_after_create_file(
) -> NtfsReaderResult<()> {
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

    let volume_arg = format!("{}:", test_volume_letter());

    // Deactivate the journal so CreateFileW succeeds but the following
    // FSCTL_QUERY_USN_JOURNAL fails, exercising the early-return path in
    // Journal::new that today drops the volume handle without closing it.
    let status = std::process::Command::new("fsutil")
        .args(["usn", "deletejournal", "/D", &volume_arg])
        .status()?;
    assert!(
        status.success(),
        "fsutil usn deletejournal failed, cannot deactivate the journal for this test"
    );
    let _restore = JournalRestoreGuard {
        volume_arg: volume_arg.clone(),
    };

    let mut before = 0u32;
    unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut before) }
        .expect("GetProcessHandleCount");

    const ATTEMPTS: u32 = 20;
    for _ in 0..ATTEMPTS {
        let volume = Volume::new(format!("\\\\?\\{}", volume_arg))?;
        let result = Journal::new(volume, JournalOptions::default());
        assert!(
            result.is_err(),
            "expected Journal::new to fail while the journal is inactive"
        );
    }

    let mut after = 0u32;
    unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut after) }
        .expect("GetProcessHandleCount");

    let grew_by = after.saturating_sub(before);
    assert!(
        grew_by < ATTEMPTS,
        "process handle count grew by {grew_by} after {ATTEMPTS} failed Journal::new calls \
         (the volume handle leaks when FSCTL_QUERY_USN_JOURNAL fails after CreateFileW succeeds)"
    );

    Ok(())
}

// --- Card 004: match_rename returns the oldest name, not the previous one ---

#[test]
fn file_double_rename() -> NtfsReaderResult<()> {
    common::init_tracing();

    let dir = PathBuf::from(format!(
        "\\\\?\\{}:\\{}",
        test_volume_letter(),
        "usn-journal-test-double-rename"
    ));
    let _cleanup = TempDirGuard::new(&dir)?;

    let path_a = dir.join("usn-journal-test-double-rename.a");
    let path_b = path_a.with_extension("b");
    let path_c = path_a.with_extension("c");

    File::create(&path_a)?.write_all(b"test")?;

    let options = JournalOptions {
        reason_mask: Reason::RENAME_OLD_NAME | Reason::RENAME_NEW_NAME,
        ..JournalOptions::default()
    };
    let volume = Volume::new(format!("\\\\?\\{}:", test_volume_letter()))?;
    let mut journal = Journal::new(volume, options)?;
    drain(&mut journal)?;

    std::fs::rename(&path_a, &path_b)?;
    std::fs::rename(&path_b, &path_c)?;

    let name_b = path_b.file_name().unwrap().to_os_string();

    for _ in 0..10 {
        for result in journal.read()?.records {
            let path = journal.resolve_path(&result);
            if path.as_deref() == Some(path_c.as_path())
                && result.reason.contains(Reason::RENAME_NEW_NAME)
            {
                if let Some(name) = journal.match_rename(&result) {
                    assert_eq!(
                        name, name_b,
                        "match_rename returned the oldest name (A) instead of the \
                         immediately previous name (B) for a double rename"
                    );
                    return Ok(());
                } else {
                    panic!("No old name found for {path:?}");
                }
            }
        }
    }

    panic!("The second rename was not detected");
}

// --- Card 005: USN journal API redesign ---

// Deletes and recreates the volume's USN journal, see the note on the handle-leak test above.
#[test]
#[ignore = "deletes and recreates the volume's USN journal; run with --ignored on a scratch volume"]
fn journal_new_rejects_a_stale_journal_id() -> NtfsReaderResult<()> {
    let volume_arg = format!("{}:", test_volume_letter());

    let volume = Volume::new(format!("\\\\?\\{}", volume_arg))?;
    let journal = Journal::new(volume, JournalOptions::default())?;
    let stale_position = journal.position();
    drop(journal);

    // Recreate the journal so its UsnJournalID changes, making the saved position above stale.
    let status = std::process::Command::new("fsutil")
        .args(["usn", "deletejournal", "/D", &volume_arg])
        .status()?;
    assert!(status.success(), "fsutil usn deletejournal failed");
    // The guard's own `createjournal` on drop is a harmless no-op after the explicit one below.
    let _restore = JournalRestoreGuard {
        volume_arg: volume_arg.clone(),
    };
    let status = std::process::Command::new("fsutil")
        .args(["usn", "createjournal", "m=1000", "a=100", &volume_arg])
        .status()?;
    assert!(status.success(), "fsutil usn createjournal failed");

    let volume = Volume::new(format!("\\\\?\\{}", volume_arg))?;
    let options = JournalOptions {
        next_usn: NextUsn::Custom(stale_position),
        ..JournalOptions::default()
    };
    let result = Journal::new(volume, options);

    assert!(
        matches!(result, Err(NtfsReaderError::JournalIdMismatch)),
        "expected JournalIdMismatch when reopening with a stale journal id, got {:?}",
        result.err()
    );

    Ok(())
}

/// The names of the records `journal` returns until every one of `names` has been seen (or the
/// attempts run out), whatever the number of records NTFS wrote per file.
fn read_until_seen(
    journal: &mut Journal,
    names: &std::collections::HashSet<std::ffi::OsString>,
) -> NtfsReaderResult<Vec<UsnRecord>> {
    let mut records = Vec::new();
    for _ in 0..20 {
        records.extend(journal.read()?.records);
        let seen: std::collections::HashSet<_> = records.iter().map(|r| &r.name).collect();
        if names.iter().all(|name| seen.contains(name)) {
            break;
        }
    }
    Ok(records)
}

// Needs the lookup counter, which the crate only builds under `internals`.
#[cfg(feature = "internals")]
#[test]
fn read_does_not_open_file_handles_to_resolve_paths() -> NtfsReaderResult<()> {
    // `read` must not resolve paths: doing so costs handle opens per record. The crate counts
    // the path lookups each thread makes (the `internals` feature these tests build with), so
    // this asserts on the count instead of timing. NTFS writes several records per file, so it
    // asserts nothing about how many records a file produces.
    use ntfs_reader::internals::path_lookups_on_this_thread;

    let dir = PathBuf::from(format!(
        "\\\\?\\{}:\\{}",
        test_volume_letter(),
        "usn-journal-test-no-eager-paths"
    ));
    let _cleanup = TempDirGuard::new(&dir)?;

    let options = JournalOptions {
        reason_mask: Reason::FILE_CREATE,
        ..JournalOptions::default()
    };
    let volume = Volume::new(format!("\\\\?\\{}:", test_volume_letter()))?;
    let mut journal = Journal::new(volume, options)?;
    drain(&mut journal)?;

    let names: std::collections::HashSet<std::ffi::OsString> = (0..30)
        .map(|i| format!("usn-journal-test-no-eager-paths-{i}.txt").into())
        .collect();
    for name in &names {
        File::create(dir.join(name))?.write_all(b"test")?;
    }

    let before = path_lookups_on_this_thread();
    let records = read_until_seen(&mut journal, &names)?;
    let during_read = path_lookups_on_this_thread() - before;

    let seen: std::collections::HashSet<_> = records.iter().map(|r| r.name.clone()).collect();
    assert!(
        names.iter().all(|name| seen.contains(name)),
        "did not observe the creation of every file; missing {:?}",
        names.difference(&seen).collect::<Vec<_>>()
    );
    assert_eq!(
        during_read, 0,
        "read() looked up {during_read} paths through file handles; it must not resolve paths \
         unless resolve_path is called"
    );

    // The counter does move when paths are resolved, so a zero above means something.
    let created = records
        .iter()
        .find(|r| names.contains(&r.name))
        .expect("a record for one of the files");
    let resolved = journal.resolve_path(created);
    assert!(
        path_lookups_on_this_thread() > before,
        "resolve_path did not register a lookup, the counter is not counting"
    );
    assert_eq!(
        resolved.as_deref(),
        Some(dir.join(&created.name).as_path()),
        "resolve_path did not return the file's path"
    );

    Ok(())
}

// A path that cannot be resolved is `None`, never a bare name: a caller acting on a relative
// path would touch the current directory. Deleting a whole tree leaves records whose file and
// parent are both gone. `resolve_path` goes through the parent's current path first, so the
// tree's top directory, whose parent (the volume root) still exists, resolves to its absolute
// path even though it is deleted; everything below it has no parent left and is `None`.
// The records also say what was deleted: a directory's carries `FILE_ATTRIBUTE_DIRECTORY`.
#[test]
fn a_deleted_tree_resolves_to_an_absolute_path_or_none() -> NtfsReaderResult<()> {
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

    let volume_path = format!("\\\\?\\{}:", test_volume_letter());
    let dir = PathBuf::from(format!("{volume_path}\\usn-journal-test-delete-tree"));
    let _cleanup = TempDirGuard::new(&dir)?;
    let sub = dir.join("sub");
    std::fs::create_dir(&sub)?;
    File::create(sub.join("f.txt"))?.write_all(b"test")?;

    let options = JournalOptions {
        reason_mask: Reason::FILE_DELETE,
        ..JournalOptions::default()
    };
    let mut journal = Journal::new(Volume::new(&volume_path)?, options)?;
    drain(&mut journal)?;

    // Delete one by one: `remove_dir_all` renames each directory to a random name before deleting
    // it, so the journal's FILE_DELETE records would not carry "sub" or the tree's own name.
    std::fs::remove_file(sub.join("f.txt"))?;
    std::fs::remove_dir(&sub)?;
    std::fs::remove_dir(&dir)?;

    let names: std::collections::HashSet<std::ffi::OsString> =
        ["f.txt", "sub", "usn-journal-test-delete-tree"]
            .into_iter()
            .map(Into::into)
            .collect();
    let records = read_until_seen(&mut journal, &names)?;

    let of_the_tree: Vec<_> = records
        .iter()
        .filter(|record| names.contains(&record.name))
        .collect();
    let seen: std::collections::HashSet<_> = of_the_tree.iter().map(|r| r.name.clone()).collect();
    assert_eq!(
        seen, names,
        "a FILE_DELETE record for each name of the tree"
    );

    for record in of_the_tree {
        assert!(record.reason.contains(Reason::FILE_DELETE));
        // What went away, once it is gone: the record's attributes say a directory or a file.
        assert_eq!(
            record.file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
            record.name != "f.txt",
            "the attributes of the deleted {:?}: {:#x}",
            record.name,
            record.file_attributes
        );
        let expected = (record.name == "usn-journal-test-delete-tree").then_some(dir.as_path());
        assert_eq!(
            journal.resolve_path(record).as_deref(),
            expected,
            "the path of the deleted {:?}",
            record.name
        );
    }

    Ok(())
}

// The same file must have the same `FileId` whether it comes from the MFT or from a journal
// record (a V2 record carries the 64-bit reference, a V3 record the 128-bit form).
#[test]
fn a_file_has_the_same_id_in_the_mft_and_in_the_journal() -> NtfsReaderResult<()> {
    let volume_path = format!("\\\\?\\{}:", test_volume_letter());
    let dir = PathBuf::from(format!("{volume_path}\\usn-journal-test-file-id"));
    let _cleanup = TempDirGuard::new(&dir)?;

    let options = JournalOptions {
        reason_mask: Reason::FILE_CREATE,
        ..JournalOptions::default()
    };
    let mut journal = Journal::new(Volume::new(&volume_path)?, options)?;
    drain(&mut journal)?;

    let name = "usn-journal-test-file-id.txt";
    let path = dir.join(name);
    let mut created = File::create(&path)?;
    created.write_all(b"test")?;
    drop(created);

    let names = [std::ffi::OsString::from(name)].into_iter().collect();
    let record = read_until_seen(&mut journal, &names)?
        .into_iter()
        .find(|r| r.name == name)
        .expect("a create record for the file");

    // Flush the volume so the MFT record is on disk when the raw read below looks for it:
    // `sync_all` only flushes the file's own data and attributes, not the `$MFT` record around it.
    common::flush_volume(&test_volume_letter());
    let mft = Mft::new(Volume::new(&volume_path)?)?;
    let file = mft
        .files()
        .find(|file| {
            file.names()
                .any(|file_name| file_name.to_os_string() == name)
        })
        .unwrap_or_else(|| panic!("the MFT has no file named {name}"));

    assert_eq!(
        file.file_id(),
        record.file_id,
        "the MFT and the journal disagree on the id of {name}"
    );

    Ok(())
}

/// The records of `journal` up to its current end whose name starts with `prefix`, in the order
/// they came, and how many reads returned records. The prefix keeps out whatever else writes to
/// the volume.
fn read_to_end(
    journal: &mut Journal,
    buffer_size: usize,
    prefix: &str,
) -> NtfsReaderResult<(Vec<UsnRecord>, usize)> {
    let mut records = Vec::new();
    let mut pages = 0;
    loop {
        let page = journal.read_sized(buffer_size)?;
        pages += !page.records.is_empty() as usize;
        records.extend(
            page.records
                .into_iter()
                .filter(|record| record.name.to_string_lossy().starts_with(prefix)),
        );
        if page.caught_up {
            return Ok((records, pages));
        }
    }
}

// The persistence feature: a position saved from one `Journal` and passed to another as
// `NextUsn::Custom` resumes exactly there. The files made before the save must not come back, the
// ones made after it must all come, and `NextUsn::First` starts before the save.
#[test]
fn a_saved_position_resumes_where_the_journal_left_off() -> NtfsReaderResult<()> {
    const PREFIX: &str = "usn-journal-test-resume-";

    let volume_path = format!("\\\\?\\{}:", test_volume_letter());
    let dir = PathBuf::from(format!("{volume_path}\\usn-journal-test-resume"));
    let _cleanup = TempDirGuard::new(&dir)?;
    let create = |kind: &str| -> NtfsReaderResult<std::collections::HashSet<std::ffi::OsString>> {
        (0..5)
            .map(|i| -> NtfsReaderResult<std::ffi::OsString> {
                let name = format!("{PREFIX}{kind}-{i}.txt");
                File::create(dir.join(&name))?.write_all(b"test")?;
                Ok(name.into())
            })
            .collect()
    };
    let creations = JournalOptions {
        reason_mask: Reason::FILE_CREATE,
        ..JournalOptions::default()
    };

    let mut journal = Journal::new(Volume::new(&volume_path)?, creations.clone())?;
    drain(&mut journal)?;
    let before = create("before")?;
    drain(&mut journal)?;
    let saved = journal.position();
    let after = create("after")?;
    drop(journal);

    let options = JournalOptions {
        next_usn: NextUsn::Custom(saved),
        ..creations
    };
    let mut resumed = Journal::new(Volume::new(&volume_path)?, options)?;
    assert_eq!(resumed.position(), saved);
    let records = read_until_seen(&mut resumed, &after)?;

    let ours: Vec<_> = records
        .iter()
        .filter(|record| record.name.to_string_lossy().starts_with(PREFIX))
        .collect();
    let names: std::collections::HashSet<_> = ours.iter().map(|r| r.name.clone()).collect();
    assert!(
        names.is_disjoint(&before),
        "a file made before the save came back: {:?}",
        names.intersection(&before).collect::<Vec<_>>()
    );
    assert_eq!(
        names, after,
        "the resumed journal must return the files made after the save, and only those"
    );
    for record in ours {
        assert!(record.reason.contains(Reason::FILE_CREATE));
        assert!(
            record.usn >= saved.usn,
            "{:?} has USN {}, before the saved position {}",
            record.name,
            record.usn,
            saved.usn
        );
    }

    // From the first record there is history from before the save.
    let options = JournalOptions {
        next_usn: NextUsn::First,
        ..JournalOptions::default()
    };
    let mut first = Journal::new(Volume::new(&volume_path)?, options)?;
    let oldest = loop {
        let page = first.read()?;
        if let Some(record) = page.records.first() {
            break Some(record.usn);
        }
        if page.caught_up {
            break None;
        }
    };
    assert!(
        oldest.is_some_and(|usn| usn < saved.usn),
        "NextUsn::First returned {oldest:?} as its oldest record, not one before {}",
        saved.usn
    );

    Ok(())
}

// Reading with the smallest buffer takes many pages: no record may be lost or repeated at a page
// boundary. The pages must add up to what one large read returns, in increasing USN order, and
// every file must show its creation and its deletion.
#[test]
fn the_smallest_read_buffer_pages_without_losing_or_repeating_records() -> NtfsReaderResult<()> {
    const PREFIX: &str = "usn-journal-test-paging-";
    const FILES: usize = 40;

    let volume_path = format!("\\\\?\\{}:", test_volume_letter());
    let dir = PathBuf::from(format!("{volume_path}\\usn-journal-test-paging"));
    let _cleanup = TempDirGuard::new(&dir)?;

    let mut journal = Journal::new(Volume::new(&volume_path)?, JournalOptions::default())?;
    drain(&mut journal)?;
    let start = journal.position();

    let names: Vec<String> = (0..FILES).map(|i| format!("{PREFIX}{i:03}.txt")).collect();
    for name in &names {
        File::create(dir.join(name))?.write_all(b"test")?;
    }
    for name in &names {
        std::fs::remove_file(dir.join(name))?;
    }

    let (paged, pages) = read_to_end(&mut journal, Journal::MIN_READ_BUFFER_SIZE, PREFIX)?;
    assert!(
        pages > 3,
        "{FILES} files should fill several {}-byte pages, got {pages}",
        Journal::MIN_READ_BUFFER_SIZE
    );

    let options = JournalOptions {
        next_usn: NextUsn::Custom(start),
        ..JournalOptions::default()
    };
    let mut reference = Journal::new(Volume::new(&volume_path)?, options)?;
    let (whole, _) = read_to_end(&mut reference, 1 << 20, PREFIX)?;
    let key = |records: &[UsnRecord]| -> Vec<_> {
        records
            .iter()
            .map(|r| (r.usn, r.name.clone(), r.reason))
            .collect()
    };
    assert_eq!(key(&paged), key(&whole), "the pages differ from one read");

    assert!(
        paged.windows(2).all(|pair| pair[0].usn < pair[1].usn),
        "the USNs do not increase strictly: a record repeated or out of order"
    );
    for name in &names {
        for reason in [Reason::FILE_CREATE, Reason::FILE_DELETE] {
            assert!(
                paged
                    .iter()
                    .any(|r| r.name == name.as_str() && r.reason.contains(reason)),
                "no {reason:?} record for {name}"
            );
        }
    }

    Ok(())
}
