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

/// The `$MFT` of a volume, read into memory once with every record's update
/// sequence fixups applied. After that it is immutable and everything borrows
/// from it: [`NtfsFile`]s, their attributes and names.
///
/// Loading reads the whole `$MFT` (about 1 KiB per file on the volume), so
/// [`Mft::new`] takes seconds and memory on a large volume; see
/// [`Self::size_in_memory`].
pub struct Mft {
    volume: Volume,
    data: Vec<u8>,
    bitmap: Vec<u8>,
    record_count: u64,
    extension_records: Vec<(u64, u64)>,
    corrupt_records: u64,
}

impl fmt::Debug for Mft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mft")
            .field("volume", &self.volume)
            .field("record_count", &self.record_count)
            .field("corrupt_records", &self.corrupt_records)
            .field("size_in_memory", &self.data.len())
            .finish_non_exhaustive()
    }
}

impl Mft {
    /// Reads the `$MFT` of `volume` into memory.
    ///
    /// Needs the same elevated access as [`Volume::new`]; the volume is
    /// reopened by its path. A record that fails its update sequence check is
    /// skipped and counted in [`Self::corrupt_records`] instead of failing
    /// the load.
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

    /// Builds the in-memory MFT from the raw `$MFT` data and bitmap: applies
    /// every record's fixups and indexes extension records, in one pass over
    /// `data`. A record whose update sequence doesn't match (torn write, BAAD
    /// record, one that changed under a live volume) is invalidated and
    /// skipped rather than failing the whole load; see
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

            // A successful fixup only means the update-sequence array
            // matched; `Record::new` still checks the rest of the header
            // (a real "FILE" signature, bounds on `used_size` and
            // `attributes_offset`) before it can be trusted for anything.
            if !allocated {
                continue;
            }
            let Some(file) = Record::new(number, record) else {
                corrupt_records += 1;
                continue;
            };
            if !file.is_used() {
                continue;
            }
            if let Some(base) = file.base_number() {
                // A record whose base reference points at itself is
                // corrupt, not its own extension; indexing it would make
                // NtfsFile::records yield it twice and duplicate its attributes.
                if base != number {
                    extension_records.push((base, number));
                }
            }
        }
        extension_records.sort_unstable();

        Ok(Mft {
            volume,
            data,
            bitmap,
            record_count,
            extension_records,
            corrupt_records,
        })
    }

    /// Number of records [`Self::new`] skipped because they could not be
    /// trusted: slots the `$BITMAP` marks as allocated whose update sequence
    /// check failed (torn write, BAAD record, a live-volume race, or not the
    /// complete array Windows requires) or whose header is not a valid `FILE`
    /// record. Free slots are not counted, whatever they hold.
    pub fn corrupt_records(&self) -> u64 {
        self.corrupt_records
    }

    /// The volume this MFT was loaded from.
    pub fn volume(&self) -> &Volume {
        &self.volume
    }

    /// The number of record slots in the `$MFT` (its size divided by the
    /// record size), free and corrupt ones included. So it is one past the
    /// highest valid record number: [`Self::record`] and
    /// [`Self::is_allocated`] return `None` and `false` for `record_count()`
    /// and above.
    pub fn record_count(&self) -> u64 {
        self.record_count
    }

    /// Size in bytes of the `$MFT` `$DATA` stream held in memory.
    pub fn size_in_memory(&self) -> usize {
        self.data.len()
    }

    /// Whether the `$MFT` `$BITMAP` marks record `number` as allocated, and
    /// nothing more: it does not look at the record itself. A record skipped
    /// as corrupt while loading (bad fixup or header) keeps its bitmap bit,
    /// so [`Self::record`] can return `None` for a number this returns `true`
    /// for. `false` for `number` at or above [`Self::record_count`].
    pub fn is_allocated(&self, number: u64) -> bool {
        number < self.record_count && Self::bitmap_bit_set(&self.bitmap, number)
    }

    /// Whether bit `number` is set in a `$BITMAP` buffer. A free function of
    /// `bitmap` alone (not `&self`) so [`Self::from_parts`] can use it while
    /// still building the `Mft`, before `self.bitmap`/`self.record_count`
    /// exist; [`Self::is_allocated`] is the public, bounds-checked version
    /// once a `Mft` exists.
    fn bitmap_bit_set(bitmap: &[u8], number: u64) -> bool {
        let bitmap_idx = (number / 8) as usize;
        let bitmap_off = (number % 8) as u8;
        bitmap
            .get(bitmap_idx)
            .is_some_and(|bit| bit & (1u8 << bitmap_off) != 0)
    }

    /// Every file on the volume: each in-use base record, in record number
    /// order.
    ///
    /// Extension records are not files and are never yielded (reach them with
    /// [`NtfsFile::records`], or through the file's own accessors). The 24
    /// records NTFS reserves for its own use (records 0 to 23: `$MFT`,
    /// `$LogFile`, `$Volume`, the root directory at record 5, and so on) are
    /// skipped too, so **the root directory is not yielded**. [`Self::record`]
    /// still returns any of them by number.
    pub fn files<'a>(&'a self) -> impl Iterator<Item = NtfsFile<'a>> + use<'a> {
        (FIRST_NORMAL_RECORD..self.record_count)
            .filter(|&n| self.is_allocated(n))
            .filter_map(|n| self.record(n))
            .filter(|f| f.is_used() && !f.is_extension())
    }

    /// The record numbers of the live extension records indexed for the file
    /// whose base record is `base_number`.
    pub(crate) fn extension_record_numbers(
        &self,
        base_number: u64,
    ) -> impl Iterator<Item = u64> + '_ {
        let start = self
            .extension_records
            .partition_point(|(base, _)| *base < base_number);
        let end = self
            .extension_records
            .partition_point(|(base, _)| *base <= base_number);
        self.extension_records[start..end]
            .iter()
            .map(|(_, extension)| *extension)
    }

    fn record_data(&self, number: u64) -> &[u8] {
        let start = number * self.volume.file_record_size();
        let end = start + self.volume.file_record_size();
        &self.data[start as usize..end as usize]
    }

    /// The record `number`, or `None` if it is at or above [`Self::record_count`]
    /// or does not hold a valid record (never used, or skipped as corrupt).
    /// Unlike [`Self::files`] this returns any valid record: free ones,
    /// extension records and the reserved metadata records, so check
    /// [`NtfsFile::is_used`] and [`NtfsFile::is_extension`] where they matter.
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

    /// Reads an attribute's value from `$MFT`'s own record 0, following the
    /// `$ATTRIBUTE_LIST` into other records when the value is split across
    /// extension records. Matches unnamed attributes only. Not for
    /// arbitrary records: `record`'s own unnamed `$DATA` extent doubles as
    /// the map used to locate any extension record (see `locate_record`
    /// below), which only holds for `$MFT`'s own bootstrap record.
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

        // `$MFT`'s own unnamed `$DATA`, VCN-0 extent: on a real volume this
        // is always in record 0 (the loader could not bootstrap otherwise).
        // Its runs are the only way to locate any other MFT record by
        // number - that is what "record N" means, byte N * file_record_size
        // of $MFT's own $DATA stream - used below to reach extension
        // records, regardless of whether the attribute actually requested
        // here is $DATA or $BITMAP (issue #11 was $BITMAP behind an
        // attribute list).
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

        // Translates an MFT record number to a byte position on the volume
        // by walking `runs`. `None` if it isn't covered by them: such a
        // record can't be located before the full `$DATA` is read, so its
        // list entry is skipped rather than guessed at (for example with
        // the old `mft_position + n * file_record_size` formula, which
        // silently reads the wrong bytes once `$DATA` is fragmented).
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

        // Follow the attribute list, if any, for extension records holding
        // further extents. A target that can't be located through
        // `mft_data_runs`, or whose base isn't record 0, is skipped rather
        // than trusted. Their bytes are kept (alongside the entry's
        // declared VCN) so attributes can be borrowed from them below,
        // outliving this loop.
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
                // The record at this position may have been freed and
                // reused since the list was written; only trust it if its
                // current sequence still matches what the entry expects
                // (NtfsFile::records makes the same check for a base record's
                // own reference).
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

        // The extents must tile the value: each one starts at the VCN the
        // ones before it end at. A missing or repeated extent (a stale or
        // duplicated list entry, or a corrupt VCN) would silently shift
        // everything after it.
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

    /// Reads `size` bytes out of `runs` (already checked against `size` by
    /// the caller), sequentially: sparse runs contribute zeroes.
    ///
    /// `size` comes straight from an on-disk attribute header, checked so
    /// far only against the runs' own declared lengths, which a corrupt or
    /// hostile volume can inflate just as cheaply as `size` itself (found by
    /// fuzzing: a few dozen bytes claimed a 2 GiB `$DATA`). `try_reserve`
    /// below already turns a failed allocation into an error instead of
    /// aborting, but attempting a large-but-technically satisfiable
    /// allocation is still wasted work for an amount no genuine stream could
    /// reach on the volume it claims to be on, so this bounds `size` against
    /// the volume's own size first. A `volume_size` of 0 (every synthetic
    /// `Volume` in this crate's own tests) skips the bound rather than
    /// rejecting everything: a real `Volume::new` always fills it in from the
    /// boot sector.
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
        // The array must hold one saved value per sector, or the last
        // sectors' ends would stay unrestored (Windows rejects such a
        // record). A zero length is not a record at all (a free or zeroed
        // slot): nothing to fix up, and `Record::new` rejects it.
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

/// Synthetic MFT record fixtures. `pub` (not `pub(crate)`) only under
/// `internals`, so `benches/*_synthetic.rs` can build a synthetic `Mft` the
/// same way the unit tests do, without a second copy of the low-level
/// record-building code; see the crate's `internals` feature doc.
#[cfg(any(test, feature = "internals"))]
#[doc(hidden)]
pub mod test_records;

#[cfg(test)]
mod structured_tests;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::test_records::*;
    use super::*;
    use crate::file::NtfsDataStream;
    use crate::file_info::FileInfo;
    use crate::path::{DefaultPathCache, PathCache};

    #[test]
    fn combines_base_and_extension_records_into_one_file() {
        let attributes = NtfsFileNameFlags::Hidden as u32
            | NtfsFileNameFlags::System as u32
            | NtfsFileNameFlags::Archive as u32
            | NtfsFileNameFlags::SparseFile as u32;
        let file_size = 34_359_738_368u64;

        let mut base = new_record(24, 7, 0);
        let mut offset = ATTRIBUTES_OFFSET;
        offset = add_standard_information(&mut base, offset, attributes);
        offset = add_nonresident_data(&mut base, offset, file_size);
        finish_record(&mut base, offset);

        let base_reference = (7u64 << 48) | 24;
        let mut extension = new_record(25, 3, base_reference);
        let offset = add_file_name(
            &mut extension,
            ATTRIBUTES_OFFSET,
            "large-fragmented.rar",
            attributes,
        );
        finish_record(&mut extension, offset);

        let mft = mft_with(vec![base, extension]);

        let files: Vec<_> = mft.files().collect();
        assert_eq!(files.len(), 1, "extension record must not be a second file");
        assert_eq!(files[0].number(), 24);
        assert_eq!(files[0].records().count(), 2);

        let info = FileInfo::new(&files[0]);
        assert_eq!(info.name, "large-fragmented.rar");
        assert_eq!(info.size, file_size);
        assert_eq!(info.file_attributes, attributes);
    }

    #[test]
    fn hard_links_exclude_dos_aliases() {
        let attributes = NtfsFileNameFlags::Archive as u32;
        // Different parent: a second real hard link.
        let other_parent = (2u64 << 48) | 100;

        let mut base = new_record(24, 1, 0);
        // Windows counts the DOS alias in link_count too.
        write_u16(&mut base, 18, 3);
        let mut offset = ATTRIBUTES_OFFSET;
        for (id, parent, namespace, name) in [
            (1, ROOT_RECORD, NtfsFileNamespace::Win32, "longfilename.txt"),
            (2, ROOT_RECORD, NtfsFileNamespace::Dos, "LONGFI~1.TXT"),
            (3, other_parent, NtfsFileNamespace::Posix, "secondlink.txt"),
        ] {
            offset = add_file_name_ex(&mut base, offset, id, parent, namespace, name, attributes);
        }
        finish_record(&mut base, offset);

        let mft = mft_with(vec![base]);
        let file = mft.files().next().expect("one file");

        assert_eq!(file.names().count(), 3);

        let links: Vec<_> = file
            .hard_links()
            .map(|name| (name.to_string(), name.parent_number()))
            .collect();
        assert_eq!(
            links,
            [
                ("longfilename.txt".to_string(), ROOT_RECORD),
                ("secondlink.txt".to_string(), 100),
            ]
        );

        let best = file.best_name().expect("best name");
        assert_eq!(best.to_string(), "longfilename.txt");
    }

    #[test]
    fn data_streams_include_alternate_streams() {
        let mut base = new_record(24, 1, 0);
        let mut offset = ATTRIBUTES_OFFSET;
        offset = add_file_name(&mut base, offset, "file.txt", 0);
        offset =
            add_resident_attribute(&mut base, offset, NtfsAttributeType::Data, 2, "", b"hello");
        offset = add_resident_attribute(
            &mut base,
            offset,
            NtfsAttributeType::Data,
            3,
            "Zone.Identifier",
            b"[ZoneTransfer]",
        );
        finish_record(&mut base, offset);

        let mft = mft_with(vec![base]);
        let file = mft.files().next().expect("one file");

        let streams: Vec<_> = file.data_streams().collect();
        assert_eq!(
            streams,
            [
                NtfsDataStream {
                    name: None,
                    size: 5
                },
                NtfsDataStream {
                    name: Some("Zone.Identifier".into()),
                    size: 14
                },
            ]
        );
        assert_eq!(file.resident_data(), Some(&b"hello"[..]));
        assert_eq!(FileInfo::new(&file).size, 5);
    }

    #[test]
    fn resolves_a_path_for_every_hard_link() {
        let mut directory = new_record(24, 1, 0);
        write_u16(
            &mut directory,
            22,
            NtfsFileFlags::InUse as u16 | NtfsFileFlags::IsDirectory as u16,
        );
        let offset = add_file_name(&mut directory, ATTRIBUTES_OFFSET, "dir", 0);
        finish_record(&mut directory, offset);

        let mut file = new_record(25, 1, 0);
        let mut offset = ATTRIBUTES_OFFSET;
        offset = add_file_name_ex(
            &mut file,
            offset,
            1,
            (1u64 << 48) | 24,
            NtfsFileNamespace::Win32AndDos,
            "a.txt",
            0,
        );
        offset = add_file_name_ex(
            &mut file,
            offset,
            2,
            (ROOT_SEQUENCE as u64) << 48 | ROOT_RECORD,
            NtfsFileNamespace::Win32AndDos,
            "b.txt",
            0,
        );
        finish_record(&mut file, offset);

        let mft = mft_with(vec![directory, file]);
        let file = mft.record(25).expect("file record");

        let mut cache = DefaultPathCache::new();
        let paths: Vec<_> = file
            .hard_links()
            .map(|link| mft.resolve_path(&link, &mut cache))
            .collect();
        // Built with `.join()`, not a hard-coded `\`-joined literal: `resolve_path` assembles
        // the result with `PathBuf::push`, which uses the platform's own separator, so a test
        // comparing against a literal Windows path would only pass when run on Windows.
        assert_eq!(
            paths,
            [
                Some(PathBuf::from(r"\\.\T:").join("dir").join("a.txt")),
                Some(PathBuf::from(r"\\.\T:").join("b.txt")),
            ]
        );
        // The cache is keyed by the directory's full reference (sequence 1, record 24).
        assert_eq!(
            cache.get((1u64 << 48) | 24),
            crate::path::CachedPath::Resolved(PathBuf::from(r"\\.\T:").join("dir").as_path())
        );
    }

    #[test]
    fn read_data_fs_follows_resident_attribute_list() {
        let entry = |type_id: NtfsAttributeType, record: u64| {
            let mut entry = vec![0u8; 32];
            write_u32(&mut entry, 0, type_id as u32);
            write_u16(&mut entry, 4, 32);
            entry[7] = 26;
            write_u64(&mut entry, 16, (1u64 << 48) | record);
            entry
        };

        let mut base = new_record(0, 1, 0);
        let list = [
            entry(NtfsAttributeType::StandardInformation, 0),
            entry(NtfsAttributeType::Bitmap, 1),
        ]
        .concat();
        let mut offset = add_resident_attribute(
            &mut base,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::AttributeList,
            0,
            "",
            &list,
        );
        // $MFT's own $DATA, needed to locate record 1 through the list.
        offset = add_identity_mft_data(&mut base, offset, 1);
        finish_record(&mut base, offset);

        let mut extension = new_record(1, 1, 1u64 << 48);
        let offset = add_resident_attribute(
            &mut extension,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::Bitmap,
            1,
            "",
            &[0xAB, 0xCD],
        );
        finish_record(&mut extension, offset);

        let volume = test_volume();
        let mut reader = std::io::Cursor::new([base.clone(), extension].concat());
        let read = |attribute_type, reader: &mut std::io::Cursor<Vec<u8>>| {
            Mft::read_data_fs(&volume, reader, &base, attribute_type).expect("read")
        };

        assert_eq!(
            read(NtfsAttributeType::Bitmap, &mut reader),
            Some(vec![0xAB, 0xCD])
        );
        // StandardInformation's list entry points back at record 0 (self)
        // and record 0 has no such attribute; genuinely absent, unlike
        // Data, which now stands for $MFT's own bootstrap attribute above.
        assert_eq!(
            read(NtfsAttributeType::StandardInformation, &mut reader),
            None
        );
    }

    // `$MFT`'s attribute list can itself be non-resident (on a fragmented volume it outgrows the
    // record): the list is read through its own runs, from a cluster of its own, and the entry in
    // it still reaches the extension record.
    #[test]
    fn read_data_fs_reads_a_non_resident_attribute_list() {
        let list = list_entry(NtfsAttributeType::Bitmap, "", 0, 1);
        let mut base = new_record(0, 1, 0);
        // The list is in cluster 1, after the four records the identity `$DATA` maps.
        let mut offset = add_nonresident_data_runs(
            &mut base,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::AttributeList,
            "",
            0,
            0,
            list.len() as u64,
            &encode_runs(&[(1, Some(1))]),
        );
        offset = add_identity_mft_data(&mut base, offset, 1);
        finish_record(&mut base, offset);

        let mut extension = new_record(1, 1, 1u64 << 48);
        let offset = add_resident_attribute(
            &mut extension,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::Bitmap,
            1,
            "",
            &[0xAB, 0xCD],
        );
        finish_record(&mut extension, offset);

        let mut image = vec![0u8; 2 * CLUSTER_SIZE];
        image[..RECORD_SIZE].copy_from_slice(&base);
        image[RECORD_SIZE..2 * RECORD_SIZE].copy_from_slice(&extension);
        image[CLUSTER_SIZE..CLUSTER_SIZE + list.len()].copy_from_slice(&list);

        let data = Mft::read_data_fs(
            &test_volume(),
            &mut std::io::Cursor::new(image),
            &base,
            NtfsAttributeType::Bitmap,
        )
        .expect("read");
        assert_eq!(data, Some(vec![0xAB, 0xCD]));
    }

    // Record 0's own `$DATA` has two runs with a physical gap between them; the extension record
    // the attribute list points at (record 4) falls in the second run. A decoy sits at the position
    // a contiguous `n * file_record_size` formula would read (byte 4096), so that formula fails
    // the assertion below instead of passing by luck.
    #[test]
    fn read_data_fs_locates_extension_record_through_fragmented_mft_data() {
        let list = list_entry(NtfsAttributeType::Bitmap, "", 0, 4);
        let mut base = new_record(0, 1, 0);
        let mut offset = add_resident_attribute(
            &mut base,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::AttributeList,
            0,
            "",
            &list,
        );
        // $MFT's own $DATA, VCN-0 extent: two runs, physically apart (a gap
        // between cluster 1 and cluster 5). Records 0-3 map through the
        // first run (logical bytes 0..4096), records 4-7 through the
        // second (logical bytes 4096..8192, physically at cluster 5).
        offset = add_nonresident_data_runs(
            &mut base,
            offset,
            NtfsAttributeType::Data,
            "",
            0,
            1,
            2 * CLUSTER_SIZE as u64,
            &[0x11, 0x01, 0x01, 0x11, 0x01, 0x04],
        );
        finish_record(&mut base, offset);

        let mut image = vec![0u8; 6 * CLUSTER_SIZE];

        // Decoy at the naive `mft_position + 4 * file_record_size` position
        // (byte 4096): a valid, correctly-based record 4, but with a
        // different value, so reading it instead of the real one is
        // visible in the assertion below.
        let mut decoy = new_record(999, 1, 1u64 << 48);
        let decoy_offset = add_resident_attribute(
            &mut decoy,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::Bitmap,
            1,
            "",
            &[0x99, 0x99],
        );
        finish_record(&mut decoy, decoy_offset);
        image[4096..4096 + RECORD_SIZE].copy_from_slice(&decoy);

        // The real record 4, physically in the second run (cluster 5, byte
        // 20480), at the very start of its coverage (within-run offset 0).
        let mut extension = new_record(4, 1, 1u64 << 48);
        let extension_offset = add_resident_attribute(
            &mut extension,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::Bitmap,
            1,
            "",
            &[0xAB, 0xCD],
        );
        finish_record(&mut extension, extension_offset);
        image[20480..20480 + RECORD_SIZE].copy_from_slice(&extension);

        let mut reader = std::io::Cursor::new(image);
        let data = Mft::read_data_fs(
            &test_volume(),
            &mut reader,
            &base,
            NtfsAttributeType::Bitmap,
        )
        .expect("read");
        assert_eq!(data, Some(vec![0xAB, 0xCD]));
    }

    // Card 009: the real `$MFT` shape. Record 0 holds the VCN-0 extent and
    // the list, whose entries also point back at record 0.
    #[test]
    fn read_data_fs_joins_base_extent_with_extension_extent() {
        // Record 3, not 1: real MFT records pack four to a cluster
        // (RECORD_SIZE 1024 into CLUSTER_SIZE 4096), so any non-zero
        // extension record physically shares its cluster with VCN 0's own
        // content - here, the last record-sized slot of it.
        let list = [
            list_entry(NtfsAttributeType::AttributeList, "", 0, 0),
            list_entry(NtfsAttributeType::Data, "", 0, 0),
            list_entry(NtfsAttributeType::Data, "", 1, 3),
        ]
        .concat();
        let mut base = new_record(0, 1, 0);
        let mut offset = add_resident_attribute(
            &mut base,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::AttributeList,
            0,
            "",
            &list,
        );
        offset = add_nonresident_data_runs(
            &mut base,
            offset,
            NtfsAttributeType::Data,
            "",
            0,
            0,
            2 * CLUSTER_SIZE as u64,
            &[0x11, 0x01, 0x01],
        );
        finish_record(&mut base, offset);

        let mut extension = new_record(3, 1, 1u64 << 48);
        let offset = add_nonresident_data_runs(
            &mut extension,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::Data,
            "",
            1,
            1,
            0,
            &[0x11, 0x01, 0x02],
        );
        finish_record(&mut extension, offset);

        // VCN 0 is at cluster 1 (bytes 4096..8192); record 3 (byte
        // 4096 + 3 * RECORD_SIZE = 7168) lives in its last record slot, so
        // only the first three slots are content-checked below. VCN 1 is
        // at cluster 2, untouched by record placement.
        let mut image = vec![0u8; 4 * CLUSTER_SIZE];
        image[4096..7168].fill(0xAA);
        image[7168..7168 + RECORD_SIZE].copy_from_slice(&extension);
        image[8192..12288].fill(0xBB);

        let mut reader = std::io::Cursor::new(image);
        let data = Mft::read_data_fs(&test_volume(), &mut reader, &base, NtfsAttributeType::Data)
            .expect("read")
            .expect("present");
        assert_eq!(data.len(), 2 * CLUSTER_SIZE);
        assert!(data[..3 * RECORD_SIZE].iter().all(|&b| b == 0xAA));
        assert!(data[CLUSTER_SIZE..].iter().all(|&b| b == 0xBB));
    }

    // Card 009 (spec 009 SC-002): a named `$DATA` before the unnamed one.
    #[test]
    fn read_data_fs_skips_named_attribute() {
        let mut base = new_record(0, 1, 0);
        let mut offset = add_resident_attribute(
            &mut base,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::Data,
            1,
            "stream",
            b"wrong",
        );
        offset =
            add_resident_attribute(&mut base, offset, NtfsAttributeType::Data, 2, "", b"right");
        finish_record(&mut base, offset);

        let mut reader = std::io::Cursor::new(base.clone());
        let data = Mft::read_data_fs(&test_volume(), &mut reader, &base, NtfsAttributeType::Data)
            .expect("read");
        assert_eq!(data.as_deref(), Some(&b"right"[..]));
    }

    // Card 009 (FR-004): a named `$DATA` reached through the list is skipped.
    #[test]
    fn read_data_fs_skips_named_attribute_in_list() {
        let mut base = new_record(0, 1, 0);
        let list = list_entry(NtfsAttributeType::Bitmap, "stream", 0, 1);
        let mut offset = add_resident_attribute(
            &mut base,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::AttributeList,
            0,
            "",
            &list,
        );
        // $MFT's own $DATA, needed to locate record 1 through the list.
        offset = add_identity_mft_data(&mut base, offset, 1);
        finish_record(&mut base, offset);

        let mut extension = new_record(1, 1, 1u64 << 48);
        let offset = add_resident_attribute(
            &mut extension,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::Bitmap,
            1,
            "stream",
            &[0xEE],
        );
        finish_record(&mut extension, offset);

        let mut reader = std::io::Cursor::new([base.clone(), extension].concat());
        let data = Mft::read_data_fs(
            &test_volume(),
            &mut reader,
            &base,
            NtfsAttributeType::Bitmap,
        )
        .expect("read");
        assert_eq!(data, None);
    }

    // Card 009 (FR-002): a list entry whose target record belongs to another
    // base record is ignored.
    #[test]
    fn read_data_fs_ignores_record_of_another_base() {
        let mut base = new_record(0, 1, 0);
        let list = [
            list_entry(NtfsAttributeType::Bitmap, "", 0, 1),
            list_entry(NtfsAttributeType::Bitmap, "", 0, 2),
        ]
        .concat();
        let mut offset = add_resident_attribute(
            &mut base,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::AttributeList,
            0,
            "",
            &list,
        );
        // $MFT's own $DATA, needed to locate records 1 and 2 through the list.
        offset = add_identity_mft_data(&mut base, offset, 1);
        finish_record(&mut base, offset);

        // Record 1 is an extension of record 5, not of record 0.
        let mut foreign = new_record(1, 1, (1u64 << 48) | ROOT_RECORD);
        let offset = add_resident_attribute(
            &mut foreign,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::Bitmap,
            1,
            "",
            &[0xEE],
        );
        finish_record(&mut foreign, offset);
        let mut own = new_record(2, 1, 1u64 << 48);
        let offset = add_resident_attribute(
            &mut own,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::Bitmap,
            1,
            "",
            &[0xAB, 0xCD],
        );
        finish_record(&mut own, offset);

        let mut reader = std::io::Cursor::new([base.clone(), foreign, own].concat());
        let data = Mft::read_data_fs(
            &test_volume(),
            &mut reader,
            &base,
            NtfsAttributeType::Bitmap,
        )
        .expect("read");
        assert_eq!(data, Some(vec![0xAB, 0xCD]));
    }

    // Card 009 (review follow-up): a list entry whose target record's
    // sequence no longer matches what the entry expects - freed and reused
    // since the list was written - is ignored, the same check
    // NtfsFile::records makes for a base record's own reference.
    #[test]
    fn read_data_fs_ignores_record_of_stale_sequence() {
        let mut base = new_record(0, 1, 0);
        let list = [
            list_entry(NtfsAttributeType::Bitmap, "", 0, 1),
            list_entry(NtfsAttributeType::Bitmap, "", 0, 2),
        ]
        .concat();
        let mut offset = add_resident_attribute(
            &mut base,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::AttributeList,
            0,
            "",
            &list,
        );
        // $MFT's own $DATA, needed to locate records 1 and 2 through the list.
        offset = add_identity_mft_data(&mut base, offset, 1);
        finish_record(&mut base, offset);

        // list_entry() always writes sequence 1. Record 1's base is still
        // record 0 (unlike the "another base" case above), but its own
        // sequence is 2: freed and reused since the list was written.
        let mut stale = new_record(1, 2, 1u64 << 48);
        let offset = add_resident_attribute(
            &mut stale,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::Bitmap,
            1,
            "",
            &[0xEE],
        );
        finish_record(&mut stale, offset);

        // Record 2's sequence (1) matches what the list expects.
        let mut fresh = new_record(2, 1, 1u64 << 48);
        let offset = add_resident_attribute(
            &mut fresh,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::Bitmap,
            1,
            "",
            &[0xAB, 0xCD],
        );
        finish_record(&mut fresh, offset);

        let mut reader = std::io::Cursor::new([base.clone(), stale, fresh].concat());
        let data = Mft::read_data_fs(
            &test_volume(),
            &mut reader,
            &base,
            NtfsAttributeType::Bitmap,
        )
        .expect("read");
        assert_eq!(data, Some(vec![0xAB, 0xCD]));
    }

    // Card 009 (spec 009 SC-003): a short record is an error, not a panic.
    #[test]
    fn read_data_fs_rejects_short_record() {
        let record = [0u8; 16];
        let mut reader = std::io::Cursor::new(Vec::new());
        let result = Mft::read_data_fs(
            &test_volume(),
            &mut reader,
            &record,
            NtfsAttributeType::Data,
        );
        assert!(result.is_err());
    }

    // Card 028: found by fuzzing (a byte-level cargo-fuzz target, since removed). A
    // non-resident $DATA attribute's declared size is trusted up to u64::MAX with nothing to
    // check it against except the data runs actually present - and an attacker can supply
    // matching runs cheaply (a handful of bytes can claim billions of clusters). try_reserve
    // already keeps that from panicking, but a large-but-technically satisfiable request (2 GiB
    // in the fuzzer's minimized case) aborted the fuzzer's process outright, and even off the
    // fuzzer it is real wasted work for an amount no genuine $MFT stream could reach on the
    // volume it claims to be on. Fixed by bounding the declared size against the volume's own
    // size in `read_runs`, the one place that actually allocates.
    #[test]
    fn read_data_fs_rejects_size_larger_than_the_volume() {
        let mut base = new_record(0, 1, 0);
        let offset = add_nonresident_data_runs(
            &mut base,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::Data,
            "",
            0,
            0,
            1_000_000,
            // 250 clusters (4096 bytes each, test_volume()'s cluster size) = 1,024,000 bytes of
            // runs: enough to pass the "runs at least cover the declared size" check, so the
            // rejection below is really about the volume-size bound, not a shorter-runs error.
            &[0x11, 250, 1],
        );
        finish_record(&mut base, offset);

        // No real volume this small could hold a 1,000,000-byte $DATA.
        let volume = test_volume().with_volume_size(1000);

        let mut reader = std::io::Cursor::new(vec![0u8; 2000]);
        let result = Mft::read_data_fs(&volume, &mut reader, &base, NtfsAttributeType::Data);
        assert!(
            matches!(result, Err(NtfsReaderError::InvalidDataRun { .. })),
            "a declared $DATA size (1,000,000) larger than the whole volume (1000 bytes) must be \
             rejected, not trusted enough to attempt allocating it; got {result:?}",
        );
    }

    // The `$MFT` data is read into one buffer, so its size must fit `usize`: on a 32-bit build a
    // size above 4 GiB is refused, not truncated to a short buffer. The volume is unbounded
    // (`volume_size` 0), so the volume-size check above is not what refuses it.
    #[cfg(target_pointer_width = "32")]
    #[test]
    fn read_runs_rejects_a_size_that_does_not_fit_in_usize() {
        let size = 1u64 << 32;
        let mut reader = std::io::Cursor::new(Vec::new());

        let result = Mft::read_runs(
            &mut reader,
            &test_volume(),
            size,
            &[DataRun::Sparse { length: size }],
        );

        assert!(
            matches!(result, Err(NtfsReaderError::AllocationTooLarge { size: refused }) if refused == size),
            "{result:?}"
        );
    }

    // A size that fits `usize` but no allocator can satisfy is an error too, not an abort.
    #[test]
    fn read_runs_reports_an_allocation_that_cannot_be_made() {
        let size = 1u64 << 62;
        let mut reader = std::io::Cursor::new(Vec::new());

        let result = Mft::read_runs(
            &mut reader,
            &test_volume(),
            size,
            &[DataRun::Sparse { length: size }],
        );

        assert!(
            matches!(result, Err(NtfsReaderError::AllocationTooLarge { size: refused }) if refused == size),
            "{result:?}"
        );
    }

    // Card 009: translating a record number through $MFT's own $DATA runs
    // must not overflow, even when a run's LCN sits implausibly close to
    // u64::MAX.
    #[test]
    fn read_data_fs_does_not_overflow_record_position() {
        let mut base = new_record(0, 1, 0);
        let list = list_entry(NtfsAttributeType::Bitmap, "", 0, 4);
        let mut offset = add_resident_attribute(
            &mut base,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::AttributeList,
            0,
            "",
            &list,
        );
        // One run whose LCN is the largest multiple of CLUSTER_SIZE that
        // fits in a u64: record 4's within-run offset (4096 bytes) then
        // pushes the translated position exactly one past u64::MAX.
        let cluster_offset = u64::MAX / CLUSTER_SIZE as u64;
        let mut runs = vec![0x81, 0x02];
        runs.extend(cluster_offset.to_le_bytes());
        offset = add_nonresident_data_runs(
            &mut base,
            offset,
            NtfsAttributeType::Data,
            "",
            0,
            1,
            2 * CLUSTER_SIZE as u64,
            &runs,
        );
        finish_record(&mut base, offset);

        // What a wrapped-around position (0) would read: a valid extension record of record
        // 0 that holds the bitmap. The list entry names it, so only the overflow keeps it out.
        let mut extension = new_record(4, 1, reference(1, 0));
        let offset = add_resident_attribute(
            &mut extension,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::Bitmap,
            1,
            "",
            b"bits",
        );
        finish_record(&mut extension, offset);

        let mut reader = std::io::Cursor::new(extension);
        let result = Mft::read_data_fs(
            &test_volume(),
            &mut reader,
            &base,
            NtfsAttributeType::Bitmap,
        );
        assert!(matches!(result, Ok(None)), "{result:?}");
    }

    // Card 010: a record whose update sequence does not match is skipped
    // instead of failing the whole load.
    #[test]
    fn load_skips_record_with_bad_fixup() {
        const SECTOR_END_VALUE: u16 = 0x1234;
        let first = FIRST_NORMAL_RECORD as usize;
        let names = ["a.txt", "bad.txt", "c.txt"];

        let mut data = vec![0u8; first * RECORD_SIZE];
        let mut bitmap = vec![0u8; (first + names.len()).div_ceil(8)];
        for (index, name) in names.iter().enumerate() {
            let number = first + index;
            let mut record = new_record(number as u64, 1, 0);
            let offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, name, 0);
            finish_record(&mut record, offset);
            write_u16(&mut record, SECTOR_SIZE - 2, SECTOR_END_VALUE);
            protect_record(&mut record, 7);
            if *name == "bad.txt" {
                record[RECORD_SIZE - 2] ^= 0xFF;
            }
            bitmap[number / 8] |= 1 << (number % 8);
            data.extend(record);
        }

        let mft = Mft::from_parts(test_volume(), data, bitmap)
            .expect("one bad record must not fail the load");

        let loaded: Vec<_> = mft
            .files()
            .map(|file| file.best_name().expect("name").to_string())
            .collect();
        assert_eq!(loaded, ["a.txt", "c.txt"]);
        assert!(mft.record(first as u64 + 1).is_none());
        assert_eq!(mft.corrupt_records(), 1);

        // Fixups were still applied to the good records.
        let sector_end = first * RECORD_SIZE + SECTOR_SIZE - 2;
        assert_eq!(
            u16::from_le_bytes([mft.data[sector_end], mft.data[sector_end + 1]]),
            SECTOR_END_VALUE
        );
    }

    // Card 011: an absolute LCN past `u64::MAX` bytes must be an error, not
    // silently truncated.
    #[test]
    fn data_run_past_u64_max_is_an_error() {
        // 0x81: 1-byte cluster count, 8-byte offset. i64::MAX clusters of
        // 4096 bytes is about 2^75 bytes.
        let mut runs = vec![0x81, 0x01];
        runs.extend(i64::MAX.to_le_bytes());

        let mut record = new_record(24, 1, 0);
        let offset = add_nonresident_data_runs(
            &mut record,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::Data,
            "",
            0,
            0,
            CLUSTER_SIZE as u64,
            &runs,
        );
        finish_record(&mut record, offset);

        let file = Record::new(24, &record).expect("record");
        let attribute = file.attributes().next().expect("data attribute");
        let result = attribute.nonresident_data_runs(&test_volume());
        assert!(result.is_err(), "{result:?}");
    }

    // Card 012: a record whose base reference is itself is not its own
    // extension. A real base/extension pair next to it must still combine.
    #[test]
    fn self_referencing_record_yields_attributes_once() {
        let mut looped = new_record(24, 1, (1u64 << 48) | 24);
        let mut offset = add_standard_information(&mut looped, ATTRIBUTES_OFFSET, 0);
        offset = add_file_name(&mut looped, offset, "loop.txt", 0);
        finish_record(&mut looped, offset);

        let mut base = new_record(25, 1, 0);
        let offset = add_standard_information(&mut base, ATTRIBUTES_OFFSET, 0);
        finish_record(&mut base, offset);
        let mut extension = new_record(26, 1, (1u64 << 48) | 25);
        let offset = add_file_name(&mut extension, ATTRIBUTES_OFFSET, "base.txt", 0);
        finish_record(&mut extension, offset);

        let mft = mft_with(vec![looped, base, extension]);

        let looped = mft.record(24).expect("record 24");
        assert_eq!(looped.records().count(), 1);
        assert_eq!(looped.attributes().count(), 2);

        let base = mft.record(25).expect("record 25");
        assert_eq!(base.records().count(), 2);
        assert_eq!(base.attributes().count(), 2);
    }

    // `corrupt_records` counts the allocated slots (per the bitmap) that could not be used,
    // whichever check rejected them: a failed update sequence check or a header that is not a
    // `FILE` record. A free slot is not a corrupt record, whatever bytes it holds.
    #[test]
    fn corrupt_records_counts_only_allocated_slots() {
        let first = FIRST_NORMAL_RECORD;
        let good = |number: u64| {
            let mut record = new_record(number, 1, 0);
            let offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, "good.txt", 0);
            finish_record(&mut record, offset);
            record
        };
        let bad_fixup = |number: u64| {
            let mut record = good(number);
            record[RECORD_SIZE - 2] ^= 0xFF;
            record
        };
        // Passes the update sequence check (no array) but is no `FILE` record.
        let not_a_record = || vec![0u8; RECORD_SIZE];

        let (allocated_bad_fixup, allocated_not_a_record) = (first + 1, first + 2);
        let (free_bad_fixup, free_not_a_record, free_bad_fixup_too) =
            (first + 3, first + 4, first + 5);
        let (volume, data, mut bitmap) = raw_parts(vec![
            good(first),
            bad_fixup(allocated_bad_fixup),
            not_a_record(),
            bad_fixup(free_bad_fixup),
            not_a_record(),
            bad_fixup(free_bad_fixup_too),
        ]);
        for number in [free_bad_fixup, free_not_a_record, free_bad_fixup_too] {
            bitmap[number as usize / 8] &= !(1 << (number % 8));
        }

        let mft = build_from_parts(volume, data, bitmap);

        assert_eq!(mft.corrupt_records(), 2);
        let files: Vec<_> = mft.files().map(|file| file.number()).collect();
        assert_eq!(files, [first]);
        for number in [allocated_bad_fixup, allocated_not_a_record] {
            assert!(mft.is_allocated(number) && mft.record(number).is_none());
        }
    }

    // Windows rejects a record whose update sequence array does not hold one
    // saved value for each of its sectors. Accepting one would leave the ends
    // of the sectors past the array unrestored.
    #[test]
    fn a_short_or_long_update_sequence_array_is_rejected() {
        let first = FIRST_NORMAL_RECORD as usize;
        for count in [1u16, 2, 4, 200] {
            let mut good = new_record(first as u64, 1, 0);
            let offset = add_file_name(&mut good, ATTRIBUTES_OFFSET, "good.txt", 0);
            finish_record(&mut good, offset);
            let mut bad = new_record(first as u64 + 1, 1, 0);
            let offset = add_file_name(&mut bad, ATTRIBUTES_OFFSET, "bad.txt", 0);
            finish_record(&mut bad, offset);
            write_u16(&mut bad, 6, count);

            assert!(
                Record::new(first as u64 + 1, &bad).is_none(),
                "an array of {count} entries passes for a 2 sector record"
            );
            let mft = mft_with(vec![good, bad]);
            let names: Vec<_> = mft
                .files()
                .map(|file| file.best_name().expect("name").to_string())
                .collect();
            assert_eq!(names, ["good.txt"], "array of {count}");
            assert_eq!(mft.corrupt_records(), 1, "array of {count}");
            assert!(mft.record(first as u64 + 1).is_none());
        }
    }

    // A record's data crosses the end of its first sector, where the update
    // sequence number sits on disk. The fixup must give the real bytes back.
    #[test]
    fn fixups_restore_the_data_at_the_end_of_each_sector() {
        // Fills the record to its last byte: the value covers both sector ends.
        let value: Vec<u8> = (0..839).map(|index| (index % 251) as u8 + 1).collect();
        let mut record = new_record(FIRST_NORMAL_RECORD, 1, 0);
        let mut offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, "f.txt", 0);
        offset =
            add_resident_attribute(&mut record, offset, NtfsAttributeType::Data, 2, "", &value);
        assert!(
            offset > SECTOR_SIZE + 500,
            "the value must cross both sector ends"
        );
        finish_record(&mut record, offset);

        let protected = record.clone();
        assert_ne!(
            &protected[SECTOR_SIZE - 2..SECTOR_SIZE],
            &record_without_protection(&record)[SECTOR_SIZE - 2..SECTOR_SIZE],
            "the sector end holds the update sequence number on disk"
        );

        let mft = mft_with(vec![record]);
        assert_eq!(mft.corrupt_records(), 0);
        let file = mft.files().next().expect("one file");
        assert_eq!(file.resident_data(), Some(&value[..]));
    }

    /// The record with each sector end replaced by the saved value, by hand.
    fn record_without_protection(record: &[u8]) -> Vec<u8> {
        let mut record = record.to_vec();
        for sector in 0..RECORD_SIZE / SECTOR_SIZE {
            let end = (sector + 1) * SECTOR_SIZE - 2;
            let slot = UPDATE_SEQUENCE_OFFSET + 2 + sector * 2;
            record.copy_within(slot..slot + 2, end);
        }
        record
    }

    /// `$MFT`'s `$DATA` in two extents: VCN 0 in record 0 and the extent
    /// `extension_vcn` in record 3 (one cluster each), reached through an
    /// attribute list with `entries`.
    fn read_split_mft_data(
        entries: &[Vec<u8>],
        extension_vcn: u64,
    ) -> NtfsReaderResult<Option<Vec<u8>>> {
        let mut base = new_record(0, 1, 0);
        let mut offset = add_resident_attribute(
            &mut base,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::AttributeList,
            0,
            "",
            &entries.concat(),
        );
        offset = add_nonresident_data_runs(
            &mut base,
            offset,
            NtfsAttributeType::Data,
            "",
            0,
            0,
            2 * CLUSTER_SIZE as u64,
            &[0x11, 0x01, 0x01],
        );
        finish_record(&mut base, offset);

        let mut extension = new_record(3, 1, 1u64 << 48);
        let offset = add_nonresident_data_runs(
            &mut extension,
            ATTRIBUTES_OFFSET,
            NtfsAttributeType::Data,
            "",
            extension_vcn,
            extension_vcn,
            0,
            &[0x11, 0x01, 0x02],
        );
        finish_record(&mut extension, offset);

        let mut image = vec![0u8; 4 * CLUSTER_SIZE];
        image[7168..7168 + RECORD_SIZE].copy_from_slice(&extension);
        Mft::read_data_fs(
            &test_volume(),
            &mut std::io::Cursor::new(image),
            &base,
            NtfsAttributeType::Data,
        )
    }

    // The extents of a value must follow each other by VCN. A missing extent
    // (the second starts at VCN 2, after a gap) or one listed twice would
    // otherwise be joined anyway and shift everything after it.
    #[test]
    fn read_data_fs_rejects_extents_that_do_not_follow_each_other() {
        let entry = |vcn, record| list_entry(NtfsAttributeType::Data, "", vcn, record);
        let list_start = list_entry(NtfsAttributeType::AttributeList, "", 0, 0);

        let joined = read_split_mft_data(&[list_start.clone(), entry(0, 0), entry(1, 3)], 1)
            .expect("read")
            .expect("present");
        assert_eq!(joined.len(), 2 * CLUSTER_SIZE, "the well-formed pair joins");

        let gap = read_split_mft_data(&[list_start.clone(), entry(0, 0), entry(2, 3)], 2);
        assert!(
            matches!(gap, Err(NtfsReaderError::InvalidDataRun { .. })),
            "extents at VCN 0 and 2 (one cluster each): {gap:?}"
        );
        let repeated = read_split_mft_data(&[list_start, entry(0, 0), entry(1, 3), entry(1, 3)], 1);
        assert!(
            matches!(repeated, Err(NtfsReaderError::InvalidDataRun { .. })),
            "the same extent twice: {repeated:?}"
        );
    }
}
