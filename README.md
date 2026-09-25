# ntfs-reader

[![crates.io](https://img.shields.io/crates/v/ntfs-reader)](https://crates.io/crates/ntfs-reader)
[![docs.rs](https://img.shields.io/docsrs/ntfs-reader)](https://docs.rs/ntfs-reader)
![license: MIT OR Apache-2.0](https://img.shields.io/crates/l/ntfs-reader)

Reads an NTFS volume's `$MFT` into memory, and its USN change journal. Windows only; reading the raw
volume, it sees files Windows keeps locked or has deleted.

## Features

- Fast in-memory scan of every record in the $MFT
- Deleted files: names, sizes, times, paths and, best effort, content
- Read any file's data from the raw volume, even one Windows keeps locked
- Cluster bitmap: see whether a deleted file's clusters were reused
- Usn journal reader

## Examples

Complete programs in `examples`, run from an elevated shell with `cargo run --example <name>`.

| Example           | What it shows                                                                                  | Start here to                          |
| ----------------- | ---------------------------------------------------------------------------------------------- | -------------------------------------- |
| `read_mft`        | Every file with its path and size.                                                             | list a volume                          |
| `list_hardlinks`  | Files with more than one hard link, and all their paths.                                       | work with hard links                   |
| `list_deleted`    | A volume's deleted files: size, time, path, and with `--allocation` the data's state.           | see what was deleted                   |
| `recover_file`    | Copies a deleted file's data to a new file, reporting how much can be trusted.                 | recover a file                         |
| `monitor_journal` | Watches the journal: created, deleted, renamed, moved and modified files, `--from-start`, `--snapshot`. | monitor a volume                       |
| `watch_deletes`   | The light version: only deletes, no `Mft`, best effort paths.                                  | log deletes with a few lines           |

## Guides

[Deleted files](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/deleted-files.md), [the USN journal](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/journal.md),
[paths and caches](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/paths-and-caches.md) and [reading file data](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/reading-data.md), also on
docs.rs in the `ntfs_reader::guide` modules.

## Opening a volume

`Volume::new` takes a Win32 device path such as `\\.\C:` or `\\?\C:`: the volume itself, not a
file on it. The crate passes the string straight to `CreateFileW`, so `\\.\` and `\\?\` are
interchangeable here; the examples below use both only to show that.

Opening a raw volume needs an elevated (administrator) process; without one you get
`NtfsReaderError::AccessDenied`. Use the device path exactly as shown: a trailing backslash
(`\\.\C:\`) or a plain `C:` fails with "Access is denied", which looks like a missing elevation
but is not.

## Listing files

```rust,no_run
# use ntfs_reader::{DefaultPathCache, FileInfo, Mft, Volume};
# fn main() -> Result<(), Box<dyn std::error::Error>> {
let mft = Mft::new(Volume::new(r"\\.\C:")?)?;
let mut cache = DefaultPathCache::new(); // remembers directory paths, faster on a scan

// Every in-use file, except the 24 records NTFS reserves (so not the root directory).
for file in mft.files() {
    let info = FileInfo::with_cache(&file, &mut cache);
    println!("{:?} {} bytes", info.path, info.size); // `path` is None if it cannot be resolved
}
# Ok(())
# }
```

More: [paths and caches](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/paths-and-caches.md), docs.rs `ntfs_reader::guide::paths_and_caches`.

## Reading a stream

```rust,no_run
# use std::io::Read;
# use ntfs_reader::{Mft, Volume};
# fn main() -> Result<(), Box<dyn std::error::Error>> {
let mft = Mft::new(Volume::new(r"\\.\C:")?)?;
for file in mft.files().filter(|file| !file.is_directory()).take(10) {
    let Ok(mut stream) = file.open_stream(None) else { continue };
    let mut head = Vec::new();
    stream.by_ref().take(16).read_to_end(&mut head)?;
    println!("{} bytes, starts with {:02x?}", stream.size(), head);
}
# Ok(())
# }
```

The reader is `Read + Seek` and works on a locked file; `Some(name)` opens an alternate stream.
Compressed and encrypted streams error out. More: [reading data](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/reading-data.md), docs.rs `ntfs_reader::guide::reading_data`.

## Deleted files

```rust,no_run
# use ntfs_reader::{DeletedPathCache, Mft, Volume};
# fn main() -> Result<(), Box<dyn std::error::Error>> {
let mft = Mft::new(Volume::new(r"\\.\C:")?)?;
let mut cache = DeletedPathCache::new();
for file in mft.deleted_files().filter(|file| !file.is_directory()) {
    let Some(name) = file.best_name() else { continue };
    let found = mft.resolve_deleted_path(&name, &mut cache);
    // An incomplete path has a marker such as `<lost 1234>` where a directory is unknown.
    println!("{} (complete path: {})", found.path.display(), found.complete);
}
# Ok(())
# }
```

Deleted data is best effort: NTFS reuses a freed record for a new file (the very next one, on a quiet
volume), and a free cluster may already be trimmed (zeroes, or noise on BitLocker). More: [deleted files](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/deleted-files.md), docs.rs `ntfs_reader::guide::deleted_files`.

## Watching the journal

The journal must be active (`fsutil usn createjournal m=<size> a=<delta> <drive>:` creates one).

```rust,no_run
# use std::time::Duration;
# use ntfs_reader::{Journal, JournalOptions, Reason, Volume};
# fn main() -> Result<(), Box<dyn std::error::Error>> {
let mut journal = Journal::new(Volume::new(r"\\.\C:")?, JournalOptions::default())?; // new events only
loop {
    let page = journal.read()?;
    // NTFS puts every bit of one open handle in the record of its close: act on those.
    for record in page.records.iter().filter(|r| r.reason.contains(Reason::CLOSE)) {
        let reason = record.reason;
        if reason.contains(Reason::FILE_DELETE) {
            // None when the parent is gone too. A file deleted while open (or a directory removed
            // with `remove_dir_all`) resolves to `\\.\C:\$Extend\$Deleted\<random>`: it was renamed
            // there before the delete, and its real name is lost.
            println!("deleted {:?}", journal.resolve_path(record));
        } else if reason.contains(Reason::RENAME_NEW_NAME) {
            println!("renamed {:?} to {:?}", journal.match_rename(record), journal.resolve_path(record));
        } else if reason.contains(Reason::FILE_CREATE) {
            println!("created {:?}", journal.resolve_path(record));
        }
    }
    if page.caught_up {
        std::thread::sleep(Duration::from_millis(250));
    }
}
# }
```

Test `FILE_DELETE` first: a temporary file created and deleted through one handle is one record with
both bits. Modified files, attributes and hard links have their own reasons. More: [the journal](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/journal.md), docs.rs `ntfs_reader::guide::journal`.

## Development

Building, testing and the development shell are described in
[CONTRIBUTING.md](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/CONTRIBUTING.md).
