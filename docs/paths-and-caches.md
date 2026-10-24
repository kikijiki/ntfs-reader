# Paths and path caches

The MFT stores only a name and a parent for each file, so a full path is built by walking up the
parent chain. This page covers live and deleted paths, path caches, and scanning large volumes.

Other guides: [deleted files](deleted-files.md), [the journal](journal.md),
[reading data](reading-data.md).

## Live paths

`Mft::resolve_path(&name, &mut cache)` returns the full path of one name of a file.

- `name` is one of `NtfsFile::hard_links`, so a file with several hard links has several paths.
- The path starts with the volume path, for example `\\.\C:\Users\me\file.txt`.
- Components come from `NtfsFileName::to_os_string`, so a name that isn't valid UTF-16 still gives a
  path that opens the file.
- The result doesn't depend on the cache. Warm or cold, it is the same.

It returns `None` when:

- a parent is missing, not in use, unnamed, not a directory, or stale (its record number was freed
  and reused, so the full reference including the sequence number is compared)
- the parent chain loops
- the path would exceed Win32's limit of 32767 UTF-16 units, counting the volume path and
  separators (a character above U+FFFF takes two units)

A live file never resolves through a freed or reused directory. Extension records are refused as
parents too, whatever their directory flag says, because a parent reference never names one on an
undamaged volume.

`FileInfo::path` is the same thing for a whole file, using its best name.

```rust,no_run
use ntfs_reader::{DefaultPathCache, Mft, Volume};
fn main() -> Result<(), Box<dyn std::error::Error>> {
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
    Ok(())
}
```

`FileInfo` summarizes a file: `name`, `path`, `is_directory`, `is_deleted`, `data_lost`, `size`,
`file_attributes`, and the times `created`, `accessed` and `modified`. `NtfsFile` gives every
attribute across the base and extension records, with typed views:

```rust,no_run
use ntfs_reader::{DefaultPathCache, FileInfo, Mft, Volume};
fn main() -> Result<(), Box<dyn std::error::Error>> {
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
    Ok(())
}
```

The root directory is record `ROOT_RECORD` (5). Ordinary files start at `FIRST_NORMAL_RECORD` (24).
`NtfsFileName::is_in_root` checks only the parent's record number.

## The path cache

`FileInfo::with_cache` and `Mft::resolve_path` take a `PathCache`. It remembers each directory's
path as it is walked, so later lookups stop at the first cached parent.

- `DefaultPathCache` keeps one entry per directory. It is cheap for a few lookups and pays off on a
  full scan.
  - `new()` is unbounded.
  - `with_max_bytes(n)` evicts the least recently used directory once the cost would exceed `n`.
    Cost follows real heap use, so the memory held tracks `n`.
  - A bound never changes what `resolve_path` returns, only how much walking a later lookup
    repeats.
  - `bytes()` and `len()` report what is held.
- `()` caches nothing. `FileInfo::new` uses it, and it is fine for a single lookup.
- `PathCache` is a trait, so you can supply your own. `get` takes `&mut self`, because a hit counts
  as a use for eviction order.
- `DeletedPathCache` has no size limit. Its entries point at each other, so evicting one could
  leave a dangling pointer.

### Sharing across threads

- `Mft`, `NtfsFile`, `StreamReader`, `ClusterBitmap`, `MftChunk` and the path caches are all
  `Send + Sync`, so one `Mft` can be shared.
- A path cache needs `&mut`, so give each thread its own.
- `MftScan` is `Send` but not `Sync`, because `next_chunk` needs `&mut self`.

### Performance

Measured on 0.5.0 on the system volume of a Windows 11 VM (175k files, 190k MFT records, 186 MiB MFT
in memory), with a fresh cache per run. Not repeated for 0.5.2, and the deleted-file walk is not
included.

| Files resolved | No cache | `DefaultPathCache` | Cache heap |
| -------------- | -------- | ------------------ | ---------- |
| 10             | 23 us    | 21 us              | < 0.01 MiB |
| 100            | 379 us   | 252 us             | 0.02 MiB   |
| 1,000          | 3.2 ms   | 1.5 ms             | 0.18 MiB   |
| All (175k)     | 483 ms   | 218 ms             | 6.2 MiB    |

Dropping a full-scan cache takes about 4 ms. Numbers depend on the volume and machine. To measure
your own, run elevated with `NTFS_READER_TEST_VOLUME` set to the drive letter (the benchmarks fail
without it, see CONTRIBUTING.md):

```sh
set NTFS_READER_TEST_VOLUME=T
cargo bench --bench mft_benchmark   # time
cargo bench --bench cache_memory    # heap held by the cache
```

## Paths of deleted files

`Mft::resolve_path` never goes through a freed directory. A deleted file's path does, so it needs a
separate function that also records where it had to guess.

`Mft::resolve_deleted_path(&name, &mut DeletedPathCache)` returns a `DeletedPath` with:

- `marker`: `None` if every directory was identified, otherwise a `DeletedPathMarker`
- `path`: the names that were resolved

A parent is followed if it is a live directory, or a freed one: not in use, not allocated, and with
a sequence number one above the reference's (what deleting a directory does to its record). It must
be a base record with the directory flag. The names used are whatever the record still holds.

When the walk can't continue it doesn't return `None`. It stops at a marker and keeps what it
resolved below that point.

- A complete path starts with the volume path, like `\\.\C:\dir\file.txt`.
- An incomplete path is relative and holds the names below the marker, for example `dir\file.txt`
  under `Lost(1234)`.
- `path` never contains marker text, so a recovery tool can recreate it under its own folder and
  name the marker's folder as it likes.
- For display, `found.to_marked_path(mft.volume().path())` inserts the marker's `Display` text as a
  component: `\\.\C:\<lost 1234>\dir\file.txt`. The text contains `<` and `>`, which Win32 names
  can't, so it is never a path to create a file at.

| Marker        | Displays as  | Why the walk stopped                                                                                        |
| ---------- | ------------ | ----------------------------------------------------------------------------------------------------------- |
| `Lost(N)`  | `<lost N>`   | Record N is missing, unnamed, not a directory, reused, or another incarnation. A loop of directories collapses to one `Lost(N)`, N its lowest record. Marks a directory record only; unrelated to `data_lost` or `AllocationState::Lost`. |
| `Deleted`  | `<deleted>`  | `$Extend\$Deleted`: a directory `remove_dir_all` renamed before deleting, or a file deleted while open. What is below it keeps the random name NTFS gave it. |
| `TooLong`  | `<too long>` | Path would exceed 32767 UTF-16 units, marker text included; `path` is the name alone.                       |

- Deleting one entry at a time with `remove_file` and `remove_dir` keeps the path complete.
  `remove_dir_all` ends it at `Deleted`, because the directories were renamed first and their real
  names are gone.
- NTFS keeps no rename history. A directory shows the name its record holds, at deletion or, if
  still live, now. So a path shows where the file was when its directories were last renamed,
  not necessarily where it was when it was deleted.

### The deleted path cache

`DeletedPathCache` is a separate type, not a `PathCache`, because a deleted walk crosses freed
directories and ends at markers, which a live lookup must never see.

- It remembers every directory it passes, including loops and too-long paths.
- Use one cache for a whole scan, so each directory is walked once. A fresh cache per call walks the
  whole chain every time.
- If you reuse a cache (this one or a `DefaultPathCache`) with a different `Mft`, it starts empty
  for the new one. That is safe but wastes the warm cache, so give each `Mft` its own.

`FileInfo` uses the same walk. For a deleted file, `path` is `Some` only when it is complete.
`FileInfo::new` and `with_cache` create a cache of their own and drop it right after, so a scan
should use `with_caches` with one of each cache.

```rust,no_run
use ntfs_reader::{DefaultPathCache, DeletedPathCache, DeletedPathMarker, FileInfo, Mft, Volume};
fn main() -> Result<(), Box<dyn std::error::Error>> {
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
    Ok(())
}
```

## Scanning a large volume

`Mft::new` keeps the whole `$MFT` in memory, about 1 KiB per file. A million files cost over 1 GiB.

`MftScan` visits every file, live and deleted, without keeping it all. It reads `$MFT` twice:

1. Once to index directories and extension records. This takes a few MiB even for a million files
   (see `MftScan::size_in_memory`).
2. Once to return the files, chunk by chunk.

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

- `MftChunk::files`, `deleted_files`, `record`, `resolve_path` and `resolve_deleted_path` work like
  their `Mft` counterparts.
- `FileInfo::with_cache` and `with_caches` take a file from a chunk the same way as from an `Mft`.
- Share one `PathCache` or `DeletedPathCache` across the whole scan, not one per chunk. A scan is
  one reference space, like an `Mft`.
- `record(number)` returns a base record in the current chunk, or a retained record. It returns an
  extension record only if its base is also available (usually in the current chunk), otherwise
  `None`. `files()` and `deleted_files()` still include all retained extensions for each base.
- `MftScan::new` reads 4 MiB chunks. `MftScan::with_chunk_records` picks another size.

### Live volumes

The two reads aren't one snapshot, so on a live volume they can disagree.

- A directory or extension record that the first read indexed is seen as the first read saw it, in
  every chunk. Everything else is as the second read found it.
- A directory renamed between the reads keeps its old name in paths.
- A directory created after the first read isn't seen by path resolution, so files under it get no
  path.
- A record reused as a plain file after the first read still reads as what the first read indexed.
- Reading a shadow copy twice avoids all of this, because both reads see the same snapshot (see
  [shadow copies](shadow-copies.md)).

Paths always use the directory index from the first read, including for directories it rejected.
The root also comes from the first read. So warm, cold and bounded caches give the same paths
throughout a scan, and a deleted path through a missing parent keeps its `Lost` marker.
