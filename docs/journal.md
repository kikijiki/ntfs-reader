# The USN journal

The USN journal is NTFS's log of changes: one record per file event, with the file id, parent id,
name, what changed and when. `Journal` reads it.

Other guides: [deleted files](deleted-files.md), [paths and caches](paths-and-caches.md),
[reading data](reading-data.md).

## Reading the journal

- Opening the volume needs elevation, as for the MFT.
- The journal must already be active, otherwise `Journal::new` returns
  `NtfsReaderError::JournalNotActive`. Most Windows volumes have one. To create one:
  `fsutil usn createjournal m=<max size> a=<allocation delta> <drive>:`.

```rust,no_run
use ntfs_reader::{Journal, JournalOptions, Reason, Volume};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let volume = Volume::new("\\\\?\\C:")?;

    // With `JournalOptions` you can customize things like where to start reading from
    // (beginning, end, specific point) and `reason_mask`, which events to get: a bitmask of
    // `Reason` constants (for example `Reason::FILE_CREATE`), OR'd together.
    let options = JournalOptions {
        reason_mask: Reason::FILE_CREATE | Reason::FILE_DELETE,
        ..JournalOptions::default()
    };
    let mut journal = Journal::new(volume, options)?;

    // Try to read some events.
    // You can call `read_sized(buffer_size)` to use a custom buffer size.
    let result = journal.read()?;

    // `result.caught_up` is true once this read reached the journal's current end (even if
    // `result.records` is empty because reason_mask filtered everything in this window).
    for record in &result.records {
        // Available fields are: usn, timestamp, file_id, parent_id, reason, file_attributes,
        // name.
        // `record.reason` is a `Reason` bitmask: `contains` is true when every bit passed to
        // it is set, `intersects` when at least one is.
        if record.reason.contains(Reason::FILE_DELETE) {
            // `record.name` is the file name only, as a lossless OsString (a name that is not
            // valid UTF-16 is not altered). Resolving a full path is a separate, explicit step
            // (it costs one or two handle opens), so it's not done for every record. It returns
            // `None` when the file and its parent are both gone. For
            // the path of a deleted file see "Paths of deleted files" below.
            let _path: Option<std::path::PathBuf> = journal.resolve_path(record);
        }
    }
    Ok(())
}
```

About `read`:

- One `read()` returns one page (4096 bytes), not everything since the start. `read_sized` takes a
  bigger buffer, at least `Journal::MIN_READ_BUFFER_SIZE`.
- Call it in a loop and sleep or stop when `caught_up` is true. Don't test for an empty `records`
  instead, because `reason_mask` alone can empty a page.
- `JournalOptions::default()` reads all reasons from the current end (`NextUsn::Next`) and keeps
  4096 rename records for `match_rename` (`HistorySize::Limited(4096)`). `NextUsn::First` starts at
  the oldest record still held.
- `UsnRecord::name` is the file's own name, without the directory, as a lossless `OsString`.
- `file_id` and `parent_id` match `NtfsFile::file_id`.
- `file_attributes` are the attributes at the time of the change. After `FILE_DELETE` they tell a
  deleted directory (`FILE_ATTRIBUTE_DIRECTORY`, 0x10) from a deleted file.
- `timestamp` is UTC.

## From records to events

A record can carry several reasons. NTFS collects the reason bits of one open handle into its
close record (`Reason::CLOSE`).

- A temp file created, written and deleted through one handle is a single
  `FILE_CREATE | DATA_EXTEND | FILE_DELETE | CLOSE` record.
- Act on `CLOSE` records, and test `FILE_DELETE` first, otherwise that file is reported as a
  create.
- Records without `CLOSE` are intermediate. Their bits show up again in the close record.
- `Reason` implements `Display`, which lists the set bits.

One operation can also write several records. A rename is a `RENAME_OLD_NAME` record followed by a
`RENAME_NEW_NAME` record.

| Reason                                                                       | Event                                            |
| ---------------------------------------------------------------------------- | ------------------------------------------------ |
| `FILE_CREATE`                                                                | created                                          |
| `FILE_DELETE`                                                                | deleted (a directory too, see `file_attributes`) |
| `DATA_OVERWRITE`, `DATA_EXTEND`, `DATA_TRUNCATION`                           | modified (default stream)                        |
| `NAMED_DATA_OVERWRITE`, `NAMED_DATA_EXTEND`, `NAMED_DATA_TRUNCATION`         | modified (alternate stream)                      |
| `RENAME_NEW_NAME` (after `RENAME_OLD_NAME`)                                  | renamed, or moved if the parent changed          |
| `BASIC_INFO_CHANGE`, `SECURITY_CHANGE`, `EA_CHANGE`                          | attributes, times or access rights changed       |
| `HARD_LINK_CHANGE`                                                           | hard link added or removed                       |
| `STREAM_CHANGE`, `REPARSE_POINT_CHANGE`, `COMPRESSION_CHANGE`, `ENCRYPTION_CHANGE`, `OBJECT_ID_CHANGE`, `INDEXABLE_CHANGE` | other changes |
| `CLOSE` alone                                                                | nothing, the file was closed                     |

`examples/monitor_journal.rs` classifies records like this and prints the raw reason set on each
line. Bits without a name show as "other change".

### Renames and moves

- `Journal::match_rename(record)` returns the old name for a `RENAME_NEW_NAME` record, using the
  journal's history of old-name records.
- It needs `Reason::RENAME_OLD_NAME` in `reason_mask` (the default has it).
- It returns `None` if the record isn't a new-name record, or if the old-name record has aged out of
  a bounded history (`HistorySize`, `Journal::trim_history`).
- The journal doesn't say whether the file moved. Compare `(file_id, parent_id)` between the old and
  new records: the old one has the previous directory, the new one the current directory. The
  example does this.

## Paths of deleted files

`UsnRecord::name` is only the name. `Journal::resolve_path(record)` looks up the parent directory by
id as it is now, and adds the name. It costs one or two handle opens per call, so call it only for
records you report.

| Record                 | Result                                                               |
| ---------------------- | -------------------------------------------------------------------- |
| Created, modified, renamed | The current path. `RENAME_OLD_NAME` gives where the file used to be. |
| Deleted                | Works while the parent directory still exists.                       |
| Deleted while open, or removed by `remove_dir_all` | Succeeds, but gives `\\.\C:\$Extend\$Deleted\<random>`. See below. |
| Parent deleted too     | `None`.                                                              |

Windows renames a file deleted while open, and every directory `remove_dir_all` removes, into
`$Extend\$Deleted` before deleting it.

- The `FILE_DELETE` record arrives after the last handle closes. It has a random 24-hex-digit name
  (for example `02BD0000000002033008E7C5`), and `parent_id` is the live `$Extend\$Deleted`.
- So the path resolves, but it isn't the real path. The real name is gone.
- A file inside such a tree keeps its real name, with `parent_id` naming the renamed directory.
- `examples/monitor_journal.rs` reports `real name lost` for this case.

When `resolve_path` returns `None`, there are two best-effort options.

- Report what you know. The record has the name, whether it was a directory
  (`file_attributes & 0x10`) and the parent's id. `examples/watch_deletes.rs` prints
  `<parent unknown>\name` and needs no `Mft`.
- Keep a snapshot from before. Load an `Mft` at startup. When `resolve_path` returns `None`, look
  up the parent with `record_by_id`, then `best_name()`, then `resolve_path`, and join the record's
  name to that path. `examples/monitor_journal.rs --snapshot` does this.

Load the snapshot before the deletes. A snapshot loaded afterwards still works up to a point:

- `record_by_id` accepts a freed record but rejects a reused one. On a quiet volume the very next
  file created reused it.
- `resolve_path` on a freed directory still gives its old path, but returns `None` once a directory
  above it was also deleted. `resolve_deleted_path` can walk freed directories.
- It misses directories created after it was loaded, and shows the old path of a renamed directory.
- Loading takes seconds on a big volume. Flush first (see
  [deleted files](deleted-files.md#flush-the-volume-first)) so the snapshot isn't older than the
  disk.

## A monitor loop

This loop handles only close records, tests `FILE_DELETE` first, and uses the snapshot to resolve
the path of a deleted file:

```rust,no_run
use std::time::Duration;
use ntfs_reader::{DefaultPathCache, Journal, JournalOptions, Mft, Reason, Volume};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let volume = r"\\.\C:";
    let mut journal = Journal::new(Volume::new(volume)?, JournalOptions::default())?; // new events only
    let mft = Mft::new(Volume::new(volume)?)?; // the snapshot, taken before the deletes
    let mut cache = DefaultPathCache::new();
    loop {
        let page = journal.read()?;
        for record in &page.records {
            let reason = record.reason;
            if !reason.contains(Reason::CLOSE) {
                continue; // the record of the close holds every bit of that open
            }
            if reason.contains(Reason::FILE_DELETE) {
                // A file deleted while open, or a directory removed with `remove_dir_all`, resolves to
                // `\\.\C:\$Extend\$Deleted\<random>`: the path resolves, and the real name is lost.
                let path = journal.resolve_path(record).or_else(|| {
                    let parent = mft.record_by_id(record.parent_id)?.best_name()?;
                    Some(mft.resolve_path(&parent, &mut cache)?.join(&record.name))
                });
                println!("deleted {path:?}");
            } else if reason.contains(Reason::RENAME_NEW_NAME) {
                let (old, new) = (journal.match_rename(record), journal.resolve_path(record));
                println!("renamed from {old:?} to {new:?}");
            } else if reason.contains(Reason::FILE_CREATE) {
                println!("created {:?}", journal.resolve_path(record));
            } else if reason.intersects(Reason::DATA_OVERWRITE | Reason::DATA_EXTEND) {
                println!("modified {:?}", journal.resolve_path(record));
            }
        }
        if page.caught_up {
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}
```

## Resuming after a restart

`Journal::position()` returns a `JournalPosition`: the journal's id and the USN the next read starts
from. Save it, and open with `NextUsn::Custom(saved)` to continue where you stopped:

```rust,no_run
use ntfs_reader::{Journal, JournalOptions, NextUsn, NtfsReaderError, Volume};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let volume = r"\\.\C:";
    let saved = None; // the JournalPosition you saved last time, if any
    let options = match saved {
        Some(position) => JournalOptions { next_usn: NextUsn::Custom(position), ..JournalOptions::default() },
        None => JournalOptions::default(),
    };
    let mut journal = match Journal::new(Volume::new(volume)?, options) {
        Ok(journal) => journal,
        // The journal was deleted and recreated: its id and its USNs started over, the position is
        // meaningless. Changes made in between are not recoverable; start again from the end.
        Err(NtfsReaderError::JournalIdMismatch) => {
            Journal::new(Volume::new(volume)?, JournalOptions::default())?
        }
        Err(error) => return Err(error.into()),
    };
    let page = match journal.read() {
        Ok(page) => page,
        // The saved USN is older than the journal's first record: the journal wrapped and the
        // records in between are gone. A read does not move on from it, so reopen the journal at the
        // oldest record it still holds (`journal.first_usn()` says which, as of when it was opened).
        Err(NtfsReaderError::JournalEntryDeleted) => {
            let from_the_start = JournalOptions { next_usn: NextUsn::First, ..JournalOptions::default() };
            journal = Journal::new(Volume::new(volume)?, from_the_start)?;
            journal.read()?
        }
        Err(error) => return Err(error.into()),
    };
    let _ = page;
    let _to_save = journal.position(); // save it (a file, a registry value) after each page you handled
    Ok(())
}
```

## Errors

| Error                     | Meaning                                                                 |
| ------------------------- | ----------------------------------------------------------------------- |
| `AccessDenied`            | The process isn't elevated.                                             |
| `JournalNotActive`        | No journal on the volume (`fsutil usn createjournal`).                  |
| `JournalIdMismatch`       | From `Journal::new`: the saved position belongs to an older journal.    |
| `JournalEntryDeleted`     | From a read: the requested USN left the journal and changes were lost. Reopen with `NextUsn::First`. |
| `JournalDeleteInProgress` | The journal is being deleted.                                           |
| `ReadBufferTooSmall`      | `read_sized` got less than `Journal::MIN_READ_BUFFER_SIZE`.             |

The journal is a ring of limited size. A monitor that stays stopped long enough loses records, and
`JournalEntryDeleted` is how you find out.
