// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! [`Mft`]: the whole `$MFT` of a volume, loaded into memory.

use std::fmt;
use std::io::{Read, Seek, SeekFrom};
use std::mem::size_of;

use crate::{
    aligned_reader::open_volume,
    api::*,
    attribute::NtfsAttribute,
    data_run::DataRun,
    errors::{NtfsReaderError, NtfsReaderResult},
    file::{NtfsFile, Record},
    volume::Volume,
};

/// The `$MFT` of a volume, read into memory once with every record's update sequence fixups
/// applied. Immutable after that: [`NtfsFile`]s, their attributes and names all borrow from it.
///
/// Loading reads the whole `$MFT` (about 1 KiB per file on the volume), so [`Mft::new`] costs
/// seconds and memory on a large volume; see [`Self::size_in_memory`].
pub struct Mft {
    volume: Volume,
    data: Vec<u8>,
    bitmap: Vec<u8>,
    record_count: u64,
    extension_records: Vec<(u64, u64)>,
    freed_extension_records: Vec<(u64, u64)>,
    corrupt_records: u64,
}

/// The low 48 bits of a file reference: the record number.
pub(crate) const RECORD_NUMBER_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// What a record is when its in-use flag and `$BITMAP` bit agree: the one place that decides
/// whether a record, or a reference to it, is live or freed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Liveness {
    /// In use and allocated.
    Live,
    /// Not in use and not allocated: what a delete leaves.
    Freed,
}

impl Liveness {
    /// `None` when the flag and the bitmap disagree: a record caught between the two writes, or
    /// corrupt. Neither state is trusted then.
    pub(crate) fn of(record: &Record, allocated: bool) -> Option<Self> {
        match (record.is_used(), allocated) {
            (true, true) => Some(Self::Live),
            (false, false) => Some(Self::Freed),
            _ => None,
        }
    }

    /// Whether `reference` names `record` (the record at its number), and in which state. A live
    /// record is named by its own reference; freeing adds one to the sequence number and leaves
    /// other references unchanged, so a freed record is named one sequence below its own. A
    /// record in use with that higher sequence is a reuse; any other sequence is another
    /// incarnation: `None`.
    ///
    /// The sequence wraps at 0xFFFF. Measured on Windows 11 (build 26200): a record freed at
    /// 0xFFFF stores 0, and the next file to use it gets 1, since NTFS skips 0 only for live
    /// records. A stored 1 is accepted for 0xFFFF as well; no live record has sequence 0, so no
    /// other reference can claim it.
    ///
    /// Only known failure: the 16-bit sequence can repeat after about 65,535 reuses of one
    /// record number, wrongly accepting an old reference for a different incarnation. Not
    /// measured, and not fixable: the field holds no more bits.
    pub(crate) fn of_reference(reference: u64, record: &Record, allocated: bool) -> Option<Self> {
        let state = Self::of(record, allocated)?;
        if record.number() & RECORD_NUMBER_MASK != reference & RECORD_NUMBER_MASK {
            return None;
        }
        let sequence = (reference >> 48) as u16;
        let stored = record.sequence();
        let named = match state {
            Self::Live => stored == sequence,
            Self::Freed => {
                stored == sequence.wrapping_add(1) || (sequence == 0xFFFF && stored == 1)
            }
        };
        named.then_some(state)
    }

    /// The reference a freed record was named by while live: one sequence below its own, what
    /// [`Self::of_reference`] accepts. A record freed at the wrap (stored sequence 0, as
    /// measured, or 1) is read as live at 0xFFFF: NTFS never gives a live record sequence 0.
    pub(crate) fn live_reference(freed: &Record) -> u64 {
        let sequence = match freed.sequence() {
            0 | 1 => 0xFFFF,
            stored => stored - 1,
        };
        (u64::from(sequence) << 48) | (freed.number() & RECORD_NUMBER_MASK)
    }
}

impl fmt::Debug for Mft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mft")
            .field("volume", &self.volume)
            .field("record_count", &self.record_count)
            .field("corrupt_records", &self.corrupt_records)
            .field("size_in_memory", &self.size_in_memory())
            .finish_non_exhaustive()
    }
}

impl Mft {
    /// Reads the `$MFT` of `volume` into memory.
    ///
    /// Needs the same elevated access as [`Volume::new`]; the volume is reopened by its path. A
    /// record that fails its update sequence check is skipped and counted in
    /// [`Self::corrupt_records`] instead of failing the load.
    ///
    /// Reads through a normal volume handle, so a change made moments ago (a file created or
    /// deleted, data written) may not be on disk yet: a just-deleted file can look live, or, if
    /// its in-use flag and `$BITMAP` bit were written at different times, appear in neither
    /// [`Mft::files`] nor [`Mft::deleted_files`]. Flush the volume first
    /// (`OpenOptions::new().read(true).write(true).open(r"\\.\C:")?.sync_all()?`, i.e.
    /// `FlushFileBuffers`, needs the same elevation; or `Write-VolumeCache C` in PowerShell).
    pub fn new(volume: Volume) -> NtfsReaderResult<Self> {
        let mut reader = open_volume(volume.path())?;

        let mft_record = Self::read_record_fs(
            &mut reader,
            volume.file_record_size(),
            volume.mft_position(),
        )?;

        let data = Self::read_data_fs(&volume, &mut reader, &mft_record, NtfsAttributeType::Data)?
            .ok_or(NtfsReaderError::MissingMftAttribute { attribute: "Data" })?;
        let bitmap =
            Self::read_data_fs(&volume, &mut reader, &mft_record, NtfsAttributeType::Bitmap)?
                .ok_or(NtfsReaderError::MissingMftAttribute {
                    attribute: "Bitmap",
                })?;

        Self::from_parts(volume, data, bitmap)
    }

    /// Builds the in-memory MFT from the raw `$MFT` data and bitmap in one pass: applies every
    /// record's fixups and indexes extension records, live and freed ones separately. A record
    /// whose update sequence does not match (torn write, BAAD record, one changed under a live
    /// volume) is invalidated and skipped rather than failing the whole load; see
    /// [`Self::corrupt_records`].
    pub(crate) fn from_parts(
        volume: Volume,
        mut data: Vec<u8>,
        bitmap: Vec<u8>,
    ) -> NtfsReaderResult<Self> {
        let file_record_size = volume.file_record_size();
        let record_count = data.len() as u64 / file_record_size;
        let mut corrupt_records = 0u64;
        let mut extension_records = Vec::new();
        let mut freed_extension_records = Vec::new();

        for number in 0..record_count {
            let start = number * file_record_size;
            let end = start + file_record_size;
            let (start, end) = (start as usize, end as usize);
            let record = &mut data[start..end];
            let allocated = Self::bitmap_bit_set(&bitmap, number);
            if Self::fixup_record(number, record).is_err() {
                record[..FILE_RECORD_SIGNATURE.len()].fill(0);
                corrupt_records += u64::from(allocated);
                continue;
            }

            // A successful fixup only means the update-sequence array matched; `Record::new`
            // still checks the rest of the header (a real "FILE" signature, bounds on
            // `used_size` and `attributes_offset`) before trusting it for anything. A free slot
            // with no valid header is just an empty slot.
            let Some(file) = Record::new(number, record) else {
                corrupt_records += u64::from(allocated);
                continue;
            };
            let Some(liveness) = Liveness::of(&file, allocated) else {
                continue;
            };
            if let Some(base) = file.base_number() {
                // A record whose base reference points at itself is corrupt, not its own
                // extension; indexing it would make NtfsFile::records yield it twice and
                // duplicate its attributes.
                if base != number {
                    match liveness {
                        Liveness::Live => extension_records.push((base, number)),
                        Liveness::Freed => freed_extension_records.push((base, number)),
                    }
                }
            }
        }
        extension_records.sort_unstable();
        freed_extension_records.sort_unstable();

        Ok(Mft {
            volume,
            data,
            bitmap,
            record_count,
            extension_records,
            freed_extension_records,
            corrupt_records,
        })
    }

    /// Number of records [`Self::new`] skipped as untrustworthy: slots the `$BITMAP` marks
    /// allocated whose update sequence check failed (torn write, BAAD record, a live-volume race,
    /// or an incomplete array) or whose header is not a valid `FILE` record. Free slots are never
    /// counted, whatever they hold.
    pub fn corrupt_records(&self) -> u64 {
        self.corrupt_records
    }

    /// The volume this MFT was loaded from.
    pub fn volume(&self) -> &Volume {
        &self.volume
    }

    /// Number of record slots in the `$MFT` (its size divided by the record size), free and
    /// corrupt ones included: one past the highest valid record number. [`Self::record`] and
    /// [`Self::is_allocated`] return `None`/`false` at `record_count()` and above.
    pub fn record_count(&self) -> u64 {
        self.record_count
    }

    /// Size in bytes of what this `Mft` holds in memory: the records (the `$MFT` `$DATA` stream),
    /// the `$MFT` `$BITMAP`, and the two extension-record indexes (live and freed, 16 bytes an
    /// entry). Excludes the [`Volume`] and its few fixed fields. Path caches and
    /// [`ClusterBitmap`](crate::ClusterBitmap)s count separately.
    pub fn size_in_memory(&self) -> usize {
        let entries = self.extension_records.len() + self.freed_extension_records.len();
        self.data.len() + self.bitmap.len() + entries * std::mem::size_of::<(u64, u64)>()
    }

    /// Whether the `$MFT` `$BITMAP` marks record `number` allocated; does not look at the record
    /// itself. A record skipped as corrupt while loading (bad fixup or header) keeps its bitmap
    /// bit, so [`Self::record`] can still return `None` where this returns `true`. `false` at or
    /// above [`Self::record_count`].
    pub fn is_allocated(&self, number: u64) -> bool {
        number < self.record_count && Self::bitmap_bit_set(&self.bitmap, number)
    }

    /// Whether bit `number` is set in a `$BITMAP` buffer. Free function of `bitmap` alone (not
    /// `&self`) so [`Self::from_parts`] can use it while still building the `Mft`, before
    /// `self.bitmap`/`self.record_count` exist. [`Self::is_allocated`] is the bounds-checked,
    /// public version once a `Mft` exists.
    fn bitmap_bit_set(bitmap: &[u8], number: u64) -> bool {
        let bitmap_idx = (number / 8) as usize;
        let bitmap_off = (number % 8) as u8;
        bitmap
            .get(bitmap_idx)
            .is_some_and(|bit| bit & (1u8 << bitmap_off) != 0)
    }

    /// Every file on the volume: each in-use base record, in record number order.
    ///
    /// Extension records are never yielded (reach them via [`NtfsFile::records`] or the file's
    /// own accessors), and neither are the 24 records NTFS reserves for itself (0 to 23: `$MFT`,
    /// `$LogFile`, `$Volume`, the root directory at 5, etc.), so **the root directory is not
    /// yielded**; [`Self::record`] still returns any of them by number. Deleted files are not
    /// here, see [`Self::deleted_files`]. A delete-pending file (deleted while held open,
    /// renamed under `$Extend\$Deleted`) stays in use until the last handle closes, so it
    /// appears here with [`NtfsFile::is_deleted`] `false`.
    pub fn files<'a>(&'a self) -> impl Iterator<Item = NtfsFile<'a>> + use<'a> {
        (FIRST_NORMAL_RECORD..self.record_count)
            .filter(|&n| self.is_allocated(n))
            .filter_map(|n| self.record(n))
            .filter(|f| f.is_used() && !f.is_extension())
    }

    /// The files that were deleted: each freed base record NTFS has not reused yet, in record
    /// number order, from record 24 up.
    ///
    /// Yielded when the header is valid, it is a base record not in use with its `$BITMAP` bit
    /// clear, and it still has a `$STANDARD_INFORMATION` or `$FILE_NAME`, in itself or a freed
    /// extension record. Needs no name of its own (the base of a file with an `$ATTRIBUTE_LIST`
    /// has none) and no data. Freed extension records and slots that never held a file are never
    /// yielded.
    ///
    /// A delete-pending file (deleted while held open, renamed under `$Extend\$Deleted`) is still
    /// in use, so it is in [`Self::files`] instead, until the last handle closes. A record whose
    /// in-use flag and `$BITMAP` bit disagree is in neither.
    ///
    /// [`NtfsFile`] accessors work on these as on a live file: [`NtfsFile::is_deleted`] is
    /// `true`, [`NtfsFile::is_used`] `false`. The converse does not hold: `is_deleted` is true
    /// for any freed record with a freed base, and this list also needs record 24 or higher with
    /// `$STANDARD_INFORMATION` or `$FILE_NAME` present. Values are what the record still holds,
    /// not always what the file had: see [`NtfsFile::data_streams`]. A freed record is usually
    /// reused by the next file created, so a busy volume loses deleted files fast. The `Mft` is a
    /// snapshot: files deleted after loading are not in it, and there is no deletion time, since
    /// the record keeps none.
    pub fn deleted_files<'a>(&'a self) -> impl Iterator<Item = NtfsFile<'a>> + use<'a> {
        (FIRST_NORMAL_RECORD..self.record_count)
            .filter(|&n| !self.is_allocated(n))
            .filter_map(|n| self.record(n))
            .filter(|f| {
                !f.is_used()
                    && !f.is_extension()
                    && f.attributes().any(|attribute| {
                        matches!(
                            attribute.attribute_type(),
                            Some(
                                NtfsAttributeType::StandardInformation
                                    | NtfsAttributeType::FileName
                            )
                        )
                    })
            })
    }

    /// The record a file id names, live or deleted, or `None` if the id is not a 48-bit record
    /// number plus 16-bit sequence number (see [`FileId::as_reference`]), the number is out of
    /// range, or the record is not the one the id was made for.
    ///
    /// An id names a live record when it is in use, allocated, and has the id's sequence number;
    /// a freed one when it is not in use, not allocated, and its sequence number is the id's
    /// plus one, what freeing does. A record freed and reused, or freed twice, is not named by
    /// the old id: the new file has its own. So a `FILE_DELETE` journal record's id finds the
    /// file it deleted, as long as the record has not been reused since. The sequence wraps at
    /// 0xFFFF (freed to 0, next live use 1), and after about 65,535 reuses of a record the wrap
    /// can make an id name the wrong incarnation.
    ///
    /// The `Mft` is a snapshot: a file deleted after loading is still live here, one created
    /// after loading is not here at all. Like [`Self::record`], the record can be an extension
    /// record; journal ids name base records.
    pub fn record_by_id(&self, id: FileId) -> Option<NtfsFile<'_>> {
        let reference = id.as_reference()?;
        let file = self.record(reference & RECORD_NUMBER_MASK)?;
        Liveness::of_reference(reference, &file.record, self.is_allocated(file.number()))?;
        Some(file)
    }

    /// Extension records indexed for the file whose base record is `base_number`, live or freed,
    /// as `(base, extension)` record number pairs.
    pub(crate) fn extension_records(&self, base_number: u64, liveness: Liveness) -> &[(u64, u64)] {
        let index = match liveness {
            Liveness::Live => &self.extension_records,
            Liveness::Freed => &self.freed_extension_records,
        };
        let start = index.partition_point(|(base, _)| *base < base_number);
        let end = index.partition_point(|(base, _)| *base <= base_number);
        &index[start..end]
    }

    /// Whether `record` is live or freed in this `Mft`, or neither: see
    /// [`Liveness::of`].
    pub(crate) fn liveness(&self, record: &Record) -> Option<Liveness> {
        Liveness::of(record, self.is_allocated(record.number()))
    }

    /// Whether `reference` names `record`, and in which state: see
    /// [`Liveness::of_reference`].
    pub(crate) fn reference_liveness(&self, reference: u64, record: &Record) -> Option<Liveness> {
        Liveness::of_reference(reference, record, self.is_allocated(record.number()))
    }

    fn record_data(&self, number: u64) -> &[u8] {
        let start = number * self.volume.file_record_size();
        let end = start + self.volume.file_record_size();
        &self.data[start as usize..end as usize]
    }

    /// The record `number`, or `None` if it is at or above [`Self::record_count`] or does not hold
    /// a valid record (never used, or skipped as corrupt). Unlike [`Self::files`], returns any
    /// valid record: free ones (the deleted files, see [`Self::deleted_files`]), extension
    /// records, and the reserved metadata records; check [`NtfsFile::is_used`] and
    /// [`NtfsFile::is_extension`] where that matters. A record's accessors work whether it is live
    /// or freed; [`Self::record_by_id`] finds one by file id.
    pub fn record(&self, number: u64) -> Option<NtfsFile<'_>> {
        if number >= self.record_count {
            return None;
        }
        NtfsFile::new(self, number, self.record_data(number))
    }

    pub(crate) fn read_record_fs<R>(
        fs: &mut R,
        file_record_size: u64,
        position: u64,
    ) -> NtfsReaderResult<Vec<u8>>
    where
        R: Seek + Read,
    {
        let mut data = vec![0; file_record_size as usize];
        fs.seek(SeekFrom::Start(position))?;
        fs.read_exact(&mut data)?;

        if !Record::is_valid(&data) {
            return Err(NtfsReaderError::InvalidMftRecord { position });
        }
        Self::fixup_record(0, &mut data)?;
        Ok(data)
    }

    /// Reads an attribute's value from `$MFT`'s own record 0, following the `$ATTRIBUTE_LIST`
    /// into other records when the value spans extension records. Matches unnamed attributes
    /// only. Not for arbitrary records: `record`'s own unnamed `$DATA` extent doubles as the map
    /// for locating extension records (see `locate_record` below), which only holds for `$MFT`'s
    /// bootstrap record.
    pub(crate) fn read_data_fs<R>(
        volume: &Volume,
        reader: &mut R,
        record: &[u8],
        attribute_type: NtfsAttributeType,
    ) -> NtfsReaderResult<Option<Vec<u8>>>
    where
        R: Seek + Read,
    {
        let file = Record::new(0, record).ok_or(NtfsReaderError::InvalidMftRecord {
            position: volume.mft_position(),
        })?;

        fn is_unnamed_of(attribute: &NtfsAttribute, attribute_type: NtfsAttributeType) -> bool {
            attribute.attribute_type() == Some(attribute_type) && { attribute.header.name_length }
                == 0
        }

        // `$MFT`'s own unnamed `$DATA`, VCN-0 extent: on a real volume always in record 0 (the
        // loader could not bootstrap otherwise). Its runs are the only way to locate any other
        // MFT record by number ("record N" is byte N * file_record_size of `$MFT`'s own `$DATA`),
        // used below regardless of whether the attribute requested here is `$DATA` or `$BITMAP`
        // (issue #11 was `$BITMAP` behind an attribute list).
        let mft_data_runs: Vec<DataRun> = file
            .attributes()
            .find(|attribute| is_unnamed_of(attribute, NtfsAttributeType::Data))
            .filter(|attribute| {
                attribute
                    .nonresident_header()
                    .is_some_and(|header| { header.lowest_vcn } == 0)
            })
            .map(|attribute| attribute.nonresident_extent_runs(volume))
            .transpose()?
            .unwrap_or_default();

        // Translates an MFT record number to a byte position by walking `runs`. `None` if it
        // is not covered: such a record cannot be located before the full `$DATA` is read, so its
        // list entry is skipped rather than guessed at (the old `mft_position + n *
        // file_record_size` formula silently read the wrong bytes once `$DATA` fragmented).
        fn locate_record(runs: &[DataRun], record_size: u64, number: u64) -> Option<u64> {
            let target = number.checked_mul(record_size)?;
            let mut consumed = 0u64;
            for run in runs {
                let length = match run {
                    DataRun::Data { length, .. } | DataRun::Sparse { length } => *length,
                };
                let end = consumed.checked_add(length)?;
                if target < end {
                    let within = target - consumed;
                    return match run {
                        DataRun::Data { offset, .. } => offset.checked_add(within),
                        DataRun::Sparse { .. } => None,
                    };
                }
                consumed = end;
            }
            None
        }

        // Follow the attribute list, if any, for extension records holding further extents. A
        // target that cannot be located through `mft_data_runs`, or whose base is not record 0, is
        // skipped rather than trusted. Bytes are kept (with the entry's declared VCN) so
        // attributes can be borrowed from them below, outliving this loop.
        let mut extension_records: Vec<(u64, u64, Vec<u8>)> = Vec::new();
        let list = file
            .attributes()
            .find(|attribute| attribute.attribute_type() == Some(NtfsAttributeType::AttributeList));
        if let Some(list) = list {
            let list = Self::read_attribute_data(reader, &list, volume)?;

            for entry in attribute_list_entries(&list) {
                if { entry.type_id } != attribute_type as u32 || { entry.name_length } != 0 {
                    continue;
                }
                let number = entry.number();
                if number == 0 {
                    // Record 0 was already scanned directly, below.
                    continue;
                }
                let Some(position) =
                    locate_record(&mft_data_runs, volume.file_record_size(), number)
                else {
                    continue;
                };
                let Ok(target) = Self::read_record_fs(reader, volume.file_record_size(), position)
                else {
                    continue;
                };
                let Some(target_file) = Record::new(number, &target) else {
                    continue;
                };
                if target_file.base_number() != Some(0) {
                    continue;
                }
                // The record here may have been freed and reused since the list was written;
                // trust it only if its current sequence matches what the entry expects
                // (NtfsFile::records makes the same check for a base record's own reference).
                if target_file.reference() != { entry.base_file_reference } {
                    continue;
                }
                extension_records.push((number, entry.starting_vcn, target));
            }
        }

        // Extents of the requested type: record 0's own, plus every reached
        // extension record's.
        let mut extents: Vec<NtfsAttribute> = file
            .attributes()
            .filter(|attribute| is_unnamed_of(attribute, attribute_type))
            .collect();
        for (number, starting_vcn, target) in &extension_records {
            // `target` was validated above.
            let Some(record) = Record::new(*number, target) else {
                continue;
            };
            extents.extend(record.attributes().filter(|attribute| {
                is_unnamed_of(attribute, attribute_type) && vcn_matches(attribute, *starting_vcn)
            }));
        }

        let Some(first) = extents.first() else {
            return Ok(None);
        };

        if first.is_resident() {
            let data = first.resident().ok_or(NtfsReaderError::InvalidDataRun {
                details: "resident attribute missing value",
            })?;
            return Ok(Some(data.to_vec()));
        }

        // Non-resident: join every extent in VCN order, size from the VCN-0
        // extent (later extents carry `data_size == 0` on disk).
        extents.sort_by_key(|attribute| {
            attribute
                .nonresident_header()
                .map_or(i64::MAX, |header| header.lowest_vcn)
        });

        let size = extents
            .first()
            .and_then(|attribute| attribute.nonresident_header())
            .filter(|header| { header.lowest_vcn } == 0)
            .map(|header| header.data_size)
            .ok_or(NtfsReaderError::InvalidDataRun {
                details: "missing VCN-0 extent",
            })?;

        // The extents must tile the value: each starts at the VCN the ones before it end at. A
        // missing or repeated extent (a stale or duplicated list entry, or a corrupt VCN) would
        // silently shift everything after it.
        let mut runs = Vec::new();
        let mut total = 0u64;
        for extent in &extents {
            let extent_runs = extent.nonresident_extent_runs(volume)?;
            let start = extent
                .nonresident_header()
                .and_then(|header| u64::try_from(header.lowest_vcn).ok())
                .and_then(|vcn| vcn.checked_mul(volume.cluster_size()));
            if start != Some(total) {
                return Err(NtfsReaderError::InvalidDataRun {
                    details: "extents do not follow each other in VCN order",
                });
            }
            for run in &extent_runs {
                let (DataRun::Data { length, .. } | DataRun::Sparse { length }) = run;
                total = total
                    .checked_add(*length)
                    .ok_or(NtfsReaderError::InvalidDataRun {
                        details: "total run length overflow",
                    })?;
            }
            runs.extend(extent_runs);
        }
        if total < size {
            return Err(NtfsReaderError::InvalidDataRun {
                details: "data runs shorter than declared size",
            });
        }

        Self::read_runs(reader, volume, size, &runs).map(Some)
    }

    fn read_attribute_data<R>(
        reader: &mut R,
        att: &NtfsAttribute,
        volume: &Volume,
    ) -> NtfsReaderResult<Vec<u8>>
    where
        R: Seek + Read,
    {
        if att.is_resident() {
            let data = att.resident().ok_or(NtfsReaderError::InvalidDataRun {
                details: "resident attribute missing value",
            })?;
            Ok(data.to_vec())
        } else {
            let (size, runs) = att.nonresident_data_runs(volume)?;
            Self::read_runs(reader, volume, size, &runs)
        }
    }

    /// Reads `size` bytes out of `runs` (already checked against `size` by the caller),
    /// sequentially: sparse runs contribute zeroes.
    ///
    /// `size` comes straight from an on-disk attribute header, checked so far only against the
    /// runs' own declared lengths, which a corrupt or hostile volume can inflate just as cheaply
    /// (found by fuzzing: a few dozen bytes claimed a 2 GiB `$DATA`). `try_reserve` below turns a
    /// failed allocation into an error rather than aborting, but a large-but-satisfiable one is
    /// still wasted work for a size no genuine stream could reach on its claimed volume, so this
    /// bounds `size` against the volume's own size first. A `volume_size` of 0 (every synthetic
    /// `Volume` in this crate's tests) skips the bound instead of rejecting everything; a real
    /// `Volume::new` always fills it in from the boot sector.
    fn read_runs<R>(
        reader: &mut R,
        volume: &Volume,
        size: u64,
        runs: &[DataRun],
    ) -> NtfsReaderResult<Vec<u8>>
    where
        R: Seek + Read,
    {
        if volume.volume_size() != 0 && size > volume.volume_size() {
            return Err(NtfsReaderError::InvalidDataRun {
                details: "declared size exceeds the volume's own size",
            });
        }

        let total_size =
            usize::try_from(size).map_err(|_| NtfsReaderError::AllocationTooLarge { size })?;

        let mut data = Vec::new();
        data.try_reserve(total_size)
            .map_err(|_| NtfsReaderError::AllocationTooLarge { size })?;
        let mut copied = 0u64;

        for run in runs.iter() {
            if copied >= size {
                break;
            }

            let buf_size = match run {
                DataRun::Data { offset, length } => {
                    let buf_size = u64::min(*length, size - copied);
                    let start = data.len();
                    data.resize(start + buf_size as usize, 0u8);

                    reader.seek(SeekFrom::Start(*offset))?;
                    reader.read_exact(&mut data[start..])?;
                    buf_size
                }
                DataRun::Sparse { length } => {
                    let buf_size = u64::min(*length, size - copied);
                    data.resize(data.len() + buf_size as usize, 0);
                    buf_size
                }
            };
            copied += buf_size;
        }

        Ok(data)
    }

    fn fixup_record(record_number: u64, data: &mut [u8]) -> NtfsReaderResult<()> {
        if data.len() < core::mem::size_of::<NtfsFileRecordHeader>() {
            return Err(NtfsReaderError::MftRecordFixupFailed {
                number: record_number,
            });
        }
        // SAFETY: the length check above covers a whole header, and every bit pattern is a
        // valid value of that packed struct of plain integers.
        let header =
            unsafe { core::ptr::read_unaligned(data.as_ptr() as *const NtfsFileRecordHeader) };

        let usn_start = header.update_sequence_offset as usize;
        if usn_start + 2 > data.len() {
            return Err(NtfsReaderError::MftRecordFixupFailed {
                number: record_number,
            });
        }
        let usa_start = usn_start + 2;
        let usa_end =
            usn_start.saturating_add((header.update_sequence_length as usize).saturating_mul(2));
        if usa_end > data.len() {
            return Err(NtfsReaderError::MftRecordFixupFailed {
                number: record_number,
            });
        }
        // The array must hold one saved value per sector, or the last sectors' ends would stay
        // unrestored (Windows rejects such a record). A zero length is not a record at all (a
        // free or zeroed slot): nothing to fix up, and `Record::new` rejects it.
        let sectors = data.len() / SECTOR_SIZE;
        if header.update_sequence_length != 0
            && header.update_sequence_length as usize != sectors + 1
        {
            return Err(NtfsReaderError::MftRecordFixupFailed {
                number: record_number,
            });
        }

        let usn0 = data[usn_start];
        let usn1 = data[usn_start + 1];

        let mut sector_off = SECTOR_SIZE - 2;
        for usa_off in (usa_start..usa_end).step_by(2) {
            if sector_off + 2 > data.len() {
                break;
            }

            let mut usa = [0u8; 2];
            usa.copy_from_slice(&data[usa_off..usa_off + 2]);

            let d0 = data[sector_off];
            let d1 = data[sector_off + 1];
            if d0 != usn0 || d1 != usn1 {
                return Err(NtfsReaderError::MftRecordFixupFailed {
                    number: record_number,
                });
            }

            data[sector_off..sector_off + 2].copy_from_slice(&usa);
            sector_off += SECTOR_SIZE;
        }
        Ok(())
    }
}

fn attribute_list_entries(data: &[u8]) -> impl Iterator<Item = &NtfsAttributeListEntry> {
    let mut offset = 0usize;
    std::iter::from_fn(move || {
        let rest = data.get(offset..)?;
        if rest.len() < size_of::<NtfsAttributeListEntry>() {
            return None;
        }
        // SAFETY: `rest` holds a whole entry (checked above), and the entry is a packed struct of
        // plain integers (alignment 1), so the reference is aligned and any bytes are valid.
        let entry = unsafe { &*(rest.as_ptr() as *const NtfsAttributeListEntry) };
        let length = entry.length as usize;
        if length < size_of::<NtfsAttributeListEntry>() || length > rest.len() {
            return None;
        }
        offset = (offset + length).next_multiple_of(8);
        Some(entry)
    })
}

/// Whether `attribute` is the extent an `$ATTRIBUTE_LIST` entry with
/// `starting_vcn` describes: for a non-resident attribute its `lowest_vcn`
/// must match; a resident attribute has no VCN and always matches.
fn vcn_matches(attribute: &NtfsAttribute, starting_vcn: u64) -> bool {
    match attribute.nonresident_header() {
        Some(header) => u64::try_from(header.lowest_vcn) == Ok(starting_vcn),
        None => true,
    }
}

/// Synthetic MFT record fixtures. `pub` (not `pub(crate)`) only under `internals`, so
/// `benches/*_synthetic.rs` can build a synthetic `Mft` the same way the unit tests do, without a
/// second copy of the low-level record-building code; see the crate's `internals` feature doc.
/// Lives at `src/test_records.rs`, not `src/mft/`, so that directory does not exist just to hold
/// it.
#[cfg(any(test, feature = "internals"))]
#[doc(hidden)]
#[path = "test_records.rs"]
pub mod test_records;

#[cfg(test)]
#[path = "tests/mft/mod.rs"]
mod tests;
