# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.1] - 2026-09-25

### Fixed

- The volume reader treated a short read from the underlying handle as the end of the volume, so a read could fail
  with `UnexpectedEof` or come back truncated. Raw volume handles do not return short reads in practice, so this
  was not seen on real volumes ([#20](https://github.com/kikijiki/ntfs-reader/issues/20)).

## [0.5.0] - 2026-09-24

### Compared with 0.4.7

Measured on a Windows 11 VM against Win32 as the reference, on the VM's system drive (380k records), a 1.09M-file
stress volume and a volume whose `$MFT` is fragmented. 0.5.0 was not wrong anywhere 0.4.7 was right.

Where 0.4.7 is wrong:

- It cannot load a volume whose `$MFT` data is fragmented through an attribute list (`InvalidDataRun: data runs
  shorter than declared size`); 0.5.0 reads all 401k files of such a volume correctly.
- It returns no path and no name for files whose names all carry the reparse point flag: 2,429 files on the system
  drive (Edge, WebView and similar).
- It gets paths and hard links wrong below 1024 directory levels, turns names with an unpaired UTF-16 surrogate into
  paths that do not exist, and reports dates before 1970 or after 9999 wrongly.
- Journal: after a tree is deleted, after a double rename (A to B to C gives A as C's old name instead of B), for names
  with an unpaired surrogate, and with a small read buffer (it returned 2 of 122 records) its results are wrong.

Speed (hyperfine, x64, every operation includes loading the `$MFT`):

| Volume | Load `$MFT` | Full scan with path cache | 1000 lookups |
| --- | --- | --- | --- |
| System drive, 380k records | 6.4 s to 0.11 s (57x) | 5.9 s to 0.40 s (15x) | 5.5 s to 0.12 s (46x) |
| Stress volume, 1.09M records | 15.5 s to 0.28 s (56x) | 16.3 s to 0.95 s (17x) | 15.7 s to 0.30 s (52x) |

A full scan without a cache is 4 to 9 times faster. Peak memory while loading is the same (the `$MFT` dominates); a
scan with a cache uses 8 to 14% less, because only directories are cached. Reading the journal costs the same when every
record's path is resolved; 0.5.0 resolves paths only on request, and without them a read is 6 times faster.

### Added

- Attribute API on `NtfsFile`, covering the base record and its extension records: `attributes()`,
  `record_attributes()` (one record), `names`, `hard_links` (DOS 8.3 aliases skipped), `best_name`,
  `standard_information`, `data_streams` (default and alternate data streams, as `NtfsDataStream`) and
  `resident_data`.
- `NtfsAttribute::attribute_type`, `is_resident`, `name` and `value_size`.
- `Mft::resolve_path` resolves any name (for example each hard link) to a full path.
- The `PathCache` trait, `DefaultPathCache` and `CachedPath`. `PathCache` also caches paths that failed to
  resolve.
- `Mft::corrupt_records()` counts the allocated records skipped while loading. `Mft::volume()`,
  `record_count()` (the number of record slots, one past the last record number) and `size_in_memory()`
  replace the public fields.
- `Volume::path`, `cluster_size`, `volume_size`, `file_record_size` and `mft_position` replace the public
  fields.
- `NtfsStandardInformation::created`, `modified`, `mft_modified` and `accessed` (as `OffsetDateTime`) and
  `file_attributes`.
- Journal: `UsnReadResult` with a `caught_up` flag, `Journal::resolve_path`, `JournalPosition` (journal id
  plus USN), `journal_id`, `first_usn`, `next_usn` and `position` accessors, and
  `Journal::MIN_READ_BUFFER_SIZE`. `Journal` is `Send`.
- `Reason`, the USN change reason flags, with a constant for every `USN_REASON_*` flag (callers no longer need
  the `windows` crate), `EMPTY`, `ALL`, `bits`, `from_bits`, `contains` (every flag set), `intersects` (any
  flag set), `|` and `|=`, and a `Display` impl.
- `FileId::as_u128`, `FileId::as_reference`, and `From<u64>` and `From<u128>` for `FileId`.
- `NtfsFileName::parent_reference`, the parent directory's 64-bit reference (record number and sequence
  number), to check that the parent is still the same directory.
- `UsnRecord::file_attributes`, the record's `FILE_ATTRIBUTE_*` flags. After a `FILE_DELETE` it is the only
  thing that says whether a directory or a file was deleted.
- `NtfsFileName::to_os_string`, a lossless conversion. `Display` is now only a label.
- New `NtfsReaderError` variants: `AccessDenied`, `InvalidBootSector`, `InvalidVolumePath`,
  `InvalidUsnRecord`, `JournalNotActive`, `JournalEntryDeleted`, `JournalDeleteInProgress`,
  `JournalIdMismatch`, `ReadBufferTooSmall`. Error messages now include the cause.
- `Debug` for every public type (short for `Mft`, `Volume` and `Journal`), `Clone`, `PartialEq` and `Eq` for
  `FileInfo`, `Copy`, `PartialEq` and `Eq` for `NextUsn`, `HistorySize` and `NtfsStandardInformation`.
- Documentation for every public item. The README doubles as the crate documentation.
- Minimum supported Rust version is 1.88.

### Changed

- **Breaking:** the modules are private and every public item is exported from the crate root only. Import
  from the root (`ntfs_reader::{Mft, Volume, Journal, NtfsFile, FileInfo, ...}`); paths such as
  `ntfs_reader::mft::Mft`, `ntfs_reader::journal::Reason` or `ntfs_reader::api::NtfsAttributeType` no longer
  exist.
- **Breaking:** the accessors that return 64-bit file references are `reference`, `base_reference` and
  `parent_reference`, and the ones that return record numbers are `number`, `base_number` and
  `parent_number`: `NtfsFile::reference_number` is `reference`, `base_reference_number` is
  `base_reference`, `base_record_number` is `base_number`, and `NtfsFileName::parent` is `parent_number`.
- **Breaking:** `Mft::record_exists` is `is_allocated`, which says what it checks: only the `$BITMAP` bit, so
  `Mft::record` can still return `None` for a record it is `true` for. The public `max_record` field is
  `Mft::record_count()`.
- **Breaking:** `NtfsReaderError::MissingMftAttribute` is `MissingMftAttribute { attribute: &'static str }`
  instead of a tuple variant holding a `String`, like the other variants with data.
- **Breaking:** `FileInfo`, `UsnRecord`, `UsnReadResult` and `NtfsDataStream` are `#[non_exhaustive]`, so a
  field can be added without a breaking release. They can no longer be built with a struct literal or
  matched exhaustively outside the crate (use `..` in a pattern).
- **Breaking:** `NtfsAttributeType` has the other attribute types (`ObjectId`, `SecurityDescriptor`,
  `VolumeName`, `VolumeInformation`, `IndexRoot`, `IndexAllocation`, `ReparsePoint`, `EaInformation`, `Ea`
  and `LoggedUtilityStream`), so an exhaustive `match` needs new arms.
- `NtfsAttribute::standard_information` and `NtfsFile::standard_information` return the
  `NtfsStandardInformation` by value (it is `Copy`) instead of a reference.
- `Volume::new` rejects a path with a NUL character with `InvalidVolumePath`, before opening anything.
- **Breaking:** the `Journal` API was reshaped. `read` and `read_sized` return `UsnReadResult` instead of a
  `Vec`. `UsnRecord::timestamp` is an `OffsetDateTime`, `UsnRecord::reason` is a `Reason`, and
  `UsnRecord::path` is replaced by `UsnRecord::name` (file name only); call `Journal::resolve_path` when a
  full path is needed. It returns `Option<PathBuf>`: `None` when the file and its parent are gone, and a
  returned path is always absolute. `read_sized` takes the buffer size as an argument instead of a const
  generic and rejects buffers smaller than `Journal::MIN_READ_BUFFER_SIZE` (1024 bytes). `get_next_usn` is now
  `next_usn`, and `match_rename` returns the old name as an `OsString`. A saved `NextUsn::Custom` position
  now carries the journal id and is checked against the volume's current journal.
  `JournalOptions::max_history_size` now defaults to 4096 records instead of unlimited.
- **Breaking:** `JournalOptions::reason_mask` is a `Reason` instead of a `u32`.
- **Breaking:** `FileId` is a `u128` newtype (`Copy`, `Eq`, `Hash`, `Ord`) instead of the `Normal`/`Extended`
  enum, with no `windows` type inside. The same file compares equal whether its id came from the MFT or from
  a V2 or V3 journal record, and it can key a map. `NtfsFile::get_file_id` is now `NtfsFile::file_id`.
- **Breaking:** names read from disk are lossless `OsString`s: `NtfsDataStream.name` is `Option<OsString>`,
  `UsnRecord.name` is `OsString`, and `NtfsAttribute::name()` returns `Option<OsString>`. A name with an
  unpaired UTF-16 surrogate is no longer changed to U+FFFD, so it can open `path:stream` or be matched
  against names from the MFT. `FileInfo.name` stays a `String` label; use `NtfsFileName::to_os_string` or
  `FileInfo.path` for the exact name.
- **Breaking:** `FileInfo.path` is an `Option<PathBuf>`: `None` when the file has no name or its parent chain
  cannot be resolved (it was an empty `PathBuf`), like `Mft::resolve_path`.
- **Breaking:** `NtfsReaderError` is `#[non_exhaustive]` and holds no `windows` type. `IOError` is now `Io`,
  and Windows failures are `Io` carrying the OS error code (`raw_os_error()`) instead of `WindowsError`.
  Access denied is `AccessDenied` from `Volume::new`, `Mft::new` and `Journal::new` alike; `Volume::new` no
  longer pre-checks elevation. `MftRecordFixupFailed` replaces `CorruptMftRecord` and `CorruptMft`.
- **Breaking:** `PathCache` keys are full file references (record number plus sequence number), and
  `PathCache::get` returns a `CachedPath` (hit, cached failure or miss). `PathCache::insert_failed` was
  added.
- **Breaking:** `NtfsFile::attributes` is now an iterator over the whole logical file instead of taking a
  callback. Use `record_attributes()` for a single record.
- **Breaking:** an `NtfsFile` holds the `Mft` it came from, so the accessors that returned data of the
  whole file no longer take a `&Mft`: `attributes`, `names`, `hard_links`, `best_name`,
  `standard_information`, `data_streams` and `resident_data`. What they return still borrows from the `Mft`,
  not from the `NtfsFile`. `Mft::file_records(&file)` is `NtfsFile::records()`. `FileInfo::new` and
  `FileInfo::with_cache` take the file only. `Mft::resolve_path` is unchanged.
- **Breaking:** the fields of `Mft`, `NtfsFile`, `NtfsAttribute`, `NtfsFileName`, `NtfsStandardInformation`
  and `Volume` are private. Use the accessors, for example `Mft::volume()`, `NtfsFile::number()`,
  `Volume::cluster_size()` and `NtfsStandardInformation::created()`.
- **Breaking:** renames: `Mft::get_record` is `record`; `NtfsAttribute::get_resident` is `resident`,
  `as_standard_info` is `standard_information`, `as_name` is `file_name`, `as_resident_data` is
  `resident_data`.
- **Breaking:** the crate fails to build on non-Windows targets with a single `compile_error!`. It never
  worked there.
- `Mft::new` skips records that fail fixup verification instead of failing the whole load, and joins the
  `$MFT` data and bitmap when they span extension records. Records whose update sequence array does not
  cover every sector are rejected, as Windows does, and counted in `corrupt_records()`.
- `Mft::resolve_path` limits paths to the Win32 maximum, counted exactly (32767 UTF-16 units, the volume path
  and the separators included, so about 16k levels) instead of 1024 levels, applies the limit the same with
  and without a cache, and detects parent loops exactly.
- `Journal::match_rename` needs `Reason::RENAME_OLD_NAME` in the `reason_mask`. The history keeps only those
  records, so hard link and reparse point changes no longer push renames out of it.
- `Journal` opens files by id with `FILE_FLAG_OPEN_REPARSE_POINT`, so a reparse point resolves to its own
  path instead of its target's.
- Performance: loading the `$MFT`, full scans and path lookups are much faster; see "Compared with 0.4.7" below.
- `Cargo.toml` has an `include` allowlist, so development files are no longer published. The `windows`
  dependency is only built for Windows targets and no longer enables `Win32_System_Threading`.
- The integration tests need `NTFS_READER_TEST_VOLUME` (a disposable NTFS drive letter) and refuse the
  system drive unless `NTFS_READER_ALLOW_SYSTEM_DRIVE=1`; they used to fall back to the system drive and
  delete its USN journal.

### Removed

- **Breaking:** `Mft::iterate_files` (use `files()`).
- **Breaking:** `Mft::get_record_fs` and `Mft::read_data_fs` (low-level loading steps; use `Mft::new`).
- **Breaking:** `NtfsFile::get_attribute` (use `record_attributes()` and `find`), `get_best_file_name`
  (now `best_name`), `read_data` (now `resident_data()`), `all_file_names`, `NtfsNameKind` and
  `NtfsFileNameEntry` (use `names` or `hard_links`).
- **Breaking:** `NtfsFile::new`, `NtfsFile::is_valid`, `NtfsAttribute::new`, `data`, `len`, `is_empty`,
  `resident_header`, `nonresident_header`, `get_nonresident_data_runs` and `DataRun` are no longer public
  (use `Mft::files()`, `Mft::record()` and the typed accessors).
- **Breaking:** `FileInfoCache`, `HashMapCache` and `VecCache` (use `PathCache` and `DefaultPathCache`).
- **Breaking:** `Journal::get_reason_str` (use the `Display` impl of `Reason`).
- **Breaking:** `NtfsReaderError::Unknown`, `BinReadError`, `ElevationError`, `WindowsError`,
  `CorruptMftRecord` and `CorruptMft`, and the `binread` dependency.
- **Breaking:** `ntfs_reader::test_utils`.
- **Breaking:** the raw on-disk structs (`BootSector`, record and attribute headers, `NtfsFileFlags`,
  `NtfsFileNameFlags` and `NtfsAttributeListEntry`), `aligned_reader`, the `api` constants (`SECTOR_SIZE`,
  `MFT_RECORD`, `ROOT_RECORD`, `FIRST_NORMAL_RECORD`, `FILE_RECORD_SIGNATURE`, `EPOCH_DIFFERENCE`) and
  `api::ntfs_to_unix_time` are no longer public, and `Volume::boot_sector` is removed.

### Fixed

- Journal reads no longer use a completion port, which could leave the kernel writing into a stack buffer
  after `read` had returned. Volume and file handles are also closed on every error path.
- USN record parsing is bounds-checked: the name is read from `FileNameOffset`, and records with an odd
  `RecordLength` or a truncated name no longer misalign or overrun the buffer.
- `match_rename` returns the right old name when a file is renamed more than once (A to B to C).
- `Journal::trim_history(Some(usn))` keeps the entry at `usn` instead of dropping it.
- Boot sectors with clusters larger than 64 KiB or out-of-range fields are decoded correctly or rejected,
  instead of producing wrong offsets or huge allocations.
- Opening a volume without full administrator rights but with backup privileges is no longer rejected up
  front.
- A non-resident `$DATA` or `$BITMAP` size larger than the volume is rejected, and a data run with an
  out-of-range LCN is an error instead of reading from a truncated offset. `$MFT` extents with a gap or a
  repeat in the attribute list are rejected instead of being read shifted.
- An extension record that refers to itself no longer produces duplicate or looping results.
- Files whose `$FILE_NAME` carries the reparse point flag now get a name and path (2,154 more files on the
  test system volume), and so do all their descendants.
- A parent reference to a freed and reused record, or to a record that is not in use, no longer resolves
  through the wrong directory, and a parent cycle no longer costs a full walk for every file below it.
- `FileInfo::path` and `Mft::resolve_path` no longer replace an unpaired UTF-16 surrogate in a name with
  U+FFFD, which produced a path that does not exist. The same applies to stream names, USN record names and
  `match_rename`.
- Timestamps before 1970 were reported as 1970-01-01; they are the real date now. A value beyond what
  `OffsetDateTime` can hold is its latest time instead of the epoch.
- A `$DATA` attribute whose name cannot be read is no longer reported as the default stream.

## [0.4.7] - 2026-09-17

### Added

- `NtfsFile::all_file_names`, with `NtfsNameKind` and `NtfsFileNameEntry`, lists every `$FILE_NAME` of a
  file and tells hard links from DOS 8.3 aliases (#17, #18).
- `list_hardlinks` example.

## [0.4.6] - 2026-08-30

### Added

- `FileInfo::file_attributes`, the raw `FILE_ATTRIBUTE_*` flags.
- `Mft::file_records` and `NtfsFile::is_extension`, `base_reference_number` and `base_record_number` for
  working with extension records.

### Fixed

- Files whose attributes are split across MFT extension records now report the right size, attributes,
  timestamps and names. Before, a large fragmented file could show a size of 0 (#15, #16).

## [0.4.5] - 2026-03-28

### Added

- `Mft::files()`, an iterator over the files in use.

### Changed

- **Breaking:** journal functions return `NtfsReaderResult` like the rest of the crate.
- **Breaking:** `WindowsErrorWrapper` is removed. `NtfsReaderError::WindowsError` wraps
  `windows::core::Error` directly.
- **Breaking:** `Mft::get_record_data` is private.
- `AlignedReader::new` returns an error instead of panicking on a bad alignment.

### Deprecated

- `Mft::iterate_files`, in favor of `files()`.

### Fixed

- Reading large volumes on 32-bit targets returns `AllocationTooLarge` instead of panicking with a
  capacity overflow (#12, #13).
- Path resolution stops at a depth limit instead of looping on a cyclic parent chain.
- USN record parsing checks bounds instead of trusting the record length.

## [0.4.4] - 2025-09-26

### Added

- `NtfsReaderError` variants `CorruptMftRecord`, `InvalidMftRecord`, `CorruptMft` and `InvalidDataRun`.

### Changed

- **Breaking:** `Mft::get_record_fs` and `Mft::read_data_fs` return a `Result`.
- **Breaking:** `JournalOptions::version_range` is removed; the journal always asks for USN record
  versions 2 and 3.
- MFT, attribute and data run parsing is bounds-checked and returns errors instead of panicking on
  corrupt data. Sparse data runs are read as zeros.

### Fixed

- The file record size was computed wrongly when the boot sector stores it as a cluster count (#10).
- `Mft::record_exists` accepted `max_record`, one past the last record.
- The directory flag on file names used the wrong bit, so `is_directory` was wrong for some entries.
- `$MFT` attributes reached through an `$ATTRIBUTE_LIST` are read with checked lengths, so a malformed
  list ends the search instead of crashing it (#11).
- Journal timestamps before the epoch or out of range no longer wrap around.

## [0.4.3] - 2025-09-20

### Added

- `FileId`, `NtfsFile::reference_number` and `NtfsFile::get_file_id`.
- `NtfsReaderError::MissingMftAttribute`.
- Examples (`read_mft`, `read_journal`) and a benchmark.

### Changed

- **Breaking:** `FileId` moved from `journal` to `api`, and can now hold an extended 128-bit id.
- **Breaking:** `WindowsErrorWrapper::from_win32` is now `from_thread`.
- `windows` updated to 0.62.

### Fixed

- `Mft::new` finds the `$MFT` `$DATA` and `$BITMAP` when they are stored in another record listed by an
  `$ATTRIBUTE_LIST`, which happens on large or fragmented volumes.

## [0.4.2] - 2025-02-16

### Added

- Usage examples for the MFT and the journal in the README.

### Changed

- **Breaking:** `VecCache` stores `Vec<Option<PathBuf>>` instead of `Vec<PathBuf>`.
- `thiserror` updated to 2.0 and `windows` to 0.59.

### Fixed

- `VecCache` returned an empty path for entries that were never cached, which truncated the full path of
  files under them (#1).

## [0.4.1] - 2024-07-22

### Fixed

- Converting an NTFS timestamp earlier than the Unix epoch panicked with a subtraction overflow (#6).

## [0.4.0] - 2024-03-31

### Added

- Support for `USN_RECORD_V2` journal records, next to V3.
- `JournalOptions::version_range` to choose which record versions to read.

### Changed

- **Breaking:** `UsnRecord::file_id` and `parent_id` are a `FileId` enum instead of `u128`.
- `windows` updated to 0.54.

### Fixed

- Deleted files and files inside deleted folders got a wrong or empty `UsnRecord::path`. The path is now
  built from the record's own file id, falling back to the parent path plus the record's name (#2, #4).

## [0.3.0] - 2024-01-13

First tagged release. The crate could already read the `$MFT` into memory and read the USN journal.
Earlier history (0.1.0 to 0.2.0, 2022) is not tagged and is not covered here.

[Unreleased]: https://github.com/kikijiki/ntfs-reader/compare/v0.5.1...HEAD
[0.5.1]: https://github.com/kikijiki/ntfs-reader/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/kikijiki/ntfs-reader/compare/v0.4.7...v0.5.0
[0.4.7]: https://github.com/kikijiki/ntfs-reader/compare/v0.4.6...v0.4.7
[0.4.6]: https://github.com/kikijiki/ntfs-reader/compare/v0.4.5...v0.4.6
[0.4.5]: https://github.com/kikijiki/ntfs-reader/compare/v0.4.4...v0.4.5
[0.4.4]: https://github.com/kikijiki/ntfs-reader/compare/v0.4.3...v0.4.4
[0.4.3]: https://github.com/kikijiki/ntfs-reader/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/kikijiki/ntfs-reader/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/kikijiki/ntfs-reader/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/kikijiki/ntfs-reader/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/kikijiki/ntfs-reader/releases/tag/v0.3.0
