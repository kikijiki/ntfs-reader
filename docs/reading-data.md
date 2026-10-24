# Reading file data

`NtfsFile::open_stream` returns a `StreamReader` (`Read + Seek`) over one data stream. It reads from
the raw volume, so it works on files Windows has locked, files you are denied access to, and
deleted files. It never buffers the whole stream.

Other guides: [deleted files](deleted-files.md), [paths and caches](paths-and-caches.md),
[the journal](journal.md).

## Opening a stream

- `open_stream(None)` opens the default (unnamed) stream, which is the file's contents.
- `open_stream(Some(name))` opens an alternate data stream. `NtfsFile::data_streams` lists them.
- The name must match exactly, case included. `Some("")` is not the default stream.
- A directory has no default stream. Opening one, or a name that doesn't exist, returns
  `NtfsReaderError::StreamNotFound`.

```rust,no_run
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom};
use ntfs_reader::{Mft, Volume};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mft = Mft::new(Volume::new(r"\\.\C:")?)?;
    for file in mft.files().filter(|file| !file.is_directory()).take(10) {
        let Ok(mut stream) = file.open_stream(None) else { continue };
        stream.seek(SeekFrom::Start(0))?;
        let mut head = [0u8; 16];
        let count = stream.read(&mut head)?;
        println!("{} bytes, starts with {:02x?}", stream.size(), &head[..count]);

        // An alternate data stream, if the file has that one.
        if let Ok(mut zone) = file.open_stream(Some(OsStr::new("Zone.Identifier"))) {
            let mut text = String::new();
            zone.read_to_string(&mut text)?;
            println!("{text}");
        }
    }
    Ok(())
}
```

Elevation is needed as for `Mft::new`, with some exceptions. A resident stream comes from the MFT
record without opening a handle, and the volume is opened lazily on the first read of clustered
bytes. A stream with nothing stored (sparse, empty, lost) needs no elevation.

## What the reader returns

| Part of the stream                         | Reads as                                                          |
| ------------------------------------------ | ----------------------------------------------------------------- |
| Resident (small enough for the MFT record) | The bytes in the record.                                          |
| Non-resident                               | The clusters named by the data runs, across extension records.    |
| Sparse hole                                | Zeroes.                                                           |
| At or beyond the initialized size          | Zeroes, whatever the clusters hold. NTFS never wrote them.        |

- The initialized-size rule also applies inside a `Missing` extent. A deleted file whose missing
  extension record held only never-written clusters still reads fully.
- Reading never goes past `size()`. A seek past it reads nothing.
- Reads are sector aligned inside the reader, so you can read any amount at any offset.

Sizes:

- `size()`: the logical size.
- `initialized_size()`: the part NTFS wrote.
- `cluster_size()`: the unit of the data runs.
- `data_lost()`: the file was deleted and its runs were lost (see [deleted files](deleted-files.md)).

## Extents

`StreamReader::extents` says where each part of the stream is. They are in order from 0 to `size()`,
with no gaps or overlaps. It doesn't read the volume.

| `ExtentLocation`  | Meaning                                                                                              |
| ----------------- | ---------------------------------------------------------------------------------------------------- |
| `Resident`        | In the MFT record (the whole stream, when it is small).                                              |
| `Volume { offset }` | On the volume, starting at this byte offset from the start of the volume.                          |
| `Sparse`          | Not stored, reads as zeroes. Adjacent holes are one extent.                                          |
| `Missing`         | The record holding this part's extent wasn't found, so nothing says where its bytes are.             |

```rust,no_run
use ntfs_reader::{ExtentLocation, Mft, Volume};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mft = Mft::new(Volume::new(r"\\.\C:")?)?;
    for file in mft.files().filter(|file| !file.is_directory()).take(5) {
        let Ok(stream) = file.open_stream(None) else { continue };
        let stored: u64 = stream
            .extents()
            .iter()
            .filter(|extent| matches!(extent.location, ExtentLocation::Volume { .. }))
            .map(|extent| extent.length)
            .sum();
        println!("{}: {} extents, {stored} of {} bytes stored in clusters",
            file.number(), stream.extents().len(), stream.size());
    }
    Ok(())
}
```

A `Missing` extent makes recovery of a deleted file partial. An extension record was freed and
reused, so the extent is gone. This is different from `data_lost`, where NTFS zeroed the runs.

- A read that reaches it fails with `NtfsReaderError::StreamExtentMissing { offset }`, inside an
  `io::Error` of kind `InvalidData`. The bytes before it are returned first.
- Get the error with `io::Error::get_ref` and a downcast.
- Seek past it to read what follows.

## What it refuses

| Error                                   | Cause                                                                 |
| --------------------------------------- | --------------------------------------------------------------------- |
| `CompressedStream`                      | NTFS compression (LZNT1). The crate doesn't decompress.               |
| `EncryptedStream`                       | EFS encryption. The crate doesn't decrypt.                            |
| `WofCompressedStream`                   | CompactOS (WOF) compression. See below.                               |
| `InvalidDataRun`                        | Corrupt runs, runs outside the volume, overlapping extents, no VCN-0 extent (size unknown), or more than 4,194,304 (2^22) extents. |

- Sparse streams are fine.
- A WOF file's default stream is a sparse placeholder that reads as zeroes. The contents are in the
  alternate stream `WofCompressedData`. WOF files are recognized by that stream, not by the reparse
  tag. Opening `WofCompressedData` by name still works and returns the raw, compressed bytes.
- A missing extent is not an error. It is a `Missing` extent.

### What it can't detect

These read as zeroes or junk instead of returning an error:

- Data Deduplication files (the contents are in the chunk store)
- Cloud Files placeholders (OneDrive and other sync providers)
- HSM stubs

You can spot them by the reparse point attribute (`FILE_ATTRIBUTE_REPARSE_POINT` in
`FileInfo::file_attributes`), but symbolic links and junctions have it too.

## The reader is not the disk

The reader uses a normal volume handle, not the disk itself.

- A file that is being written, or was written moments ago, can read old bytes until the volume is
  flushed. Flush before opening the `Mft` and streams (see
  [deleted files](deleted-files.md#flush-the-volume-first)).
- A flush writes the cache out without invalidating it, and reads can be served from the system
  cache. After a delete or TRIM, the reader can return bytes the cache still holds while the disk
  already has zeroes.
- In one test on a virtual disk, a raw `FILE_FLAG_NO_BUFFERING` read saw the freed clusters zeroed
  at 10 s. The reader returned the old bytes for the full 90 s it was watched.
- A `NO_BUFFERING` handle sees the disk, but needs sector-aligned offsets, lengths and buffers,
  which the reader doesn't require from callers.

The reader also keeps the last span it read (up to 256 KiB, aligned sectors) and serves reads inside
it from memory, even after seeking back. It returns what was on the volume when the span was read,
not what is there now.

- For a deleted file, clusters may have been reused since.
- The reader doesn't check the cluster bitmap. `StreamReader::allocation` does, on request.

## The end of the volume

A Windows volume handle can only be read up to the end of its last whole cluster. A raw read that
crosses that end is short, and one past it returns 0 bytes. The reader never asks for bytes past the
end, so there is nothing for you to handle.

## Copying a stream

`StreamReader` is a plain `Read`, so `std::io::copy` works. The `recover_file` example copies a
deleted file and prints progress.

```rust,no_run
use std::fs::File;
use ntfs_reader::{Mft, Volume};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mft = Mft::new(Volume::new(r"\\.\C:")?)?;
    // `Mft::record` finds a record by number, live or deleted (`mft.files()` lists live files only).
    if let Some(file) = mft.record(1234) {
        let mut stream = file.open_stream(None)?;
        // Write to another volume when the file is deleted: see the deleted files guide.
        std::io::copy(&mut stream, &mut File::create(r"D:\copy.bin")?)?;
    }
    Ok(())
}
```
