# Paths and path caches

The MFT stores only a name and parent per file; a full path is built by walking up the parent
chain. This page covers the live and deleted walks, the caches that speed a scan, incomplete-path
markers, and the limits. Other guides: [deleted files](deleted-files.md),
[the journal](journal.md), [reading data](reading-data.md).

## Live paths

`Mft::resolve_path(&name, &mut cache)` gives the full path of one name of a file (`name` is one
of `NtfsFile::hard_links`: several hard links give several paths). The path starts with the
volume's path, e.g. `\\.\C:\Users\me\file.txt`; each component comes from
`NtfsFileName::to_os_string`, so a name invalid in UTF-16 still gives a path that opens the file.
It follows each parent's `best_name`.

It returns `None` when the chain cannot resolve: a parent is missing, not in use, unnamed, not a
directory, or stale (record number freed and reused: the full reference, sequence number
included, is compared), or the chain loops; or when the path would exceed Win32's 32767 UTF-16
unit limit, counting the volume path and separators (a character above U+FFFF costs two units).
The result never depends on the cache: warm or cold, same answer.

`FileInfo::path` is the same, for a whole file, from its best name. A live file never resolves
through a freed or reused directory, or through a record that is not a directory: `resolve_path`
refuses such a parent like any other unresolvable one. An extension record is refused too,
whatever its own directory flag says (extension records don't answer that meaningfully; ask the
base record), since a parent reference never names one on an uncorrupted volume.

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
  pays off on a full scan. `new()` is unbounded, as before; `with_max_bytes(n)` bounds it and
  evicts the least recently used directory (by `get`, `insert` or `insert_failed`) once its cost
  would exceed `n`. The cost tracks real memory, not just entry count or path length: each live
  path's real capacity plus the index's and slab's own real capacity growth (see
  `DefaultPathCache`'s doc comment), so real heap held tracks `n` instead of a multiple of it. A
  bound never changes what `resolve_path` returns, only how much of the walk a later lookup
  redoes: bounded, unbounded and no cache all agree. `bytes()`/`len()` report what is currently
  held.
- `()`: caches nothing, used by `FileInfo::new`; fine for a single lookup.

`PathCache` is a trait: plug in your own storage if you want (`get` takes `&mut self`, since a hit
counts as a use for a bounded implementation's eviction order).

`DeletedPathCache` (below) has no size limit: its entries link to each other by reference (a
directory's path is its parent's plus its own name), so evicting one that another cached entry
still points at would leave that pointer dangling.

One `Mft` can be shared by threads (`Mft`, `NtfsFile`, `StreamReader`, `ClusterBitmap`,
`MftChunk` and the path caches are all `Send + Sync`). A path cache needs `&mut`, so give each
thread its own. `MftScan` itself is `Send` (move a scan to a background thread) but not `Sync`
(`next_chunk` needs `&mut self`, so nothing shares one by reference across threads at once).

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
`Mft::resolve_deleted_path(&name, &mut DeletedPathCache)` gives a `DeletedPath`: a `marker`,
`None` when every directory was identified, and a `path`.

A parent reference is followed when it names a live directory, like `resolve_path`, or a freed
one: not in use, not allocated, sequence number one above the reference's, which is what deleting
a directory does to its record. A parent must be a base record with the directory flag; an
extension record is refused whatever its own flag says (same rule as `resolve_path`'s). Its names
are whatever the record still holds.

When the walk cannot continue, it does not return `None`: it stops at a `DeletedPathMarker` and
keeps whatever resolved below it. A complete `path` starts with the volume path, like
`resolve_path`'s: `\\.\C:\dir\file.txt`. An incomplete one is relative, the names below the
marker: `dir\file.txt` under `Lost(1234)`. `path` never holds marker text, so a recovery tool can
recreate it under a folder of its own and name the marker's folder as it likes (`lost 1234`,
`renamed on delete`). For display, `found.to_marked_path(mft.volume().path())` puts the
marker's `Display` text in as a component: `\\.\C:\<lost 1234>\dir\file.txt`. That text holds `<`
and `>`, which a Win32 name cannot (a POSIX namespace name can, so this is only a convention), so
it is never a path to create a file at.

| Marker        | Displays as  | Why the walk stopped                                                                                        |
| ---------- | ------------ | ----------------------------------------------------------------------------------------------------------- |
| `Lost(N)`  | `<lost N>`   | Record N is missing, unnamed, not a directory, reused, or another incarnation. A loop of directories collapses to one `Lost(N)`, N its lowest record. Marks a directory record only; unrelated to `data_lost` or `AllocationState::Lost`. |
| `Deleted`  | `<deleted>`  | `$Extend\$Deleted`: a directory `remove_dir_all` renamed before deleting, or a file deleted while open. What is below it keeps the random name NTFS gave it. |
| `TooLong`  | `<too long>` | Path would exceed 32767 UTF-16 units, marker text included; `path` is the name alone.                       |

A marker means an incomplete path. Deleting with `remove_file`/`remove_dir` one entry at a time
keeps the path complete regardless of how many directories were deleted; `remove_dir_all` ends it
at `Deleted`, since directories were renamed first and the real names are gone for good.

NTFS keeps no rename history: a directory shows whatever name its record holds, at deletion or,
if still live, now. A path reflects where the file was when its directories were last renamed,
not necessarily where it was when deleted.

`DeletedPathCache` is a distinct type, not a `PathCache`: a deleted walk crosses freed
directories and ends at markers, which a live lookup must never see. It remembers every directory
passed, loops and too-long paths included, so one cache for a scan of many files walks each
directory once, however the tree is shaped; a fresh cache per call walks the whole chain every
time. Reusing one, or a `DefaultPathCache`, across two `Mft`s (a rescan that keeps a cache, a live
`Mft` then an `MftScan`) starts it empty for the new `Mft` instead of returning a path from the
old one: safe, but a warm cache is wasted; give each `Mft` its own if you want to keep both warm.

`FileInfo` uses the same walk: for a deleted file, `path` is `Some` only when complete.
`FileInfo::new` and `with_cache` give it a cache of its own, dropped right after, so a scan
should use `with_caches` with one of each cache.

```rust,no_run
# use ntfs_reader::{DefaultPathCache, DeletedPathCache, DeletedPathMarker, FileInfo, Mft, Volume};
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
        match found.marker {
            None => println!("{}", found.path.display()),
            // A name of your own for the marker, and the real names below it.
            Some(DeletedPathMarker::Lost(record)) => {
                println!("lost {record}\\{}", found.path.display())
            }
            Some(DeletedPathMarker::Deleted) => {
                println!("renamed on delete\\{}", found.path.display())
            }
            Some(_) => println!("{}", found.to_marked_path(mft.volume().path()).display()),
        }
    }
}
# Ok(())
# }
```

## Scanning a large volume

`Mft::new` holds the whole `$MFT` in memory: about 1 KiB per file, so a volume with a million
files costs over 1 GiB. `MftScan` visits every file, live and deleted, without holding it all: it
reads `$MFT` twice instead, once to index directories and extension records (a few MiB even on a
million-file volume, see `MftScan::size_in_memory`), once to hand out each file as it goes.

```rust,no_run
# use ntfs_reader::{DefaultPathCache, FileInfo, MftScan, Volume};
# fn main() -> Result<(), Box<dyn std::error::Error>> {
let mut scan = MftScan::new(Volume::new(r"\\.\C:")?)?;
let mut cache = DefaultPathCache::new();
while let Some(chunk) = scan.next_chunk()? {
    for file in chunk.files() {
        let info = FileInfo::with_cache(&file, &mut cache);
    }
}
# Ok(())
# }
```

`MftChunk::files`, `deleted_files`, `record`, `resolve_path` and `resolve_deleted_path` work like
their `Mft` counterparts; `FileInfo::with_cache`/`with_caches` take a file from a chunk exactly as
they take one from a whole `Mft`, since they read through `NtfsFile::mft()` either way. Share one
`PathCache`/`DeletedPathCache` across the whole scan, not one per chunk: a scan is one reference
space, like an `Mft`.

`record(number)` can retrieve a base record in the current chunk or a retained record. An extension
record is returned only when its base is also available (usually in the current chunk); otherwise
it returns `None`. This keeps a successfully returned record's logical-file accessors complete.
The `files()` and `deleted_files()` iterators still include all retained extensions for each base
they yield.

`MftScan::new` reads 4 MiB chunks; `MftScan::with_chunk_records` picks another size.

The two reads are not one snapshot: on a live volume they can disagree. A directory or extension
record the first read indexed is seen as it read it, in every chunk, even though the second read
passes over it again; anything else is as the second read found it. So a directory renamed
between the two reads keeps its old name in paths, a directory created after the first read is
not seen by path resolution (files under it get no path), and a record reused as a plain file after the first read
still reads as what that read indexed. This is documented on `MftScan` itself; a shadow copy of
the volume, read twice, removes the difference by giving both reads the same snapshot.

The directory index is authoritative even for misses: a directory rejected in the first read
cannot appear temporarily as a parent while its repaired record passes through the current chunk.
The root also comes from the first read. Warm, cold and bounded caches therefore give the same
paths throughout a scan. A deleted path through a missing parent retains its `Lost` marker.
