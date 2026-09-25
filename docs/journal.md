# The USN journal

The USN journal is NTFS's log of changes: one record per file event, with its id, parent id,
name, what changed, and when. `Journal` reads it. This page covers turning records into events,
paths for deleted files, and resuming after a restart. Other guides:
[deleted files](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/deleted-files.md), [paths and caches](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/paths-and-caches.md),
[reading data](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/reading-data.md).

## Reading the journal

Opening the volume needs elevation, like the MFT (see "Opening a volume" in the README). The
journal must already be active: `Journal::new` fails with `NtfsReaderError::JournalNotActive`
otherwise. Most Windows volumes have one by default; on a fresh or test volume, create it with
`fsutil usn createjournal m=<max size> a=<allocation delta> <drive>:`.

```rust,no_run
# use ntfs_reader::{Journal, JournalOptions, Reason, Volume};
# fn main() -> Result<(), Box<dyn std::error::Error>> {
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
# Ok(())
# }
```

Things to know about `read`:

- One `read()` returns one page (4096 bytes; `read_sized` takes more, at least
  `Journal::MIN_READ_BUFFER_SIZE`), not every event since the start. Loop it, stopping or
  sleeping on `caught_up`. Use that, not an empty `records`: `reason_mask` alone can empty a page.
- `JournalOptions::default()` reads every reason from the journal's current end (`NextUsn::Next`),
  keeping 4096 rename records for `match_rename` (`HistorySize::Limited(4096)`). `NextUsn::First`
  starts at the oldest record still held.
- `UsnRecord::name` is the file's own name, without its directory, as a lossless `OsString`.
  `file_id`/`parent_id` match `NtfsFile::file_id`. `file_attributes` are the attributes at the
  change: after `FILE_DELETE` they mark a deleted directory (`FILE_ATTRIBUTE_DIRECTORY`, 0x10)
  versus a deleted file. `timestamp` is UTC.

## From records to events

Two facts shape every monitor:

1. **A record can carry several reasons.** NTFS accumulates the reason bits of one open handle
   into its close record (`Reason::CLOSE`). A temp file created, written and deleted through one
   handle arrives as one `FILE_CREATE | DATA_EXTEND | FILE_DELETE | CLOSE` record. Act on `CLOSE`
   records, testing `FILE_DELETE` first: tested last, it reports as a create. A record without
   `CLOSE` is intermediate; its bits reappear in the close record. `Reason` has a `Display`
   listing the set bits, handy next to an event.
2. **NTFS writes several records per operation.** A rename is an old-name record
   (`RENAME_OLD_NAME`) followed by a new-name one (`RENAME_NEW_NAME`).

Common reasons, and what to call them (all `Reason` constants):

| Reason                                                                      | Event                                     |
| ---------------------------------------------------------------------------- | ----------------------------------------- |
| `FILE_CREATE`                                                               | created                                   |
| `FILE_DELETE`                                                               | deleted (a directory too, see `file_attributes`) |
| `DATA_OVERWRITE`, `DATA_EXTEND`, `DATA_TRUNCATION`                          | modified (default stream)                 |
| `NAMED_DATA_OVERWRITE`, `NAMED_DATA_EXTEND`, `NAMED_DATA_TRUNCATION`        | modified (alternate stream)               |
| `RENAME_NEW_NAME` (with `RENAME_OLD_NAME` before it)                        | renamed, or moved if the parent changed   |
| `BASIC_INFO_CHANGE`, `SECURITY_CHANGE`, `EA_CHANGE`                         | attributes, times, access rights changed  |
| `HARD_LINK_CHANGE`                                                          | hard link added or removed                |
| `STREAM_CHANGE`, `REPARSE_POINT_CHANGE`, `COMPRESSION_CHANGE`, `ENCRYPTION_CHANGE`, `OBJECT_ID_CHANGE`, `INDEXABLE_CHANGE` | other changes |
| `CLOSE` alone                                                               | nothing: file closed                      |

`examples/monitor_journal.rs` classifies records this way, printing the raw reason set with each
line; unnamed bits show as "other change", not dropped.

### Renames and moves

`Journal::match_rename(record)` gives a `RENAME_NEW_NAME` record's old name, from the journal's
history of old-name records. Needs `Reason::RENAME_OLD_NAME` in `reason_mask` (the default has
it); `None` for a record that is not a new-name one, or one whose old-name record aged out of a
bounded history (`HistorySize`, `Journal::trim_history`). The journal does not say whether the
file moved: compare `(file_id, parent_id)` between the old- and new-name records (the example
does), since the old-name record carries the prior directory and the new-name one the current
directory. A moved file's old name is often unchanged.

## Paths of deleted files

`UsnRecord::name` is only the name. `Journal::resolve_path(record)` builds the path from the
record's parent directory as it is now, looked up by id, plus the name:

- Created, modified or renamed: gives the current path. A `RENAME_OLD_NAME` record gives where
  the file used to be, not where it is.
- Deleted: works while the parent directory still exists.
- A file deleted while open, or any directory `remove_dir_all` removes, is renamed by Windows
  into `$Extend\$Deleted` first. Its `FILE_DELETE` record (arriving after the last handle closes)
  carries a random 24-hex-digit name (for example `02BD0000000002033008E7C5`) with `parent_id` on
  the live `$Extend\$Deleted`, so `resolve_path` **succeeds**: `\\.\C:\$Extend\$Deleted\<random>`,
  not the file's real path, which is gone for good. A file inside such a tree keeps its real
  name, with `parent_id` naming the renamed directory. `examples/monitor_journal.rs` reports
  `real name lost` for this case.
- If the parent was deleted too (a whole tree removed, issue #2's "Deletions are not tracked"),
  it returns `None`. It also costs one or two handle opens per call, so call it only for records
  you report.

Two ways to do better, both best effort:

- **Say what you know.** The record keeps the name, whether it was a directory
  (`file_attributes & 0x10`), and the parent's id. `examples/watch_deletes.rs` prints
  `<parent unknown>\name`, no `Mft` needed.
- **Keep a snapshot from before.** Load an `Mft` at startup; when `resolve_path` returns `None`,
  look up the parent by id (`record_by_id`, then `best_name()`, then `resolve_path`) for its path
  as of the snapshot, and join the record's name to it. `examples/monitor_journal.rs --snapshot`
  does this.

Load the snapshot **before** the deletes. One loaded after still finds the file and directory up
to a point: `record_by_id` accepts a freed record, and `resolve_path` of the freed directory
still gives its old path, but returns `None` once a directory above it was also deleted
(`resolve_deleted_path` walks freed ones). `record_by_id` rejects a reused record, which on a
quiet volume was the very next file created, so after the delete the path or record may be gone
where an earlier snapshot has both live. It also misses directories created after loading, and
shows a renamed directory's old path. Loading takes seconds on a big volume; flush first (see
[deleted files](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/deleted-files.md), "Flush the volume first") so the snapshot is not older than the disk.

## A monitor loop

Poll `read()` in a loop and sleep when `caught_up` is true. This one reports the close records,
tests `FILE_DELETE` first, and uses the snapshot for the path of a deleted file:

```rust,no_run
# use std::time::Duration;
# use ntfs_reader::{DefaultPathCache, Journal, JournalOptions, Mft, Reason, Volume};
# fn main() -> Result<(), Box<dyn std::error::Error>> {
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
# }
```

## Resuming after a restart, and errors

`Journal::position()` is a `JournalPosition`: the id of the journal and the USN the next read
starts from. Save both, and open with `NextUsn::Custom(saved)` to carry on where you stopped:

```rust,no_run
# use ntfs_reader::{Journal, JournalOptions, NextUsn, NtfsReaderError, Volume};
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# let volume = r"\\.\C:";
# let saved = None; // the JournalPosition you saved last time, if any
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
# let _ = page;
let _to_save = journal.position(); // save it (a file, a registry value) after each page you handled
# Ok(())
# }
```

The errors of the journal, by cause:

| Error                                    | Meaning                                                                     |
| ----------------------------------------- | ---------------------------------------------------------------------------- |
| `AccessDenied`                           | Process is not elevated.                                                    |
| `JournalNotActive`                       | No journal on the volume (`fsutil usn createjournal`).                      |
| `JournalIdMismatch`                      | From `Journal::new`: saved position belongs to an older journal.            |
| `JournalEntryDeleted`                    | From a read: requested USN left the journal, changes lost. Reopen with `NextUsn::First`. |
| `JournalDeleteInProgress`                | Journal is being deleted.                                                   |
| `ReadBufferTooSmall`                     | `read_sized` given less than `Journal::MIN_READ_BUFFER_SIZE`.               |

The journal is a ring of limited size: a monitor stopped for long enough loses records, and
`JournalEntryDeleted` is how you find out.
