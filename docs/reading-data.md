# Reading file data

`NtfsFile::open_stream` returns a `StreamReader` (`Read + Seek`) over one data stream. It reads
from the raw volume, so it works on a file Windows has open with no sharing, one you are denied,
or a deleted one, without ever buffering the whole stream. This page covers what the reader does
per stream kind, its errors, and what it cannot read. Other guides: [deleted files](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/deleted-files.md), [paths and caches](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/paths-and-caches.md), [the journal](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/journal.md).

## Opening a stream

`open_stream(None)` opens the default (unnamed) stream: the file's contents. `open_stream(Some(name))`
opens an alternate data stream (`NtfsFile::data_streams` lists the names); the name must match
exactly, case included, and `Some("")` is not the default stream. A directory has no default
stream: `NtfsReaderError::StreamNotFound`, same as a nonexistent name.

```rust,no_run
# use std::ffi::OsStr;
# use std::io::{Read, Seek, SeekFrom};
# use ntfs_reader::{Mft, Volume};
# fn main() -> Result<(), Box<dyn std::error::Error>> {
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
# Ok(())
# }
```

It needs the same elevated access as `Mft::new`, but not always: a resident stream comes from the
MFT record with no handle opened, and the volume itself opens lazily, on the first read of
clustered bytes. A stream with nothing stored (sparse, empty, lost) needs no elevation at all.

## What the reader returns

- **Resident streams** (small enough for the MFT record) come from the record.
- **Non-resident streams** are read from the clusters their data runs name, fragmented or not,
  across the file's extension records.
- **Sparse parts** (holes) read as zeroes.
- **Bytes at or beyond the initialized size** read as zeroes regardless of the clusters behind
  them, since NTFS never wrote them; this holds inside a `Missing` extent too, so a deleted file
  whose missing extension record held only never-written clusters reads fully.
- Reading never goes past `size()`; a seek past it reads nothing.
- Names are exact (case included). Reads are sector aligned inside the reader; read any amount at
  any offset.

`StreamReader::size` is the logical size, `initialized_size` the part NTFS wrote, `cluster_size`
the unit of the runs, and `data_lost` marks a deleted file whose runs were lost (see
[deleted files](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/deleted-files.md)).

## Extents

`StreamReader::extents` says where each part of the stream is, in order, 0 to `size()`, no gaps
or overlaps. Nothing there reads the volume.

| `ExtentLocation`  | Meaning                                                                                              |
| ----------------- | ---------------------------------------------------------------------------------------------------- |
| `Resident`        | In the MFT record: the whole stream when small.                                                      |
| `Volume { offset }` | On the volume, starting at this byte offset from the volume's start.                               |
| `Sparse`          | Not stored: reads as zeroes. Adjacent holes are one extent.                                          |
| `Missing`         | The record holding this part's extent was not found: nothing says where its bytes are.               |

```rust,no_run
# use ntfs_reader::{ExtentLocation, Mft, Volume};
# fn main() -> Result<(), Box<dyn std::error::Error>> {
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
# Ok(())
# }
```

A `Missing` extent makes recovery of a deleted file partial: an extension record was freed and
reused, so its extent is gone (distinct from `data_lost`, a stream whose runs NTFS wiped). A read
reaching it fails with `NtfsReaderError::StreamExtentMissing { offset }`, inside the `io::Error`
(kind `InvalidData`), after returning the bytes before it. Extract it with `io::Error::get_ref`
and a downcast; seek past to read what follows.

## What it refuses, and what it cannot detect

- `NtfsReaderError::CompressedStream`, `EncryptedStream`: NTFS-compressed (LZNT1) or
  EFS-encrypted bytes are not the contents; the crate does neither decompression nor decryption.
  Sparse streams are fine.
- `NtfsReaderError::WofCompressedStream`: a CompactOS (WOF) compressed file's default stream is a
  sparse placeholder reading as zeroes; the contents sit in the alternate stream
  `WofCompressedData`. Recognised by that stream alone, not by reparse tag. Opening
  `WofCompressedData` by name still works, returning the raw, undecompressed bytes.
- `NtfsReaderError::InvalidDataRun`: corrupt runs, runs outside the volume, overlapping extents,
  no VCN-0 extent (size unknown), or more than 4,194,304 (2^22) extents. An absent extent is not
  an error: it is a `Missing` extent.

Not detected, read as zeroes or junk instead of an error: a Data Deduplication file (contents in
the chunk store), a Cloud Files placeholder (OneDrive and other sync providers), and an HSM stub
all read as they sit on the volume. A reparse point (`FILE_ATTRIBUTE_REPARSE_POINT` in
`FileInfo::file_attributes`) spots them, though symbolic links and junctions carry one too.

## The reader is not the disk

The reader reads through a normal volume handle, not the disk itself. A file being written, or
written moments ago, may read old bytes until the volume is flushed. Flush before opening the
`Mft` and the streams (see [deleted files](https://github.com/kikijiki/ntfs-reader/blob/v0.5.2/docs/deleted-files.md), "Flush the volume first"):

```rust,no_run
# fn main() -> std::io::Result<()> {
// `sync_all` on a volume opened for writing is `FlushFileBuffers`. Nothing is written.
// It needs the same elevation as reading the volume. In PowerShell: `Write-VolumeCache C`.
std::fs::OpenOptions::new().read(true).write(true).open(r"\\.\C:")?.sync_all()?;
# Ok(())
# }
```

A flush writes the cache out without invalidating it, and a normal-handle read can be served from
the system cache. So after a delete or TRIM, the reader may return bytes the cache still holds
while the disk already has zeroes. Measured once on a virtual disk: a raw
`FILE_FLAG_NO_BUFFERING` read saw the freed clusters zeroed at 10 s, while the reader returned
old bytes for the full 90 s watched. Treat the bytes as what the cache had; a `NO_BUFFERING`
handle sees the disk, but needs sector-aligned offsets, lengths and buffer addresses the reader
does not ask of callers.

The reader holds the last span read from the volume (up to 256 KiB, aligned sectors) and serves
reads inside it from memory, even after seeking back in, so it returns what was on the volume
when the span was read, not what is there now. For a deleted file this can mean clusters reused
since; the reader does not check the cluster bitmap (`StreamReader::allocation` does, on
request).

## The end of the volume

The reader never asks the volume for bytes past its end. A Windows volume handle is readable only
to the end of its last whole cluster: a raw read crossing that end is short, one past it returns
0 bytes. A stream ending in the last clusters is read with reads that stop there; nothing you
need to handle.

## Copying a stream

`StreamReader` is a plain `Read`, so `std::io::copy` works, and the `recover_file` example copies
a deleted file with a progress line:

```rust,no_run
# use std::fs::File;
# use ntfs_reader::{Mft, Volume};
# fn main() -> Result<(), Box<dyn std::error::Error>> {
let mft = Mft::new(Volume::new(r"\\.\C:")?)?;
// `Mft::record` finds a record by number, live or deleted (`mft.files()` lists live files only).
if let Some(file) = mft.record(1234) {
    let mut stream = file.open_stream(None)?;
    // Write to another volume when the file is deleted: see the deleted files guide.
    std::io::copy(&mut stream, &mut File::create(r"D:\copy.bin")?)?;
}
# Ok(())
# }
```
