# ntfs-reader

[![crates.io](https://img.shields.io/crates/v/ntfs-reader)](https://crates.io/crates/ntfs-reader)
[![docs.rs](https://img.shields.io/docsrs/ntfs-reader)](https://docs.rs/ntfs-reader)
![license: MIT OR Apache-2.0](https://img.shields.io/crates/l/ntfs-reader)

## Features

- Fast in-memory scan of all records in the $MFT
- Usn journal reader

## Examples

See the `examples` directory for complete working examples.

## Opening a volume

`Volume::new` takes a Win32 device path such as `\\.\C:` or `\\?\C:`, naming the volume
itself rather than a file on it. `\\.\` and `\\?\` are both device-path prefixes recognized by
Win32 (the second also disables the usual `MAX_PATH` and path-parsing rules for regular file
paths, which doesn't matter for a bare volume like `C:`); the crate passes the string straight
to `CreateFileW` without rewriting it, so the two forms are interchangeable here. The examples
below use different ones only to show that.

Opening a raw volume this way needs an elevated (administrator) process; a non-elevated caller
gets `NtfsReaderError::AccessDenied`.

## MFT Usage

```rust,no_run
# use ntfs_reader::{DefaultPathCache, FileInfo, Mft, Volume};
# fn main() -> Result<(), Box<dyn std::error::Error>> {
// Open the C volume and its MFT. Needs elevation; see "Opening a volume" above.
let volume = Volume::new("\\\\.\\C:")?;
let mft = Mft::new(volume)?;

// Remembers directory paths between lookups; see "Path cache" below.
let mut cache = DefaultPathCache::new();

// Iterate all files: every in-use file record, except the 24 records NTFS
// reserves for its own use (so not the root directory either).
for file in mft.files() {
    let _info = FileInfo::with_cache(&file, &mut cache);

    // Available fields: name, path (None if it cannot be resolved),
    // is_directory, size, file_attributes, and timestamps (created,
    // accessed, modified).
}

// Lower level: every attribute of a file, across base and extension records,
// plus typed views over them.
for file in mft.files() {
    // Each hard link, as a full path (DOS 8.3 aliases are skipped).
    for link in file.hard_links() {
        let _path = mft.resolve_path(&link, &mut cache);
    }
    // Default stream and alternate data streams.
    for _stream in file.data_streams() {
        // stream.name is None for the default stream, else the stream name as
        // an OsString (lossless: `path:stream` built from it opens the stream).
    }
    // Also: names, best_name, standard_information (timestamps and attribute
    // flags), resident_data, attributes.
}
# Ok(())
# }
```

## Path cache

The MFT stores only a name and a parent directory for each file, so building
a full path means walking up the parent chain. `FileInfo::with_cache` and
`Mft::resolve_path` take a `PathCache` that remembers the path of every
directory they walk through, so later lookups stop at the first cached
parent.

- `DefaultPathCache`: the cache to use. It holds one entry per directory
  visited, so it is cheap for a few lookups and pays off on a full scan.
- `()`: caches nothing. `FileInfo::new` uses it; fine for a single lookup.

`PathCache` is a trait, so you can plug in your own storage.

Measured on a Windows 11 VM system volume (175k files, 190k MFT records,
186 MiB MFT in memory), with a fresh cache for each run:

| Files resolved | No cache | `DefaultPathCache` | Cache heap |
| -------------- | -------- | ------------------ | ---------- |
| 10             | 23 µs    | 21 µs              | < 0.01 MiB |
| 100            | 379 µs   | 252 µs             | 0.02 MiB   |
| 1,000          | 3.2 ms   | 1.5 ms             | 0.18 MiB   |
| All (175k)     | 483 ms   | 218 ms             | 6.2 MiB    |

Dropping a cache filled by a full scan takes about 4 ms. Numbers depend on
the volume and machine; to measure your own, run from an elevated shell with
`NTFS_READER_TEST_VOLUME` set to the drive letter of the volume to read (the
benchmarks read that volume and fail without it; see CONTRIBUTING.md):

```sh
set NTFS_READER_TEST_VOLUME=T
cargo bench --bench mft_benchmark   # time
cargo bench --bench cache_memory    # heap held by the cache
```

## Journal Usage

Like the MFT, opening the volume needs elevation (see "Opening a volume" above). The USN journal
also has to already be active on the volume: `Journal::new` fails with
`NtfsReaderError::JournalNotActive` if it isn't. Most real-world Windows system volumes have one
running by default; on a fresh or test volume, create it first with
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
        // `None` when the file and its parent are both gone (as here, after a delete).
        let _path: Option<std::path::PathBuf> = journal.resolve_path(record);
    }
}
# Ok(())
# }
```

## Development

Building, testing and the development shell are described in
[CONTRIBUTING.md](https://github.com/kikijiki/ntfs-reader/blob/master/CONTRIBUTING.md).
