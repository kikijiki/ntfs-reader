# Deleted files

Reading deleted files: what you get back, and what was measured. Reads the raw volume, so it
needs an elevated process (see "Opening a volume" in the README). Other guides:
[reading data](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/reading-data.md),
[paths and caches](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/paths-and-caches.md), [the journal](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/journal.md).

Deleting a file frees its MFT record and clusters; little else changes. Name, times, size and
cluster list usually stay in the record; the data stays in the clusters until NTFS reuses them.
This crate reads all of it. Recovery is best effort.

## What you can get

| You want                  | Use                                                                                              |
| ------------------------- | ------------------------------------------------------------------------------------------------ |
| The list of deleted files | `Mft::deleted_files()`, same `NtfsFile` as `Mft::files()`, `is_deleted()` true.                  |
| Names                     | `NtfsFile::names`, `hard_links`, `best_name`. `FileInfo::name`.                                  |
| Sizes                     | `NtfsFile::data_streams`. `FileInfo::size`, `FileInfo::data_lost`.                                |
| Times and attributes      | `NtfsFile::standard_information`. `FileInfo::created`, `accessed`, `modified`.                   |
| Paths                     | `Mft::resolve_deleted_path` with `DeletedPathCache`. `FileInfo::path`.                           |
| Content                   | `NtfsFile::open_stream`, then read the `StreamReader`.                                           |
| Are the clusters reused   | `ClusterBitmap::new`, then `StreamReader::allocation` -> `StreamAllocation`.                      |
| Is the data lost          | `StreamReader::data_lost`, `NtfsDataStream::data_lost`. `FileInfo::data_lost` (default stream).  |
| Is it deleted             | `NtfsFile::is_deleted`. `FileInfo::is_deleted`.                                                  |
| What a journal delete was | `Mft::record_by_id` with a `FILE_DELETE` record's `file_id`.                                     |

`is_deleted()` means the file's base record is freed. `deleted_files()` is narrower: it also
needs record number 24+ and a `$STANDARD_INFORMATION` or `$FILE_NAME`, so a freed record without
either, or a freed system record, counts as deleted but is not listed. The accessors work the same
on live and deleted files, so code for `mft.files()` also works on `mft.deleted_files()`.
`NtfsFile::file_id` keeps a file's live id, so `record_by_id(file.file_id())` finds it either way;
`reference`'s sequence number bumps by one on delete.

```rust,no_run
use std::{fs::File, io};
use ntfs_reader::{AllocationState, ClusterBitmap, DeletedPathCache, Mft, Volume};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mft = Mft::new(Volume::new(r"\\.\C:")?)?;
    let bitmap = ClusterBitmap::new(&mft)?;
    let mut cache = DeletedPathCache::new();

    for file in mft.deleted_files().filter(|file| !file.is_directory()) {
        let Some(name) = file.best_name() else { continue };
        let found = mft.resolve_deleted_path(&name, &mut cache);
        // An incomplete path has a marker such as `<lost 1234>` where a directory is unknown.
        println!("{} (complete path: {})", found.path.display(), found.complete);

        let Ok(mut stream) = file.open_stream(None) else { continue };
        // A file that had an attribute list lost the location of its non-resident data when it
        // was deleted: such a stream opens as an empty one, and says it lost its data.
        if stream.data_lost() {
            continue;
        }
        // Free clusters may still hold the data, or may be zeroed already. Read and check.
        // `NoStoredData` is a resident stream (or a sparse or empty one): its bytes are in the
        // record and copy reliably. `PartlyAllocated` is a partial copy with other files' bytes
        // in it, so it is left out here.
        let state = stream.allocation(&bitmap)?.state();
        if matches!(state, AllocationState::Free | AllocationState::NoStoredData) {
            // Write to another volume: writing to this one can take the clusters you want.
            let mut out = File::create(format!(r"D:\recovered-{}.bin", file.number()))?;
            io::copy(&mut stream, &mut out)?;
        }
    }
    Ok(())
}
```

`examples/list_deleted.rs` and `examples/recover_file.rs` build on these calls with the checks a
real tool needs (partial copies, refused destinations, exit codes). Paths and their markers are
covered in [paths and caches](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/paths-and-caches.md).

## Flush the volume first

Everything here reads through a normal volume handle; the measurements below were made with the
volume flushed before every load. A file deleted a moment ago can still look live in a raw read,
or, since its in-use flag and `$BITMAP` bit are written at different times, appear in neither
`Mft::files()` nor `Mft::deleted_files()`. Flush before `Mft::new`, `ClusterBitmap::new` and
`open_stream`:

```rust,no_run
# fn main() -> std::io::Result<()> {
// `sync_all` on a volume opened for writing is `FlushFileBuffers`. Nothing is written.
// It needs the same elevation as reading the volume. In PowerShell: `Write-VolumeCache C`.
std::fs::OpenOptions::new().read(true).write(true).open(r"\\.\C:")?.sync_all()?;
# Ok(())
# }
```

## How much of a file you get back

Read the bytes, and look at the allocation first. There are three outcomes.

- **Whole.** Nothing was written to the clusters after delete: `allocation()` gives
  `AllocationState::Free`, and the bytes read are the file's own. `Free` only means unallocated
  now, not untouched: if file B took the clusters and was deleted in turn, both show `Free` and
  hold B's bytes. Measured: a file read right after deletion came back intact, every cluster.
- **Partly.** NTFS gave some clusters to new files: `AllocationState::PartlyAllocated`.
  `StreamAllocation` counts bytes in free clusters (`in_free_clusters`) and allocated ones
  (`in_allocated_clusters`); allocated-cluster bytes belong to another file now. A stream can also
  have `missing` bytes, when the extension record holding their location is gone: reading them
  returns `NtfsReaderError::StreamExtentMissing` (inside the `io::Error` from `read`) after the
  bytes before them. `AllocationState::Incomplete` then means part has no known location,
  regardless of the rest.
- **Not at all.** Either every cluster was reused (`Allocated`), or the record lost the data's
  location (`data_lost` is `true`, state `Lost`). The latter hits a deleted file with an
  `$ATTRIBUTE_LIST`, which NTFS creates for files with many hard links or alternate streams (30 of
  either tested): NTFS zeroes such a file's non-resident stream sizes and cluster lists on
  delete. They open as empty streams, same as one that was actually empty; only `data_lost` tells
  the two apart. A resident stream, small enough for the MFT record, stays intact even when
  another stream of the same file is lost (`stream_data_lost` per file, `data_lost` per stream).

What each `AllocationState` (from `StreamAllocation::state`) means for a deleted file:

| State             | Meaning                                                                          | What to do                                        |
| ----------------- | --------------------------------------------------------------------------------- | ------------------------------------------------- |
| `Free`            | Byte sits in a now-unallocated cluster. Not "untouched": see above.  | Read it, then check the bytes.                    |
| `PartlyAllocated` | Some bytes in free clusters, some allocated.                         | Use `in_free_clusters` and `in_allocated_clusters`. |
| `Allocated`       | Every byte is in a cluster another file uses now.                    | Another file's bytes. Nothing to recover.         |
| `NoStoredData`    | Nothing stored: empty, resident, sparse, or past initialized size.   | A resident stream reads normally.                 |
| `Incomplete`      | Part has no known place (`missing`, `outside_bitmap`). Wins over the others. | Read what's located, expect an error at the rest. |
| `Lost`            | Runs lost on delete (`data_lost`); nothing else known missing.       | Nothing can be located.                           |

`AllocationState` is `#[non_exhaustive]`: keep a `_` arm.

A free cluster is no guarantee either. On a disk that receives TRIM (an SSD, a thin-provisioned
virtual disk, any volume with delete notifications on), freed clusters are erased within seconds,
with no change to `AllocationState::Free`. On a physical NVMe SSD the whole file stayed intact for
5 to 11 s after the delete, then was erased within half a second, a few edge clusters aside; on a
virtual disk it took 6 to 20 s, and one busy run saw nothing erased in 30 s. Erased clusters read as
zeroes, or as random-looking bytes on a BitLocker volume, where the zeroes the SSD returns are
decrypted. With delete notifications off
(`fsutil behavior set DisableDeleteNotify NTFS 1`) nothing was erased in 22 minutes. The reader
returns whatever the volume handle gives, not necessarily what's on the disk (see [The reader is not the disk](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/reading-data.md#the-reader-is-not-the-disk)),
so zeroes or noise are an answer, not an error.

## What erodes it

- **Record reuse.** NTFS assigns the lowest free record number immediately; on a quiet volume the
  very next file created took it. A deleted file's name, times and sizes last only until a new
  file takes its record, so expect this fast on an active machine. Low record numbers go first.
- **Cluster reuse.** New data goes to free clusters, unpredictably. Read right after the delete,
  and do not write to the volume you recover from.
- **TRIM**, see above.
- **Deleted while open.** Measured with `std::fs::remove_file`: a file deleted while a program
  holds it open keeps its MFT record in use, under `$Extend\$Deleted` with a random name, until
  the last handle closes. Until then `Mft::files()` lists it under that name and
  `NtfsFile::is_deleted` is `false`. Once the handle closes, the record frees and the original
  name and directory are gone for good: only the random name remains, path
  `\\.\C:\<deleted>\<random name>`, `DeletedPath::complete` is `false`. Seen whenever something
  (Windows Defender, observed) still held the file open.
- **`remove_dir_all`.** `std::fs::remove_dir_all` renames each directory to a random name before
  deleting it. Files inside keep their names, but directory names are lost: path becomes
  `\\.\C:\<deleted>\<random name>\file.txt`, marker `<deleted>` where the real directories were,
  `DeletedPath::complete` is `false`. Deleting entries one at a time (`remove_file`, then
  `remove_dir`) keeps every directory name. `del`, `rd /s`, `Remove-Item -Recurse` and a Shell
  permanent delete left the same record state as `remove_file`.
- **The Recycle Bin.** Sending a file to the Recycle Bin is a rename, not a delete: it stays a
  live file under `$RECYCLE.BIN`, and `Mft::files()` lists it.

## Other things to know

- There is no deletion time: delete does not change a file's created, modified or accessed times.
  Modified is when the file was last written, not deleted. The crate offers no "deleted first"
  order.
- The `Mft` is a snapshot. A file deleted after `Mft::new` is still live in it; one created after
  is not in it. Load a new `Mft` to see a new delete. `ClusterBitmap` is a snapshot too.
- A deleted record may have lost its name: `NtfsFile::best_name` is then `None`, leaving
  `Mft::resolve_deleted_path` nothing to start from. A deleted directory loses its child list, so
  find them by scanning names whose parent is the directory:
  `file.names().any(|name| name.parent_id() == directory.file_id())`. Holds for live and deleted
  directories alike; `name.parent_reference() == directory.reference()` does not, since a freed
  record's sequence number is one higher.
- Compressed and encrypted streams cannot be read (see [reading data](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/reading-data.md)); like the rest of the crate, everything here needs an elevated process.
- One `Mft` can be shared across threads (`Mft`, `NtfsFile`, `StreamReader`, `ClusterBitmap` and
  the path caches are `Send + Sync`); a path cache needs `&mut`, so give each thread its own.
- To find what a USN journal `FILE_DELETE` record deleted, load the `Mft` before the delete: it
  then has both the record and its parent as live records (`record_by_id(record.file_id)` returns
  the file, `resolve_path` works, `record.parent_id` names its directory). An `Mft` loaded after
  can still find the file up to a point: `record_by_id` accepts a freed record, but rejects one
  NTFS has reused, which happened for the very next file on a quiet volume. Paths are a second
  problem when the directory was also deleted: `resolve_path` returns `None`, while
  `resolve_deleted_path` still walks it. See [the journal guide](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/journal.md), "Paths of deleted files".

## What was measured

Measured on Windows 11 (build 10.0.26200), NTFS 3.1, 4 KiB clusters, 1 KiB file records, 8.3
names on: a quiet 600 GB volume on a VM's virtual disk, and the system volume of a laptop (NVMe
SSD, BitLocker). Deletions used `std::fs::remove_file` and `remove_dir` unless noted.

- Delete changes only three header fields. Names, times, sizes, resident data and cluster lists
  survive for a resident file, a contiguous one, a fragmented one (16 runs), a sparse one, and one
  with alternate streams. A file with an attribute list loses its non-resident streams (from hard
  links, streams, or data runs alone; resident or non-resident list; sparse and compressed files
  too), while its clusters keep their bytes until reused or trimmed.
- A 4 MiB file deleted and read right away: all 1,024 clusters intact. NTFS did not reuse the freed
  clusters for the next file in two runs; reusing all of them took 124 MiB and 250 MiB of new
  data.
- TRIM: intact for 5 to 11 s after the delete on the laptop's SSD (10 runs), 6 to 20 s on the
  virtual disk, then erased. With delete notifications off, intact for 22 minutes.
- The sequence number wraps from 0xFFFF to 0 on free; the next live use of the record gets 1.
- The very next file created after one delete took its record; in another run, 20 new files took
  the 20 records of 20 deleted ones.

Not measured: other Windows builds, other cluster sizes, 8.3 names off, a spinning disk, `chkdsk`,
EFS-encrypted files, and Explorer's delete to the Recycle Bin followed by emptying it. Verify what
this crate reports; treat it as "what the disk holds", not certainty.
