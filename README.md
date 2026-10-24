# ntfs-reader

[![crates.io](https://img.shields.io/crates/v/ntfs-reader)](https://crates.io/crates/ntfs-reader)
[![docs.rs](https://img.shields.io/docsrs/ntfs-reader)](https://docs.rs/ntfs-reader)
![license: MIT OR Apache-2.0](https://img.shields.io/crates/l/ntfs-reader)

Reads an NTFS volume's `$MFT` and USN change journal. Windows only. It reads the raw volume, so it
can see locked files and deleted files.

## Features

- Fast in-memory scan of every `$MFT` record
- `MftScan`: the same scan without keeping the `$MFT` in memory (tens of MiB instead of about a
  gigabyte on a million-file volume)
- `Mft::new_compact`: a full `Mft` using 30 to 70 percent less memory
- Deleted files: names, sizes, times, paths and, best effort, content
- Read any file's data from the raw volume, even if Windows has it locked
- Cluster bitmap, to check whether a deleted file's clusters were reused
- USN journal reader

## Examples

The programs in `examples` need an elevated shell: `cargo run --example <name>`.

| Example           | What it does                                                                          |
| ----------------- | ------------------------------------------------------------------------------------- |
| `read_mft`        | Lists every file with path and size. `--compact` uses `Mft::new_compact`, `--scan` uses `MftScan`. |
| `list_hardlinks`  | Lists files with more than one hard link, with all their paths.                       |
| `list_deleted`    | Lists deleted files with size, time and path. `--allocation` adds the data's state.   |
| `recover_file`    | Copies a deleted file's data to a new file and reports how much can be trusted.       |
| `monitor_journal` | Watches the journal for created, deleted, renamed, moved and modified files. Has `--from-start` and `--snapshot`. |
| `watch_deletes`   | The short version: only deletes, no `Mft`, best-effort paths.                         |

## Guides

[Deleted files](docs/deleted-files.md), [the USN journal](docs/journal.md),
[paths and caches](docs/paths-and-caches.md), [reading file data](docs/reading-data.md) and
[Volume Shadow Copies](docs/shadow-copies.md). They are also on docs.rs in the
`ntfs_reader::guide` modules.

## Opening a volume

`Volume::new` takes a Win32 device path such as `\\.\C:` or `\\?\C:`. It must be the volume itself,
not a file on it.

- Opening a raw volume needs an elevated (administrator) process. Otherwise you get
  `NtfsReaderError::AccessDenied`.
- Use the path exactly as shown. A trailing backslash (`\\.\C:\`) or a plain `C:` also fails with
  "Access is denied", even when elevated.

`Volume::new` also accepts a file holding a flat NTFS volume image, with the boot sector at byte
zero.

- It doesn't find partitions or parse VHD/VHDX metadata.
- MFT and stream reads use the image, and resolved paths start with the image's path.
- The USN journal still needs a live volume.
- The file-system size in the boot sector must fit within the device (or file) length, otherwise
  you get `NtfsReaderError::InvalidBootSector`, or an I/O error if the length can't be read.
- The reported volume size is the file-system size, without any unused space after it.

## Listing files

```rust,no_run
use ntfs_reader::{DefaultPathCache, FileInfo, Mft, Volume};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mft = Mft::new(Volume::new(r"\\.\C:")?)?;
    let mut cache = DefaultPathCache::new(); // remembers directory paths, faster on a scan

    // Every in-use file, except the 24 records NTFS reserves (so not the root directory).
    for file in mft.files() {
        let info = FileInfo::with_cache(&file, &mut cache);
        println!("{:?} {} bytes", info.path, info.size); // `path` is None if it cannot be resolved
    }
    Ok(())
}
```

More: [paths and caches](docs/paths-and-caches.md).

## Scanning a large volume

`Mft::new` keeps the whole `$MFT` in memory, about 1 KiB per file. `MftScan` visits every file, live
and deleted, by reading the `$MFT` twice in chunks. It needs a few MiB even for a million files.

```rust,no_run
use ntfs_reader::{DefaultPathCache, FileInfo, MftScan, Volume};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut scan = MftScan::new(Volume::new(r"\\.\C:")?)?;
    let mut cache = DefaultPathCache::new();
    while let Some(chunk) = scan.next_chunk()? {
        for file in chunk.files() {
            let info = FileInfo::with_cache(&file, &mut cache);
        }
    }
    Ok(())
}
```

- Paths use the directory index from the first read, so cache eviction doesn't change a path.
- `MftChunk::record` returns an extension record only while its base record is available.
- `files()` and `deleted_files()` include the extensions kept for their files.

More, including how the two reads of a live volume can differ:
[paths and caches](docs/paths-and-caches.md#scanning-a-large-volume).

For random access with less memory, use `Mft::new_compact(volume)`. It keeps only each record's used
bytes plus the header and update sequence array needed to validate it. It sizes the store in a first
pass and grows it if a live volume changes before the second pass.

## Reading a stream

```rust,no_run
use std::io::Read;
use ntfs_reader::{Mft, Volume};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mft = Mft::new(Volume::new(r"\\.\C:")?)?;
    for file in mft.files().filter(|file| !file.is_directory()).take(10) {
        let Ok(mut stream) = file.open_stream(None) else { continue };
        let mut head = Vec::new();
        stream.by_ref().take(16).read_to_end(&mut head)?;
        println!("{} bytes, starts with {:02x?}", stream.size(), head);
    }
    Ok(())
}
```

- The reader is `Read + Seek` and works on locked files.
- `Some(name)` opens an alternate stream.
- Compressed and encrypted streams return an error.

More: [reading data](docs/reading-data.md).

## Deleted files

```rust,no_run
use ntfs_reader::{DeletedPathCache, Mft, Volume};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mft = Mft::new(Volume::new(r"\\.\C:")?)?;
    let mut cache = DeletedPathCache::new();
    for file in mft.deleted_files().filter(|file| !file.is_directory()) {
        let Some(name) = file.best_name() else { continue };
        let found = mft.resolve_deleted_path(&name, &mut cache);
        // Where a directory is unknown, the path has a marker: `\\.\C:\<lost 1234>\dir\file.txt`.
        println!("{}", found.to_marked_path(mft.volume().path()).display());
    }
    Ok(())
}
```

Deleted data is best effort.

- NTFS reuses a freed record for a new file. On a quiet volume that is the very next one.
- A free cluster may already be trimmed. It then reads as zeroes, or as noise on BitLocker.

More: [deleted files](docs/deleted-files.md).

## Watching the journal

The journal must be active. `fsutil usn createjournal m=<size> a=<delta> <drive>:` creates one.

```rust,no_run
use std::time::Duration;
use ntfs_reader::{Journal, JournalOptions, Reason, Volume};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut journal = Journal::new(Volume::new(r"\\.\C:")?, JournalOptions::default())?; // new events only
    loop {
        let page = journal.read()?;
        // The CLOSE record has all the reasons accumulated while the handle was open.
        for record in page.records.iter().filter(|r| r.reason.contains(Reason::CLOSE)) {
            let reason = record.reason;
            if reason.contains(Reason::FILE_DELETE) {
                // None if the parent is gone too. A file deleted while open resolves to
                // `\\.\C:\$Extend\$Deleted\<random>`: its real name is lost.
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
}
```

- Test `FILE_DELETE` first. A temporary file created and deleted through one handle is a single
  record with both bits set.
- Modifications, attribute changes and hard links have their own reasons.

More: [the journal](docs/journal.md).

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for building, testing and the development shell.
