# Deleted files

How to read deleted files and what you can expect to get back. This reads the raw volume, so it
needs an elevated process (see "Opening a volume" in the README).

Deleting a file frees its MFT record and clusters, and not much else. The name, times, size and
cluster list usually stay in the record, and the data stays in the clusters until NTFS reuses
them. Recovery is best effort.

Other guides: [reading data](reading-data.md), [paths and caches](paths-and-caches.md),
[the journal](journal.md).

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

- `is_deleted()` means the file's base record is freed.
- `deleted_files()` is narrower. It also needs record number 24 or higher and a
  `$STANDARD_INFORMATION` or `$FILE_NAME`, so some freed records are not listed.
- Accessors work the same on live and deleted files, so code for `mft.files()` also works on
  `mft.deleted_files()`.
- `NtfsFile::file_id` keeps the live id, so `record_by_id(file.file_id())` finds the file either way.
  The sequence number in `reference` goes up by one on delete.

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
        // An incomplete path has a marker (`found.marker`) where a directory is unknown, shown as
        // `\\.\C:\<lost 1234>\dir\file.txt`; `found.path` alone holds only the real names.
        println!("{}", found.to_marked_path(mft.volume().path()).display());

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

`examples/list_deleted.rs` and `examples/recover_file.rs` add the checks a real tool needs: partial
copies, refused destinations, exit codes. Paths and markers are in
[paths and caches](paths-and-caches.md).

## Flush the volume first

The reader goes through a normal volume handle, so a file deleted a moment ago can still look live.
It can also show up in neither `Mft::files()` nor `Mft::deleted_files()`, because its in-use flag
and `$BITMAP` bit are written at different times.

Flush before `Mft::new`, `ClusterBitmap::new` and `open_stream`:

```rust,no_run
fn main() -> std::io::Result<()> {
    // `sync_all` on a volume opened for writing is `FlushFileBuffers`. Nothing is written.
    // It needs the same elevation as reading the volume. In PowerShell: `Write-VolumeCache C`.
    std::fs::OpenOptions::new().read(true).write(true).open(r"\\.\C:")?.sync_all()?;
    Ok(())
}
```

## How much you get back

Check the allocation before trusting the bytes. There are three outcomes.

### Whole

Nothing was written to the clusters after the delete.

- `allocation()` gives `AllocationState::Free` and the bytes are the file's own.
- `Free` only means "unallocated now". If file B took the clusters and was deleted too, both files
  show `Free` and both read B's bytes.

### Partly

NTFS gave some clusters to new files.

- The state is `PartlyAllocated`. `StreamAllocation` counts bytes in free clusters
  (`in_free_clusters`) and in allocated ones (`in_allocated_clusters`). The latter belong to
  another file now.
- A stream can also have `missing` bytes, when the extension record that held their location is
  gone. Reading them fails with `NtfsReaderError::StreamExtentMissing`, inside the `io::Error`,
  after the bytes before them. The state is then `Incomplete`.

### Not at all

Either every cluster was reused (`Allocated`), or the record lost the data's location
(`data_lost` is true, state `Lost`).

- `data_lost` affects deleted files with an `$ATTRIBUTE_LIST`. NTFS creates one for files with many
  hard links or alternate streams (30 of either were tested).
- On delete, NTFS zeroes such a file's non-resident stream sizes and cluster lists. The streams
  open as empty, the same as a stream that really was empty. Only `data_lost` tells them apart.
- A resident stream stays intact even if another stream of the same file is lost. `data_lost` is
  per stream, and `stream_data_lost` is per file.

### States

| State             | Meaning                                                                | What to do                                          |
| ----------------- | ---------------------------------------------------------------------- | --------------------------------------------------- |
| `Free`            | In a cluster that is unallocated now. Not necessarily untouched.       | Read it, then check the bytes.                      |
| `PartlyAllocated` | Some bytes in free clusters, some in allocated ones.                   | Use `in_free_clusters` and `in_allocated_clusters`. |
| `Allocated`       | Every byte is in a cluster another file uses now.                      | Nothing to recover.                                 |
| `NoStoredData`    | Empty, resident, sparse, or past the initialized size.                 | A resident stream reads normally.                   |
| `Incomplete`      | Part has no known location (`missing`, `outside_bitmap`). Wins over the others. | Read what is located, expect an error at the rest. |
| `Lost`            | Runs lost on delete (`data_lost`), nothing else known to be missing.   | Nothing can be located.                             |

`AllocationState` is `#[non_exhaustive]`, so keep a `_` arm.

### TRIM

A free cluster is no guarantee either. On a disk that gets TRIM, freed clusters are erased within
seconds, and `AllocationState::Free` doesn't change.

- Physical NVMe SSD: intact for 5 to 11 s after the delete, then erased within half a second
  (a few edge clusters aside).
- Virtual disk: erased after 6 to 20 s. One busy run saw nothing erased in 30 s.
- Delete notifications off (`fsutil behavior set DisableDeleteNotify NTFS 1`): nothing erased in
  22 minutes.
- Erased clusters read as zeroes, or as random-looking bytes on BitLocker (the SSD's zeroes get
  decrypted).

The reader returns what the volume handle gives, which isn't always what is on the disk (see
[The reader is not the disk](reading-data.md#the-reader-is-not-the-disk)). Zeroes or noise are an
answer, not an error.

## Why recovery fails

### Record reuse

NTFS gives the lowest free record number to the next file created. On a quiet volume that was the
very next file. Name, times and size are gone once a new file takes the record, and low record
numbers go first.

### Cluster reuse

New data goes to free clusters unpredictably. Read right after the delete, and don't write to the
volume you are recovering from.

### TRIM

See above.

### Deleted while open

With `std::fs::remove_file`, the record stays in use under `$Extend\$Deleted` with a random name
until the last handle closes. Until then `Mft::files()` lists it and `is_deleted` is `false`.

After the close the record is freed, and the real name and directory are lost for good.
`DeletedPath::marker` is `Some(DeletedPathMarker::Deleted)` and `path` is `<random name>`
(displayed as `\\.\C:\<deleted>\<random name>`). Windows Defender kept files open like this in
testing.

### `remove_dir_all`

It renames each directory to a random name before deleting it.

- Files keep their names, but directory names are lost.
- The marker is `Deleted` where the directories were, and `path` is `<random name>\file.txt`.
- Deleting entry by entry (`remove_file`, then `remove_dir`) keeps every name.
- `del`, `rd /s`, `Remove-Item -Recurse` and a Shell permanent delete behave like `remove_file`.

### Recycle Bin

Moving a file to the Recycle Bin is a rename, not a delete. The file stays live under
`$RECYCLE.BIN`.

## Notes

- Delete doesn't change the created, modified or accessed times, so there is no deletion time. The
  crate has no "deleted first" order.
- `Mft` and `ClusterBitmap` are snapshots. A file deleted after `Mft::new` is still live in it, and
  a file created after isn't in it. Load a new `Mft` to see a new delete.
- A deleted record may have lost its name. `best_name` is then `None` and `resolve_deleted_path` has
  nothing to start from.
- A deleted directory loses its child list. To find its children, scan for names whose parent is
  the directory: `file.names().any(|name| name.parent_id() == directory.file_id())`. Don't compare
  `parent_reference()` with `reference()`, because the sequence number of a freed record is one
  higher.
- Compressed and encrypted streams can't be read (see [reading data](reading-data.md)).
- For sharing an `Mft` across threads, see "Sharing across threads" in
  [paths and caches](paths-and-caches.md).

### Finding what a journal delete removed

Load the `Mft` before the delete, so the file and its parent are both live records.

- `record_by_id(record.file_id)` returns the file, `resolve_path` works, and `record.parent_id`
  names the directory.
- An `Mft` loaded later still finds the file if its record wasn't reused. `record_by_id` accepts a
  freed record but rejects a reused one.
- If the directory was deleted too, `resolve_path` returns `None` but `resolve_deleted_path` still
  works. See "Paths of deleted files" in [the journal](journal.md).

## Measurements

Windows 11 (build 10.0.26200), NTFS 3.1, 4 KiB clusters, 1 KiB file records, 8.3 names on. Tested on
a quiet 600 GB volume on a VM's virtual disk, and on a laptop's system volume (NVMe SSD,
BitLocker). Deletions used `std::fs::remove_file` and `remove_dir` unless noted.

- Delete changes only three header fields. Names, times, sizes, resident data and cluster lists
  survived for resident, contiguous, fragmented (16 runs), sparse and alternate-stream files.
- A file with an attribute list loses its non-resident streams. This happened with hard links,
  streams or data runs alone, with resident or non-resident lists, and for sparse and compressed
  files. The clusters keep their bytes until reused or trimmed.
- A 4 MiB file read right after deletion had all 1,024 clusters intact. NTFS didn't reuse them for
  the next file in two runs. Reusing all of them took 124 MiB and 250 MiB of new data.
- TRIM: see the numbers above.
- The sequence number wraps from 0xFFFF to 0 on free. The next live use of the record gets 1.
- The next file created took the freed record. In another run, 20 new files took the 20 records of
  20 deleted files.

Not measured: other Windows builds, other cluster sizes, 8.3 names off, spinning disks, `chkdsk`,
EFS-encrypted files, and emptying the Recycle Bin from Explorer. Treat results as what the disk
holds, not as certainty.
