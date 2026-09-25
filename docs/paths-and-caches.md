# Paths and path caches

The MFT stores only a name and parent per file; a full path is built by walking up the parent
chain. This page covers the live and deleted walks, the caches that speed a scan, incomplete-path
markers, and the limits. Other guides: [deleted files](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/deleted-files.md),
[the journal](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/journal.md), [reading data](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/reading-data.md).

## Live paths

`Mft::resolve_path(&name, &mut cache)` gives the full path of one name of a file (`name` is one
of `NtfsFile::hard_links`: several hard links give several paths). The path starts with the
volume's path, e.g. `\\.\C:\Users\me\file.txt`; each component comes from
`NtfsFileName::to_os_string`, so a name invalid in UTF-16 still gives a path that opens the file.
It follows each parent's `best_name`.

It returns `None` when the chain cannot resolve: a parent is missing, not in use, unnamed, or
stale (record number freed and reused: the full reference, sequence number included, is
compared), or the chain loops; or when the path would exceed Win32's 32767 UTF-16 unit limit,
counting the volume path and separators (a name outside the Basic Multilingual Plane costs two
units per character). The result never depends on the cache: warm or cold, same answer.

`FileInfo::path` is the same, for a whole file, from its best name. A live file never resolves
through a freed or reused directory: `resolve_path` refuses a parent that is not in use.

```rust,no_run
# use ntfs_reader::{DefaultPathCache, Mft, Volume};
# fn main() -> Result<(), Box<dyn std::error::Error>> {
let mft = Mft::new(Volume::new(r"\\.\C:")?)?;
let mut cache = DefaultPathCache::new();
for file in mft.files().take(100) {
    for link in file.hard_links() {
        match mft.resolve_path(&link, &mut cache) {
            Some(path) => println!("{}", path.display()),
            None => println!("{link} (unresolved, parent record {})", link.parent_number()),
        }
    }
}
# Ok(())
# }
```

`FileInfo` summarises a file (`name`, `path`, `is_directory`, `is_deleted`, `data_lost`, `size`,
`file_attributes`, and the times `created`, `accessed`, `modified`). `NtfsFile` goes deeper,
giving every attribute across a file's base and extension records, with typed views:

```rust,no_run
# use ntfs_reader::{DefaultPathCache, FileInfo, Mft, Volume};
# fn main() -> Result<(), Box<dyn std::error::Error>> {
let mft = Mft::new(Volume::new("\\\\.\\C:")?)?;
let mut cache = DefaultPathCache::new();
for file in mft.files() {
    let _info = FileInfo::with_cache(&file, &mut cache);

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

The root directory is record `ROOT_RECORD` (5); ordinary files start at `FIRST_NORMAL_RECORD`
(24). `NtfsFileName::is_in_root` checks only the parent's record number.

## The path cache

`FileInfo::with_cache` and `Mft::resolve_path` take a `PathCache` that remembers each directory's
path as it is walked, so later lookups stop at the first cached parent.

- `DefaultPathCache`: the cache to use, one entry per directory visited. Cheap for a few lookups,
  pays off on a full scan.
- `()`: caches nothing, used by `FileInfo::new`; fine for a single lookup.

`PathCache` is a trait: plug in your own storage if you want.

One `Mft` can be shared by threads (`Mft`, `NtfsFile`, `StreamReader`, `ClusterBitmap` and the
path caches are all `Send + Sync`). A path cache needs `&mut`, so give each thread its own.

Measured on 0.5.0, on a Windows 11 VM system volume (175k files, 190k MFT records, 186 MiB MFT in
memory), fresh cache per run (not repeated for 0.5.2; the deleted-file walk is not in this table):

| Files resolved | No cache | `DefaultPathCache` | Cache heap |
| -------------- | -------- | ------------------ | ---------- |
| 10             | 23 us    | 21 us              | < 0.01 MiB |
| 100            | 379 us   | 252 us             | 0.02 MiB   |
| 1,000          | 3.2 ms   | 1.5 ms             | 0.18 MiB   |
| All (175k)     | 483 ms   | 218 ms             | 6.2 MiB    |

Dropping a full-scan cache takes about 4 ms. Numbers depend on the volume and machine; to measure
your own, run elevated with `NTFS_READER_TEST_VOLUME` set to the drive letter to read (the
benchmarks fail without it; see CONTRIBUTING.md):

```sh
set NTFS_READER_TEST_VOLUME=T
cargo bench --bench mft_benchmark   # time
cargo bench --bench cache_memory    # heap held by the cache
```

## Paths of deleted files

`Mft::resolve_path` is strict on purpose: a live file never resolves through a freed directory. A
deleted file needs a walk through freed directories that marks where it had to guess:
`Mft::resolve_deleted_path(&name, &mut DeletedPathCache)` gives a `DeletedPath`: a `path` and
`complete`, whether every directory was identified.

A parent reference is followed when it names a live directory, like `resolve_path`, or a freed
one: not in use, not allocated, sequence number one above the reference's, which is what deleting
a directory does to its record. A parent must have the directory flag; its names are whatever the
record still holds.

When the walk cannot continue, it does not return `None`: it marks where the directory would be
and sets `complete` to `false`. The marker is a component no ordinary Win32 name can be
(`<`/`>` are forbidden there; a POSIX namespace name allows them, so this is only a convention),
keeping whatever resolved below it, e.g. `\\.\C:\<lost 1234>\dir\file.txt`:

| Marker         | Why the walk stopped                                                                                                    |
| -------------- | ----------------------------------------------------------------------------------------------------------------------- |
| `<lost N>`     | Record N is missing, unnamed, reused, or another incarnation. A loop of directories collapses to one `<lost N>`, N its lowest record. Marks a directory record only; unrelated to `data_lost` or `AllocationState::Lost`. |
| `<deleted>`    | `$Extend\$Deleted`: a directory `remove_dir_all` renamed before deleting, or a file deleted while open. What is below it keeps the random name NTFS gave it. |
| `<too long>`   | Path would exceed 32767 UTF-16 units, markers included; followed by the name alone.                                     |

A marker means an incomplete path. Deleting with `remove_file`/`remove_dir` one entry at a time
keeps the path complete regardless of how many directories were deleted; `remove_dir_all` ends it
at `<deleted>`, since directories were renamed first and the real names are gone for good.

NTFS keeps no rename history: a directory shows whatever name its record holds, at deletion or,
if still live, now. A path reflects where the file was when its directories were last renamed,
not necessarily where it was when deleted.

`DeletedPathCache` is a distinct type, not a `PathCache`: a deleted walk crosses freed
directories and ends at markers, which a live lookup must never see. It remembers every directory
passed, loops and too-long paths included, so one cache for a scan of many files walks each
directory once, however the tree is shaped; a fresh cache per call walks the whole chain every
time. Never reuse one across two `Mft`s; the result does not depend on the cache.

`FileInfo` uses the same walk: for a deleted file, `path` is `Some` only when complete.
`FileInfo::new` and `with_cache` give it a cache of its own, dropped right after, so a scan
should use `with_caches` with one of each cache.

```rust,no_run
# use ntfs_reader::{DefaultPathCache, DeletedPathCache, FileInfo, Mft, Volume};
# fn main() -> Result<(), Box<dyn std::error::Error>> {
let mft = Mft::new(Volume::new(r"\\.\C:")?)?;
let (mut live, mut deleted) = (DefaultPathCache::new(), DeletedPathCache::new());
for file in mft.files().chain(mft.deleted_files()) {
    let info = FileInfo::with_caches(&file, &mut live, &mut deleted);
    println!("{} {:?}", if info.is_deleted { "deleted" } else { "live   " }, info.path);
}

// The path up to where it stops, for a name of a deleted file:
for file in mft.deleted_files() {
    for name in file.hard_links() {
        let found = mft.resolve_deleted_path(&name, &mut deleted);
        println!("{} (complete: {})", found.path.display(), found.complete);
    }
}
# Ok(())
# }
```
