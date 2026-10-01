// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! [`Mft`]: the whole `$MFT` of a volume, loaded into memory.

use std::fmt;
use std::io::{Read, Seek, SeekFrom};
use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{
    aligned_reader::open_volume,
    api::*,
    attribute::NtfsAttribute,
    data_run::DataRun,
    errors::{NtfsReaderError, NtfsReaderResult},
    file::{NtfsFile, Record},
    scan::SideStore,
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
    /// Record number of `data`'s first record: 0 for a whole `$MFT`, the window's start for a
    /// chunk of an [`MftScan`](crate::MftScan).
    first: u64,
    /// Records kept for the whole scan beside the window. `None` for a whole `$MFT`.
    side: Option<Box<SideStore>>,
    /// Compact-store offsets ([`Self::new_compact`]): `record_count + 1` entries in
    /// 8-byte units, `offsets[n]..offsets[n + 1]` is record `n`'s trimmed span in `data`, empty
    /// (`offsets[n] == offsets[n + 1]`) for a record not kept. `None` for the ordinary
    /// fixed-stride layout [`Self::new`]/[`MftScan`](crate::MftScan) use, where record `n` is
    /// always at `data[n * file_record_size..]` (relative to `first`). A `u32` table would wrap
    /// past `u32::MAX * 8` bytes (~32 GiB) of trimmed records, which a large enough volume's
    /// compact store reaches; a `u64` entry costs 4 more bytes a record slot, negligible next
    /// to what it indexes.
    offsets: Option<Vec<u64>>,
    /// This load's identity: distinct for every [`Self::new`]/[`Self::new_compact`]
    /// call and every [`MftScan`](crate::MftScan) (its chunks share one `Mft`, so its id stays
    /// the same across [`Self::load_window`] calls). What
    /// [`DefaultPathCache`](crate::DefaultPathCache) and
    /// [`DeletedPathCache`](crate::DeletedPathCache) key their contents to.
    id: MftId,
}

/// Opaque identity of one [`Mft`] load: [`Mft::new`], [`Mft::new_compact`], or one
/// [`MftScan`](crate::MftScan) (its chunks share it, since they share the `Mft`). Two loads are
/// never equal, even of the same volume back to back. Carries no accessible value and cannot be
/// constructed outside this crate; [`PathCache::check_owner`](crate::PathCache::check_owner) and
/// [`DeletedPathCache`](crate::DeletedPathCache) receive one to tell a fresh lookup from a stale
/// one left by an earlier `Mft`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MftId(u64);

impl MftId {
    fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
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
        state.names(reference, record.reference()).then_some(state)
    }

    /// The reference rule above using only the stored reference and liveness, so a scan's
    /// directory index can apply it without retaining a whole record. At stored sequence 1 a
    /// freed record accepts both sequence 0 and 0xFFFF; its canonical file id cannot encode both.
    pub(crate) fn names(self, reference: u64, stored_reference: u64) -> bool {
        if stored_reference & RECORD_NUMBER_MASK != reference & RECORD_NUMBER_MASK {
            return false;
        }
        let sequence = (reference >> 48) as u16;
        let stored = (stored_reference >> 48) as u16;
        match self {
            Self::Live => stored == sequence,
            Self::Freed => {
                stored == sequence.wrapping_add(1) || (sequence == 0xFFFF && stored == 1)
            }
        }
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

    /// Reads the `$MFT` of `volume` into memory like [`Self::new`], but stores each valid record
    /// trimmed to its `used_size` and update sequence array (rounded up to 8 bytes) instead of
    /// at a fixed stride: about 30 to 70 percent less memory on the volumes measured, since
    /// most of a record is unused slack (146 to 103 MiB, 392 to 118 MiB, and 1067 to 393 MiB).
    /// Every other accessor (`files`, `deleted_files`, `record`, `resolve_path`, streams,
    /// `FileInfo`) works the same as on
    /// [`Self::new`]'s `Mft`, since they all go through
    /// [`Self::record`]/`record_data`, which a compact store answers from its offset table. Kept
    /// records include freed ones (deleted files), the same as [`Self::new`]: dropping them saves
    /// little (a few percent of a volume's records on every one measured) for the whole of
    /// [`Self::deleted_files`]/[`NtfsFile::is_deleted`], so this is not offered as an option.
    ///
    /// Costs roughly 1.5 to 2x [`Self::new`]'s load time: `$MFT` `$DATA` is read twice, once to
    /// size the compact buffer and once to fill it, both through the same chunked reader
    /// [`MftScan`](crate::MftScan) uses, in pieces of 4096 records (4 MiB for 1 KiB records).
    /// Unchanged input needs one compact-buffer allocation; records that grow between passes
    /// can require more. Compact-store buffer and extension-index allocation failures return
    /// [`NtfsReaderError::AllocationTooLarge`].
    pub fn new_compact(volume: Volume) -> NtfsReaderResult<Self> {
        let mut reader = open_volume(volume.path())?;

        let mft_record = Self::read_record_fs(
            &mut reader,
            volume.file_record_size(),
            volume.mft_position(),
        )?;

        let value = Self::value_fs(&volume, &mut reader, &mft_record, NtfsAttributeType::Data)?
            .ok_or(NtfsReaderError::MissingMftAttribute { attribute: "Data" })?;
        let bitmap =
            Self::read_data_fs(&volume, &mut reader, &mft_record, NtfsAttributeType::Bitmap)?
                .ok_or(NtfsReaderError::MissingMftAttribute {
                    attribute: "Bitmap",
                })?;

        Self::from_value_compact(volume, reader, value, bitmap)
    }

    /// A compact store over raw `$MFT` bytes (no fixups) and its bitmap, as [`Self::from_parts`]
    /// takes them: for the tests.
    #[cfg(test)]
    pub(crate) fn from_parts_compact(
        volume: Volume,
        data: Vec<u8>,
        bitmap: Vec<u8>,
    ) -> NtfsReaderResult<Self> {
        let value = MftValue::Resident { data, position: 0 };
        let reader = std::io::Cursor::new(Vec::new());
        Self::from_value_compact(volume, reader, value, bitmap)
    }

    /// [`Self::new_compact`]'s two passes over `value`, the `$MFT` `$DATA`: pass 1 classifies
    /// every record the way pass 2 will (`Self::prepare_record`, the same step
    /// [`Self::from_parts`] and [`MftScan`](crate::MftScan) use) and sums the trimmed bytes a
    /// kept record needs, without keeping any; pass 2 then reserves `bytes`/`offsets` to that
    /// predicted final size and builds the real store. This avoids reallocations on unchanged
    /// input: growing a `Vec` can temporarily require both the old and new allocations. A live
    /// volume can change between passes, so additional bytes and extension-index entries are
    /// also reserved fallibly before appending them.
    fn from_value_compact<R: Read + Seek>(
        volume: Volume,
        mut reader: R,
        mut value: MftValue,
        bitmap: Vec<u8>,
    ) -> NtfsReaderResult<Self> {
        const CHUNK_RECORDS: u64 = 4096;
        let record_size = volume.file_record_size();
        let record_count = value.size() / record_size;
        let offset_entries = checked_compact_entries(record_count)?;
        let mut buf = Vec::new();
        resize_checked(
            &mut buf,
            checked_chunk_bytes(CHUNK_RECORDS.min(record_count), record_size)?,
        )?;

        let mut total_bytes = 0u64;
        let mut next = 0u64;
        while next < record_count {
            let count = CHUNK_RECORDS.min(record_count - next);
            let chunk = &mut buf[..(count * record_size) as usize];
            let read = value.read(&mut reader, chunk)?;
            chunk[read..].fill(0);
            for (index, record) in chunk.chunks_exact_mut(record_size as usize).enumerate() {
                let number = next + index as u64;
                let allocated = Self::bitmap_bit_set(&bitmap, number);
                let prepared = Self::prepare_record(number, record, allocated);
                if Self::keep_in_compact_store(&prepared) {
                    total_bytes += Self::compact_span(record).len() as u64;
                }
            }
            next += count;
        }
        value.rewind();

        let mut bytes = Vec::new();
        reserve_checked(&mut bytes, checked_chunk_bytes(total_bytes, 1)?, true)?;
        let mut offsets = Vec::new();
        reserve_checked(&mut offsets, offset_entries, true)?;
        offsets.push(0u64);
        let mut extension_records = Vec::new();
        let mut freed_extension_records = Vec::new();
        let mut corrupt_records = 0u64;
        let mut stored_bytes = 0u64;

        let mut next = 0u64;
        while next < record_count {
            let count = CHUNK_RECORDS.min(record_count - next);
            let chunk = &mut buf[..(count * record_size) as usize];
            let read = value.read(&mut reader, chunk)?;
            chunk[read..].fill(0);
            for (index, record) in chunk.chunks_exact_mut(record_size as usize).enumerate() {
                let number = next + index as u64;
                let allocated = Self::bitmap_bit_set(&bitmap, number);
                let prepared = Self::prepare_record(number, record, allocated);
                if let Prepared::Record {
                    liveness,
                    extension_of: Some(base),
                    ..
                } = prepared
                {
                    let index = match liveness {
                        Liveness::Live => &mut extension_records,
                        Liveness::Freed => &mut freed_extension_records,
                    };
                    reserve_checked(index, 1, false)?;
                    index.push((base, number));
                }
                if let Prepared::Corrupt = prepared {
                    corrupt_records += u64::from(allocated);
                }
                let span_len = if Self::keep_in_compact_store(&prepared) {
                    let span = Self::compact_span(record);
                    reserve_checked(&mut bytes, span.len(), false)?;
                    bytes.extend_from_slice(span);
                    span.len()
                } else {
                    0
                };
                push_compact_offset(&mut offsets, &mut stored_bytes, span_len);
            }
            next += count;
        }
        drop(buf);
        extension_records.sort_unstable();
        freed_extension_records.sort_unstable();

        Ok(Mft {
            volume,
            data: bytes,
            bitmap,
            record_count,
            extension_records,
            freed_extension_records,
            corrupt_records,
            first: 0,
            side: None,
            offsets: Some(offsets),
            id: MftId::next(),
        })
    }

    /// Whether a compact store keeps `prepared`'s record at all: every record with a valid
    /// header, live, freed, or `Undecided` (in-use flag and `$BITMAP` bit disagree), the same as
    /// [`Self::new`] keeps every valid record's bytes regardless of state. Only `Corrupt` (failed
    /// fixups or header checks: no valid header to trim by) is never kept.
    fn keep_in_compact_store(prepared: &Prepared) -> bool {
        !matches!(prepared, Prepared::Corrupt)
    }

    /// `record`'s bytes trimmed to its header's `used_size` and update sequence array, rounded
    /// up to 8: what a compact
    /// store keeps of a record [`Self::keep_in_compact_store`] says to keep. `record` must have
    /// just passed [`Self::prepare_record`] as `Prepared::Record` or `Prepared::Undecided` (a
    /// valid header, already fixed up), so re-parsing it here to reach `used_size` always
    /// succeeds; `record.len()` (never `Prepared::Corrupt`, which has no header to read) is the
    /// fallback only in case a future caller passes something else.
    fn compact_span(record: &[u8]) -> &[u8] {
        let retained = Record::new(0, record).map_or(record.len(), |r| {
            let usa_end = r.header.update_sequence_offset as usize
                + r.header.update_sequence_length as usize * 2;
            (r.header.used_size as usize)
                .max(usa_end)
                .max(size_of::<NtfsFileRecordHeader>())
        });
        &record[..retained.next_multiple_of(8).min(record.len())]
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
            let start = (number * file_record_size) as usize;
            let record = &mut data[start..start + file_record_size as usize];
            let allocated = Self::bitmap_bit_set(&bitmap, number);
            match Self::prepare_record(number, record, allocated) {
                Prepared::Corrupt => corrupt_records += u64::from(allocated),
                Prepared::Undecided => {}
                Prepared::Record {
                    liveness,
                    extension_of: Some(base),
                    ..
                } => match liveness {
                    Liveness::Live => extension_records.push((base, number)),
                    Liveness::Freed => freed_extension_records.push((base, number)),
                },
                Prepared::Record { .. } => {}
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
            first: 0,
            side: None,
            offsets: None,
            id: MftId::next(),
        })
    }

    /// A chunk view for [`MftScan`](crate::MftScan): no records in the window yet, `side` and
    /// the live and freed extension indexes from pass 1.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn view(
        volume: Volume,
        bitmap: Vec<u8>,
        record_count: u64,
        extension_records: Vec<(u64, u64)>,
        freed_extension_records: Vec<(u64, u64)>,
        corrupt_records: u64,
        side: SideStore,
    ) -> Self {
        Mft {
            volume,
            data: Vec::new(),
            bitmap,
            record_count,
            extension_records,
            freed_extension_records,
            corrupt_records,
            first: record_count,
            side: Some(Box::new(side)),
            offsets: None,
            id: MftId::next(),
        }
    }

    /// Replaces the window with the `count` records from `first`, which `read` fills in raw,
    /// then applies their fixups. A record the side store also holds is overwritten with the
    /// side copy, so the view reads one version of it whichever way it is reached. The window is
    /// empty if `read` fails. `count * file_record_size` must fit this build's own address
    /// space (see [`checked_chunk_bytes`]) and the buffer for it is grown with
    /// `try_reserve_exact` (see [`resize_checked`]), so a `count` that cannot be allocated on
    /// this machine returns [`NtfsReaderError::AllocationTooLarge`] instead of overflowing a
    /// cast or aborting the process.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn load_window(
        &mut self,
        first: u64,
        count: u64,
        read: impl FnOnce(&mut [u8]) -> NtfsReaderResult<()>,
    ) -> NtfsReaderResult<()> {
        let record_size = self.volume.file_record_size();
        let bytes = checked_chunk_bytes(count, record_size)?;
        let record_size = record_size as usize;
        self.first = first;
        resize_checked(&mut self.data, bytes)?;
        if let Err(error) = read(&mut self.data) {
            self.data.clear();
            return Err(error);
        }
        for (index, record) in self.data.chunks_exact_mut(record_size).enumerate() {
            let number = first + index as u64;
            Self::prepare_record(number, record, Self::bitmap_bit_set(&self.bitmap, number));
        }
        if let Some(side) = &self.side {
            for (number, record) in side.records_in(first..first + count) {
                let start = (number - first) as usize * record_size;
                self.data[start..start + record_size].copy_from_slice(record);
            }
        }
        Ok(())
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn side(&self) -> Option<&SideStore> {
        self.side.as_deref()
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn side_mut(&mut self) -> Option<&mut SideStore> {
        self.side.as_deref_mut()
    }

    /// The record numbers in the window: every record for a whole `$MFT`, live or compact.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn window(&self) -> std::ops::Range<u64> {
        if self.offsets.is_some() {
            // A compact store is never partial: it always holds every record slot, trimmed or
            // empty, not a scan's window.
            return 0..self.record_count;
        }
        let count = self.data.len() as u64 / self.volume.file_record_size();
        self.first..self.first + count
    }

    /// Applies `record`'s fixups in place and says what it is: the per-record step of
    /// [`Self::from_parts`], shared with [`MftScan`](crate::MftScan)'s passes. A record failing
    /// its fixups has its signature cleared, so [`Self::record`] never returns it.
    pub(crate) fn prepare_record(number: u64, record: &mut [u8], allocated: bool) -> Prepared {
        if Self::fixup_record(number, record).is_err() {
            record[..FILE_RECORD_SIGNATURE.len()].fill(0);
            return Prepared::Corrupt;
        }

        // A successful fixup only means the update-sequence array matched; `Record::new`
        // still checks the rest of the header (a real "FILE" signature, bounds on
        // `used_size` and `attributes_offset`) before trusting it for anything. A free slot
        // with no valid header is just an empty slot.
        let Some(file) = Record::new(number, record) else {
            return Prepared::Corrupt;
        };
        let Some(liveness) = Liveness::of(&file, allocated) else {
            return Prepared::Undecided;
        };
        // A record whose base reference points at itself is corrupt, not its own extension;
        // indexing it would make NtfsFile::records yield it twice and duplicate its attributes.
        let extension_of = file.base_number().filter(|&base| base != number);
        Prepared::Record {
            liveness,
            extension_of,
            is_directory: file.is_directory(),
        }
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

    /// This load's identity; see [`MftId`].
    pub(crate) fn id(&self) -> MftId {
        self.id
    }

    /// Number of record slots in the `$MFT` (its size divided by the record size), free and
    /// corrupt ones included: one past the highest valid record number. [`Self::record`] and
    /// [`Self::is_allocated`] return `None`/`false` at `record_count()` and above.
    pub fn record_count(&self) -> u64 {
        self.record_count
    }

    /// Size in bytes of what this `Mft` holds in memory: the records (the `$MFT` `$DATA` stream,
    /// trimmed and offset-indexed for [`Self::new_compact`]), the `$MFT` `$BITMAP`, and the two
    /// extension-record indexes (live and freed, 16 bytes an entry). Excludes the [`Volume`] and
    /// its few fixed fields. Path caches and [`ClusterBitmap`](crate::ClusterBitmap)s count
    /// separately.
    pub fn size_in_memory(&self) -> usize {
        let entries = self.extension_records.len() + self.freed_extension_records.len();
        let side = self.side.as_ref().map_or(0, |side| side.size_in_memory());
        let offsets = self
            .offsets
            .as_ref()
            .map_or(0, |offsets| offsets.len() * std::mem::size_of::<u64>());
        self.data.len()
            + self.bitmap.len()
            + entries * std::mem::size_of::<(u64, u64)>()
            + side
            + offsets
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
    pub(crate) fn bitmap_bit_set(bitmap: &[u8], number: u64) -> bool {
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
        let window = self.window();
        (window.start.max(FIRST_NORMAL_RECORD)..window.end)
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
        let window = self.window();
        (window.start.max(FIRST_NORMAL_RECORD)..window.end)
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

    /// The bytes of record `number`: from a compact store's offset table, or the window, else
    /// from a scan's side store. A scan copies its side records over the window (see
    /// `MftScan::next_chunk`), so a record held by both reads the same either way.
    fn record_data(&self, number: u64) -> Option<&[u8]> {
        if let Some(offsets) = &self.offsets {
            let to_byte_start = |offset: &u64| {
                offset
                    .checked_mul(8)
                    .and_then(|bytes| usize::try_from(bytes).ok())
            };
            let start = to_byte_start(offsets.get(number as usize)?)?;
            let end = to_byte_start(offsets.get(number as usize + 1)?)?;
            return (start < end).then(|| &self.data[start..end]);
        }
        let record_size = self.volume.file_record_size();
        if let Some(index) = number.checked_sub(self.first) {
            // Establish membership before forming a byte range: even an unavailable record's
            // end can overflow usize on a 32-bit scan of a large logical MFT.
            if index < self.data.len() as u64 / record_size {
                let start = (index * record_size) as usize;
                return self.data.get(start..start + record_size as usize);
            }
        }
        self.side.as_ref()?.record(number)
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
        NtfsFile::new(self, number, self.record_data(number)?)
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
        match Self::value_fs(volume, reader, record, attribute_type)? {
            None => Ok(None),
            Some(MftValue::Resident { data, .. }) => Ok(Some(data)),
            Some(MftValue::Runs(mut cursor)) => cursor.read_all(reader).map(Some),
        }
    }

    /// [`Self::read_data_fs`] without the read: the value itself if resident, else a
    /// [`RunCursor`] over its validated extents, which [`MftScan`](crate::MftScan) reads a chunk
    /// at a time.
    pub(crate) fn value_fs<R>(
        volume: &Volume,
        reader: &mut R,
        record: &[u8],
        attribute_type: NtfsAttributeType,
    ) -> NtfsReaderResult<Option<MftValue>>
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
            return Ok(Some(MftValue::Resident {
                data: data.to_vec(),
                position: 0,
            }));
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

        RunCursor::new(volume, size, runs)
            .map(MftValue::Runs)
            .map(Some)
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
            RunCursor::new(volume, size, runs)?.read_all(reader)
        }
    }

    /// Reads `size` bytes out of `runs` into one `Vec`: [`RunCursor::read_all`], for the tests.
    #[cfg(test)]
    pub(crate) fn read_runs<R>(
        reader: &mut R,
        volume: &Volume,
        size: u64,
        runs: &[DataRun],
    ) -> NtfsReaderResult<Vec<u8>>
    where
        R: Seek + Read,
    {
        RunCursor::new(volume, size, runs.to_vec())?.read_all(reader)
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

/// Appends the next entry of a compact store's `u64` offset table for a record whose kept span
/// is `span_len` bytes (0 for one [`Mft::keep_in_compact_store`] drops). `total_bytes` is the
/// running count of bytes stored so
/// far, kept by the caller across records instead of read back from the real buffer's `len()`,
/// so this arithmetic can be checked directly against a `total_bytes` no test could actually
/// allocate (see the unit test).
fn push_compact_offset(offsets: &mut Vec<u64>, total_bytes: &mut u64, span_len: usize) {
    *total_bytes += span_len as u64;
    offsets.push(*total_bytes / 8);
}

/// One offset per record plus the final sentinel, including the bytes of the whole table.
fn checked_compact_entries(record_count: u64) -> NtfsReaderResult<usize> {
    let entries = record_count
        .checked_add(1)
        .ok_or(NtfsReaderError::AllocationTooLarge { size: u64::MAX })?;
    checked_chunk_bytes(entries, size_of::<u64>() as u64)?;
    Ok(entries as usize)
}

/// Fallible vector growth, exact for a known final size and amortized when a live volume's
/// second pass needs more than the first predicted. No allocation is attempted within capacity.
fn reserve_checked<T>(buf: &mut Vec<T>, additional: usize, exact: bool) -> NtfsReaderResult<()> {
    if additional <= buf.capacity() - buf.len() {
        return Ok(());
    }
    let size = (buf.len() as u64)
        .saturating_add(additional as u64)
        .saturating_mul(size_of::<T>() as u64);
    #[cfg(test)]
    if allocation_budget::fail() {
        return Err(NtfsReaderError::AllocationTooLarge { size });
    }
    let result = if exact {
        buf.try_reserve_exact(additional)
    } else {
        buf.try_reserve(additional)
    };
    result.map_err(|_| NtfsReaderError::AllocationTooLarge { size })
}

/// Thread-local allocation failure injection: deterministic error paths without exhausting the
/// process allocator or affecting other tests. The guard restores the previous budget on panic.
#[cfg(test)]
mod allocation_budget {
    use std::cell::Cell;

    thread_local! {
        static REMAINING: Cell<Option<usize>> = const { Cell::new(None) };
    }

    pub(super) fn fail() -> bool {
        REMAINING.with(|remaining| match remaining.get() {
            None => false,
            Some(0) => true,
            Some(count) => {
                remaining.set(Some(count - 1));
                false
            }
        })
    }

    pub(super) fn after<T>(successful: usize, run: impl FnOnce() -> T) -> T {
        struct Restore(Option<usize>);
        impl Drop for Restore {
            fn drop(&mut self) {
                REMAINING.set(self.0);
            }
        }
        let _restore = Restore(REMAINING.replace(Some(successful)));
        run()
    }
}

/// `count * record_size` as a `usize`, or [`NtfsReaderError::AllocationTooLarge`] if it cannot:
/// the bound a real allocation of that many bytes would hit on this build's own pointer width,
/// checked here instead of after an unconditional `as usize` cast has already silently wrapped.
/// On a 32-bit build, an unchecked `chunk_records` could make a scan believe it had read a full
/// chunk when the truncated cast had only allocated a much smaller one.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn checked_chunk_bytes(count: u64, record_size: u64) -> NtfsReaderResult<usize> {
    count
        .checked_mul(record_size)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| NtfsReaderError::AllocationTooLarge {
            size: count.saturating_mul(record_size),
        })
}

/// Grows `buf` to exactly `len` bytes, zero filled, reserving with `try_reserve_exact` so a size
/// that cannot be allocated returns an error instead of aborting the process.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn resize_checked(buf: &mut Vec<u8>, len: usize) -> NtfsReaderResult<()> {
    if len > buf.capacity() {
        reserve_checked(buf, len - buf.len(), true)?;
    }
    buf.resize(len, 0);
    Ok(())
}

/// What a record is, as [`Mft::prepare_record`] found it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Prepared {
    /// Failed its fixups or header checks.
    Corrupt,
    /// A valid record whose in-use flag and `$BITMAP` bit disagree.
    Undecided,
    /// A live or freed record.
    Record {
        liveness: Liveness,
        /// The base record number, for an extension record of another record.
        extension_of: Option<u64>,
        /// A base record flagged as a directory.
        is_directory: bool,
    },
}

/// An `$MFT` attribute value as [`Mft::value_fs`] finds it, read front to back.
pub(crate) enum MftValue {
    Resident { data: Vec<u8>, position: usize },
    Runs(RunCursor),
}

#[cfg_attr(not(windows), allow(dead_code))]
impl MftValue {
    pub(crate) fn size(&self) -> u64 {
        match self {
            MftValue::Resident { data, .. } => data.len() as u64,
            MftValue::Runs(cursor) => cursor.size,
        }
    }

    pub(crate) fn rewind(&mut self) {
        match self {
            MftValue::Resident { position, .. } => *position = 0,
            MftValue::Runs(cursor) => cursor.rewind(),
        }
    }

    /// Fills `buf` from where the last read stopped; fewer bytes only at the end.
    pub(crate) fn read<R>(&mut self, reader: &mut R, buf: &mut [u8]) -> NtfsReaderResult<usize>
    where
        R: Seek + Read,
    {
        match self {
            MftValue::Resident { data, position } => {
                let rest = &data[(*position).min(data.len())..];
                let count = rest.len().min(buf.len());
                buf[..count].copy_from_slice(&rest[..count]);
                *position += count;
                Ok(count)
            }
            MftValue::Runs(cursor) => cursor.read(reader, buf),
        }
    }
}

/// `size` bytes out of `runs`, read front to back in pieces of the caller's choosing: sparse
/// runs read as zeroes. [`RunCursor::read_all`] reads the whole value into one `Vec`, which is
/// what `Mft::new` does with `$MFT`'s `$DATA`; [`MftScan`](crate::MftScan) reads it a chunk at a
/// time instead.
pub(crate) struct RunCursor {
    size: u64,
    runs: Vec<DataRun>,
    /// Bytes of the value read so far.
    position: u64,
    /// Current run and its first logical byte, retained across chunk boundaries.
    run_index: usize,
    run_start: u64,
    #[cfg(test)]
    run_visits: usize,
}

impl RunCursor {
    /// `size` comes straight from an on-disk attribute header, checked so far only against the
    /// runs' own declared lengths, which a corrupt or hostile volume can inflate just as cheaply
    /// (a few dozen bytes can claim a 2 GiB `$DATA`). A reader of it would fail
    /// or waste work on a size no genuine stream could reach on its claimed volume, so this
    /// bounds `size` against the volume's own size first. A `volume_size` of 0 (every synthetic
    /// `Volume` in this crate's tests) skips the bound instead of rejecting everything; a real
    /// `Volume::new` fills it in from the boot sector and checks it against the device or image length.
    pub(crate) fn new(volume: &Volume, size: u64, runs: Vec<DataRun>) -> NtfsReaderResult<Self> {
        if volume.volume_size() != 0 && size > volume.volume_size() {
            return Err(NtfsReaderError::InvalidDataRun {
                details: "declared size exceeds the volume's own size",
            });
        }
        Ok(Self {
            size,
            runs,
            position: 0,
            run_index: 0,
            run_start: 0,
            #[cfg(test)]
            run_visits: 0,
        })
    }

    fn rewind(&mut self) {
        self.position = 0;
        self.run_index = 0;
        self.run_start = 0;
    }

    /// The whole value from where the cursor stands (the start, for a new one). `try_reserve`
    /// turns a failed allocation into an error rather than aborting.
    pub(crate) fn read_all<R>(&mut self, reader: &mut R) -> NtfsReaderResult<Vec<u8>>
    where
        R: Seek + Read,
    {
        let size = self.size - self.position;
        let total_size =
            usize::try_from(size).map_err(|_| NtfsReaderError::AllocationTooLarge { size })?;
        let mut data = Vec::new();
        data.try_reserve(total_size)
            .map_err(|_| NtfsReaderError::AllocationTooLarge { size })?;
        data.resize(total_size, 0);
        let read = self.read(reader, &mut data)?;
        data.truncate(read);
        Ok(data)
    }

    /// Fills `buf` from where the last read stopped, seeking to each run in turn; fewer bytes
    /// only at the end of the value, or of the runs if they are shorter (callers check that they
    /// are not).
    pub(crate) fn read<R>(&mut self, reader: &mut R, buf: &mut [u8]) -> NtfsReaderResult<usize>
    where
        R: Seek + Read,
    {
        let wanted = u64::min(buf.len() as u64, self.size.saturating_sub(self.position)) as usize;
        let mut filled = 0usize;
        let mut run_index = self.run_index;
        let mut run_start = self.run_start;
        while filled < wanted && run_index < self.runs.len() {
            #[cfg(test)]
            {
                self.run_visits += 1;
            }
            let run = &self.runs[run_index];
            let (DataRun::Data { length, .. } | DataRun::Sparse { length }) = run;
            let run_end = run_start.saturating_add(*length);
            let at = self.position + filled as u64;
            if at < run_end {
                let within = at - run_start;
                let count = u64::min(run_end - at, (wanted - filled) as u64) as usize;
                let target = &mut buf[filled..filled + count];
                match run {
                    DataRun::Data { offset, .. } => {
                        let offset =
                            offset
                                .checked_add(within)
                                .ok_or(NtfsReaderError::InvalidDataRun {
                                    details: "run offset overflow",
                                })?;
                        reader.seek(SeekFrom::Start(offset))?;
                        reader.read_exact(target)?;
                    }
                    DataRun::Sparse { .. } => target.fill(0),
                }
                filled += count;
            }
            if self.position + filled as u64 >= run_end {
                run_index += 1;
                run_start = run_end;
            }
        }
        self.position += filled as u64;
        self.run_index = run_index;
        self.run_start = run_start;
        Ok(filled)
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
