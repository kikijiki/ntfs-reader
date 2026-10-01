// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! [`MftScan`]: every file of a volume, reading the `$MFT` a chunk at a time instead of holding
//! all of it.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{Read, Seek};
use std::mem::size_of;
use std::ops::Range;
use std::path::PathBuf;

use crate::{
    api::{NtfsAttributeType, NtfsFileName, NtfsFileNamespace, ROOT_RECORD},
    errors::{NtfsReaderError, NtfsReaderResult},
    file::{NtfsFile, Record},
    mft::{checked_chunk_bytes, resize_checked, Liveness, Mft, MftValue, Prepared},
    path::{DeletedPath, DeletedPathCache, PathCache},
    volume::Volume,
};

/// Records per chunk [`MftScan::new`] uses: the window's size, 4 MiB of 1 KiB records.
/// [`MftScan::with_chunk_records`] picks another.
const DEFAULT_CHUNK_RECORDS: u64 = 4096;

/// `Send` so a full-volume scan can move to a background thread (`MftScan` itself is `Send`,
/// not `Sync`: every method takes `&mut self`, so nothing needs to share one by reference across
/// threads at once).
trait ReadSeek: Read + Seek + Send {}
impl<T: Read + Seek + Send> ReadSeek for T {}

/// Every file of a volume, live and deleted, reading the `$MFT` a chunk at a time instead of
/// holding all of it.
///
/// [`MftScan::new`] reads the `$MFT` once (pass 1), keeping what random access needs: the `$MFT`
/// `$BITMAP`, every extension record (live and freed), and the name of every directory (live and
/// freed) in a compact index instead of a whole record. [`MftScan::next_chunk`] then reads it
/// again a chunk at a time (pass 2), each an [`MftChunk`] whose [`MftChunk::files`] and
/// [`MftChunk::deleted_files`] are that chunk's files. Every [`NtfsFile`] accessor and
/// [`FileInfo`](crate::FileInfo) work on them as on a whole [`Mft`]; only [`MftChunk::record`] is
/// narrower.
///
/// ```no_run
/// # use ntfs_reader::{DefaultPathCache, FileInfo, MftScan, Volume};
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let mut scan = MftScan::new(Volume::new(r"\\.\C:")?)?;
/// let mut cache = DefaultPathCache::new();
/// while let Some(chunk) = scan.next_chunk()? {
///     for file in chunk.files() {
///         let info = FileInfo::with_cache(&file, &mut cache);
///     }
/// }
/// # Ok(())
/// # }
/// ```
///
/// On a live volume the two passes can disagree. A kept record (a directory's name, an extension
/// record) is seen as pass 1 read it, in every chunk, even when pass 2 reads it again; anything
/// else as pass 2 read it. So a directory renamed between the passes keeps its old name in paths,
/// a directory absent or unreadable in pass 1 stays missing from paths, including while its
/// record is in the current chunk. The root is also fixed by pass 1. An extension record created
/// after pass 1 is not indexed (a file whose attributes moved to it misses them), and a
/// record reused as a plain file after pass 1 still reads as what pass 1 kept. Nothing loops: each
/// pass reads the `$MFT` front to back once, and path walks detect loops as for a whole [`Mft`].
/// A shadow copy makes both passes read one snapshot, removing this difference.
///
/// Deleted files work the same way as live ones: freed extension records and freed directory
/// names are kept beside the live ones, so [`MftChunk::deleted_files`] and
/// [`MftChunk::resolve_deleted_path`] answer as [`Mft::deleted_files`] and
/// [`Mft::resolve_deleted_path`](crate::Mft::resolve_deleted_path) would. A record reused between
/// the passes (freed then reallocated, or the reverse) is seen as pass 1 kept it if pass 1 kept
/// it, else as pass 2 reads it, the same rule as for live records.
///
/// The declared `$MFT` size is bounded by the file-system size, which [`Volume::new`] checks
/// against the backing device or image file's length. A corrupt boot sector therefore cannot
/// inflate [`Self::record_count`] beyond that bound, even with mostly sparse runs.
pub struct MftScan {
    reader: Box<dyn ReadSeek>,
    value: MftValue,
    view: Mft,
    chunk_records: u64,
    next: u64,
}

impl fmt::Debug for MftScan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MftScan")
            .field("chunk_records", &self.chunk_records)
            .field("next", &self.next)
            .field("record_count", &self.view.record_count())
            .finish_non_exhaustive()
    }
}

impl MftScan {
    /// Opens `volume` and runs pass 1 with 4 MiB chunks. Needs the same elevated access as
    /// [`Mft::new`].
    pub fn new(volume: Volume) -> NtfsReaderResult<Self> {
        Self::with_chunk_records(volume, DEFAULT_CHUNK_RECORDS)
    }

    /// Same as [`Self::new`], with `chunk_records` records per chunk instead of 4096 (at least
    /// 1, and `chunk_records * file_record_size` must fit this build's own address space: a
    /// value that would not is rejected with [`NtfsReaderError::AllocationTooLarge`] here rather
    /// than silently truncated on a 32-bit build, where the scan would otherwise believe it had
    /// read a full chunk when a wrapped cast had only allocated a much smaller one).
    pub fn with_chunk_records(volume: Volume, chunk_records: u64) -> NtfsReaderResult<Self> {
        let mut reader = crate::aligned_reader::open_volume(volume.path())?;
        let record0 = Mft::read_record_fs(
            &mut reader,
            volume.file_record_size(),
            volume.mft_position(),
        )?;
        let value = Mft::value_fs(&volume, &mut reader, &record0, NtfsAttributeType::Data)?
            .ok_or(NtfsReaderError::MissingMftAttribute { attribute: "Data" })?;
        let bitmap = Mft::read_data_fs(&volume, &mut reader, &record0, NtfsAttributeType::Bitmap)?
            .ok_or(NtfsReaderError::MissingMftAttribute {
                attribute: "Bitmap",
            })?;
        Self::from_value(volume, Box::new(reader), value, bitmap, chunk_records)
    }

    /// A scan of raw `$MFT` bytes (no fixups yet) and its bitmap, as [`Mft::from_parts`] takes
    /// them: for the tests.
    #[cfg(test)]
    pub(crate) fn from_parts(
        volume: Volume,
        data: Vec<u8>,
        bitmap: Vec<u8>,
        chunk_records: u64,
    ) -> NtfsReaderResult<Self> {
        let value = MftValue::Resident { data, position: 0 };
        let reader = Box::new(std::io::Cursor::new(Vec::new()));
        Self::from_value(volume, reader, value, bitmap, chunk_records)
    }

    /// Pass 1 over `value`, the `$MFT` `$DATA`.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn from_value(
        volume: Volume,
        mut reader: Box<dyn ReadSeek>,
        mut value: MftValue,
        bitmap: Vec<u8>,
        chunk_records: u64,
    ) -> NtfsReaderResult<Self> {
        let record_size = volume.file_record_size();
        let record_count = value.size() / record_size;
        let chunk_records = chunk_records.max(1);
        let mut side = SideStore::new(record_size as usize);
        let mut extension_records = Vec::new();
        let mut freed_extension_records = Vec::new();
        let mut corrupt_records = 0u64;

        let mut buf = Vec::new();
        let mut next = 0u64;
        while next < record_count {
            let count = chunk_records.min(record_count - next);
            resize_checked(&mut buf, checked_chunk_bytes(count, record_size)?)?;
            read_records(&mut value, &mut reader, &mut buf)?;
            for (index, record) in buf.chunks_exact_mut(record_size as usize).enumerate() {
                let number = next + index as u64;
                let allocated = Mft::bitmap_bit_set(&bitmap, number);
                match Mft::prepare_record(number, record, allocated) {
                    Prepared::Corrupt => corrupt_records += u64::from(allocated),
                    Prepared::Undecided => {
                        // `resolve_path` reads the root's reference whatever state it is in.
                        if number == ROOT_RECORD {
                            side.push(number, record);
                        }
                    }
                    Prepared::Record {
                        liveness,
                        extension_of: Some(base),
                        ..
                    } => {
                        match liveness {
                            Liveness::Live => extension_records.push((base, number)),
                            Liveness::Freed => freed_extension_records.push((base, number)),
                        }
                        side.push(number, record);
                    }
                    Prepared::Record {
                        liveness,
                        extension_of: None,
                        is_directory,
                    } => {
                        if number == ROOT_RECORD {
                            side.push(number, record);
                        } else if is_directory {
                            // A record naming itself as its own base is corrupt.
                            // `push_from_base_record` refuses it (`false`) and it is never
                            // indexed: `Mft::is_directory_parent` refuses it as a parent on a
                            // whole `Mft` too, since a self base reference makes
                            // `NtfsFile::is_extension` true, so indexing it here would only
                            // make a scan resolve a corrupt parent a whole `Mft` does not.
                            side.names.push_from_base_record(number, record, liveness);
                        }
                    }
                }
            }
            next += count;
        }
        drop(buf);
        extension_records.sort_unstable();
        freed_extension_records.sort_unstable();

        let mut view = Mft::view(
            volume,
            bitmap,
            record_count,
            extension_records,
            freed_extension_records,
            corrupt_records,
            side,
        );
        let changes = DirNames::settle(&view);
        if let Some(side) = view.side_mut() {
            side.names.apply(changes);
            side.names.finish();
        }

        value.rewind();
        Ok(Self {
            reader,
            value,
            view,
            chunk_records,
            next: 0,
        })
    }

    /// The next chunk of pass 2, or `None` after the last. An error ends the scan.
    pub fn next_chunk(&mut self) -> NtfsReaderResult<Option<MftChunk<'_>>> {
        let record_count = self.view.record_count();
        if self.next >= record_count {
            return Ok(None);
        }
        let first = self.next;
        let count = self.chunk_records.min(record_count - first);
        // Ends the scan if the read fails: the reader is somewhere in the middle.
        self.next = record_count;
        let (value, reader) = (&mut self.value, &mut self.reader);
        self.view
            .load_window(first, count, |buf| read_records(value, reader, buf))?;
        self.next = first + count;
        Ok(Some(MftChunk(&self.view)))
    }

    /// Number of record slots in the `$MFT`, as [`Mft::record_count`].
    pub fn record_count(&self) -> u64 {
        self.view.record_count()
    }

    /// Number of records skipped as untrustworthy, as [`Mft::corrupt_records`].
    pub fn corrupt_records(&self) -> u64 {
        self.view.corrupt_records()
    }

    /// What the scan holds now: the side store (extension records, the directory name index),
    /// the bitmap, the extension indexes and the window.
    pub fn size_in_memory(&self) -> usize {
        self.view.size_in_memory()
    }
}

/// One chunk of an [`MftScan`]: the files whose base record is in it, and what they can reach.
/// Accessors of its files work as on a whole [`Mft`]: extension records and directory names come
/// from what the scan kept, wherever they are in the `$MFT`.
#[derive(Clone, Copy)]
pub struct MftChunk<'a>(&'a Mft);

impl fmt::Debug for MftChunk<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MftChunk")
            .field("records", &self.records())
            .finish()
    }
}

impl<'a> MftChunk<'a> {
    /// The live files whose base record is in this chunk, in record number order: together, the
    /// chunks of a scan yield what [`Mft::files`] yields.
    pub fn files(&self) -> impl Iterator<Item = NtfsFile<'a>> + use<'a> {
        self.0.files()
    }

    /// The deleted files whose base record is in this chunk, as [`Mft::deleted_files`].
    pub fn deleted_files(&self) -> impl Iterator<Item = NtfsFile<'a>> + use<'a> {
        self.0.deleted_files()
    }

    /// The record numbers this chunk holds.
    pub fn records(&self) -> Range<u64> {
        self.0.window()
    }

    /// The record `number` if this chunk holds it or the scan kept its bytes (an extension
    /// record, the root), else `None`, even for a valid record elsewhere. A directory the scan
    /// indexed is kept by name only ([`Self::resolve_path`]/[`Self::resolve_deleted_path`]), not
    /// by its bytes, so this returns `None` for one outside its own chunk too, the same as for
    /// an unindexed record. An extension record is returned only while its base record is also
    /// available, even in the extension's own chunk: otherwise its logical-file accessors could
    /// not answer. Its retained bytes still contribute to the base's accessors in the base's
    /// chunk.
    pub fn record(&self, number: u64) -> Option<NtfsFile<'a>> {
        let record = self.0.record(number)?;
        if let Some(base) = record.base_number() {
            self.0.record(base).filter(|base| !base.is_extension())?;
        }
        Some(record)
    }

    /// Full path of `name`, as [`Mft::resolve_path`]: through the live directories the scan kept.
    /// Share one cache across the whole scan, not per chunk.
    pub fn resolve_path(&self, name: &NtfsFileName, cache: &mut impl PathCache) -> Option<PathBuf> {
        self.0.resolve_path(name, cache)
    }

    /// Full path of `name`, as [`Mft::resolve_deleted_path`](crate::Mft::resolve_deleted_path):
    /// through the live and freed directories the scan kept.
    pub fn resolve_deleted_path(
        &self,
        name: &NtfsFileName,
        cache: &mut DeletedPathCache,
    ) -> DeletedPath {
        self.0.resolve_deleted_path(name, cache)
    }

    /// The volume being scanned.
    pub fn volume(&self) -> &'a Volume {
        self.0.volume()
    }
}

fn is_win32(name: &NtfsFileName) -> bool {
    matches!(
        name.namespace(),
        Some(NtfsFileNamespace::Win32 | NtfsFileNamespace::Win32AndDos)
    )
}

/// Fills `buf` with the next records of `value`, in full. `buf`'s size is always bounded by
/// `value`'s own declared size (`record_count = value.size() / record_size`), so a read that
/// comes back short here means the run list backing `value` does not actually cover the size it
/// claims (a corrupt or hostile volume, see `RunCursor::new`'s doc comment): an error, not a
/// zero-filled tail, so a caller cannot mistake a truncated chunk for a real, if sparse, one.
fn read_records(
    value: &mut MftValue,
    reader: &mut Box<dyn ReadSeek>,
    buf: &mut [u8],
) -> NtfsReaderResult<()> {
    let read = value.read(reader, buf)?;
    if read < buf.len() {
        return Err(NtfsReaderError::InvalidDataRun {
            details: "$MFT $DATA ended before the record count its own size implies",
        });
    }
    Ok(())
}

/// Records per allocation of a [`SideStore`]: the store grows a block at a time and never copies
/// what it holds.
const BLOCK_RECORDS: usize = 1024;

/// The records a scan keeps for its whole length, beside the window: fixed up, in record number
/// order (every extension record, live and freed, plus the root), plus the directory name index.
pub(crate) struct SideStore {
    record_size: usize,
    numbers: Vec<u64>,
    blocks: Vec<Vec<u8>>,
    names: DirNames,
}

impl SideStore {
    fn new(record_size: usize) -> Self {
        Self {
            record_size,
            numbers: Vec::new(),
            blocks: Vec::new(),
            names: DirNames::default(),
        }
    }

    /// Adds `record`, which must be numbered above every record held.
    fn push(&mut self, number: u64, record: &[u8]) {
        let block_size = BLOCK_RECORDS * self.record_size;
        if self
            .blocks
            .last()
            .is_none_or(|block| block.len() == block_size)
        {
            self.blocks.push(Vec::with_capacity(block_size));
        }
        if let Some(block) = self.blocks.last_mut() {
            block.extend_from_slice(record);
        }
        self.numbers.push(number);
    }

    fn slot(&self, index: usize) -> &[u8] {
        let start = (index % BLOCK_RECORDS) * self.record_size;
        &self.blocks[index / BLOCK_RECORDS][start..start + self.record_size]
    }

    pub(crate) fn record(&self, number: u64) -> Option<&[u8]> {
        let index = self.numbers.binary_search(&number).ok()?;
        Some(self.slot(index))
    }

    /// The records numbered in `range`.
    pub(crate) fn records_in(&self, range: Range<u64>) -> impl Iterator<Item = (u64, &[u8])> {
        let start = self.numbers.partition_point(|&number| number < range.start);
        let end = self.numbers.partition_point(|&number| number < range.end);
        (start..end).map(|index| (self.numbers[index], self.slot(index)))
    }

    pub(crate) fn names(&self) -> Option<&DirNames> {
        Some(&self.names)
    }

    fn records_bytes(&self) -> usize {
        self.blocks.iter().map(Vec::capacity).sum::<usize>()
            + self.numbers.capacity() * size_of::<u64>()
    }

    pub(crate) fn size_in_memory(&self) -> usize {
        self.records_bytes() + self.names.size_in_memory()
    }
}

/// One directory known to a [`DirNames`] index: its stored reference, its name, the parent
/// reference that name holds, and whether it is live or freed. What [`Mft::resolve_path`] and
/// [`Mft::resolve_deleted_path`](crate::Mft::resolve_deleted_path) read of a directory.
pub(crate) struct DirEntry<'a> {
    pub(crate) reference: u64,
    pub(crate) name: &'a OsStr,
    pub(crate) parent: u64,
    pub(crate) liveness: Liveness,
}

/// Directories by record number, live and freed alike, in a compact index instead of whole
/// records. An owned [`OsString`] per directory avoids `unsafe` and costs about 1.2 MiB more on
/// a million-file volume than sharing one byte buffer.
#[derive(Default)]
pub(crate) struct DirNames {
    entries: Vec<DirName>,
}

struct DirName {
    number: u64,
    /// Stored reference: [`Liveness::names`] checks every accepted pre-delete reference,
    /// including the two aliases a freed directory at sequence 1 can have.
    reference: u64,
    liveness: Liveness,
    parent: u64,
    name: OsString,
    /// How far the name is known after reading the base record alone.
    state: Named,
}

/// How far a [`DirName`]'s name is known after reading the base record alone.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Named {
    /// A Win32 name from the base record: its best name, whatever its extension records hold.
    Settled,
    /// The base record's first name, not Win32: an extension record's Win32 name would win.
    Fallback,
    /// No name in the base record (yet): the first one in its extension records, if any.
    Unnamed,
}

impl DirNames {
    fn push(&mut self, number: u64, reference: u64, liveness: Liveness, parent: u64, name: &OsStr) {
        self.entries.push(DirName {
            number,
            reference,
            liveness,
            parent,
            name: name.to_os_string(),
            state: Named::Settled,
        });
    }

    fn rename(&mut self, index: usize, parent: u64, name: &OsStr) {
        let entry = &mut self.entries[index];
        entry.parent = parent;
        entry.name = name.to_os_string();
    }

    /// Indexes the directory in `record` (live or freed, per `liveness`) by what its base record
    /// says of its best name (`best_name` reads the base record first): a Win32 name is final,
    /// anything else is [`Self::settle`]d once the extension records are known. `false` for a
    /// record naming itself as its base: corrupt, and never indexed, since `Mft::is_directory_parent`
    /// refuses such a record as a parent on a whole `Mft` too (its own base reference makes it
    /// `is_extension()`).
    fn push_from_base_record(&mut self, number: u64, record: &[u8], liveness: Liveness) -> bool {
        let Some(record) = Record::new(number, record).filter(|r| r.base_reference().is_none())
        else {
            return false;
        };
        let reference = record.reference();
        let mut first = None;
        for name in record
            .attributes()
            .filter_map(|attribute| attribute.file_name())
        {
            if is_win32(&name) {
                first = Some((name, Named::Settled));
                break;
            }
            first.get_or_insert((name, Named::Fallback));
        }
        match first {
            Some((name, state)) => {
                self.push(
                    number,
                    reference,
                    liveness,
                    name.parent_reference(),
                    &name.to_os_string(),
                );
                if let Some(entry) = self.entries.last_mut() {
                    entry.state = state;
                }
            }
            None => {
                self.push(number, reference, liveness, 0, OsStr::new(""));
                if let Some(entry) = self.entries.last_mut() {
                    entry.state = Named::Unnamed;
                }
            }
        }
        true
    }

    /// Finishes every name not settled by its base record, with `view` holding every live and
    /// freed extension record: the first Win32 name among the ones naming this directory's
    /// current reference, in the order `records` visits them, else the base's fallback, else
    /// their first name. A directory left with no name is dropped, as `best_name` gives it none.
    fn settle(view: &Mft) -> Vec<(usize, Option<(u64, OsString)>)> {
        let Some(names) = view.side().and_then(SideStore::names) else {
            return Vec::new();
        };
        let mut changes = Vec::new();
        for (index, entry) in names.entries.iter().enumerate() {
            if entry.state == Named::Settled {
                continue;
            }
            let extensions = view.extension_records(entry.number, entry.liveness);
            let mut found = None;
            for extension in extensions
                .iter()
                .filter_map(|&(_, number)| view.record(number))
                .filter(|extension| {
                    extension
                        .base_reference()
                        .is_some_and(|reference| entry.liveness.names(reference, entry.reference))
                })
            {
                for name in extension
                    .record_attributes()
                    .filter_map(|attribute| attribute.file_name())
                {
                    if is_win32(&name) {
                        found = Some(name);
                        break;
                    }
                    if entry.state == Named::Unnamed {
                        found.get_or_insert(name);
                    }
                }
                if found.as_ref().is_some_and(is_win32) {
                    break;
                }
            }
            match (found, entry.state) {
                (Some(name), _) => {
                    changes.push((index, Some((name.parent_reference(), name.to_os_string()))))
                }
                (None, Named::Unnamed) => changes.push((index, None)),
                (None, _) => {}
            }
        }
        changes
    }

    /// Applies what [`Self::settle`] found. Drops entries by index in one linear pass
    /// (`unnamed`, one flag per entry). Testing membership in the dropped numbers with
    /// `Vec::contains` for every entry would be `O(entries * unnamed)`: quadratic
    /// on a hostile `$MFT` with many nameless directories (a well-formed volume has at most a
    /// handful).
    fn apply(&mut self, changes: Vec<(usize, Option<(u64, OsString)>)>) {
        let mut unnamed = vec![false; self.entries.len()];
        for (index, change) in changes {
            match change {
                Some((parent, name)) => self.rename(index, parent, &name),
                None => unnamed[index] = true,
            }
        }
        for entry in &mut self.entries {
            entry.state = Named::Settled;
        }
        let mut index = 0;
        self.entries.retain(|_| {
            let keep = !unnamed[index];
            index += 1;
            keep
        });
    }

    fn finish(&mut self) {
        self.entries.sort_unstable_by_key(|entry| entry.number);
        self.entries.shrink_to_fit();
    }

    /// The directory numbered `number`.
    pub(crate) fn get(&self, number: u64) -> Option<DirEntry<'_>> {
        let index = self
            .entries
            .binary_search_by_key(&number, |entry| entry.number)
            .ok()?;
        let entry = &self.entries[index];
        Some(DirEntry {
            reference: entry.reference,
            name: entry.name.as_os_str(),
            parent: entry.parent,
            liveness: entry.liveness,
        })
    }

    fn size_in_memory(&self) -> usize {
        self.entries.capacity() * size_of::<DirName>()
            + self
                .entries
                .iter()
                .map(|entry| entry.name.len())
                .sum::<usize>()
    }
}

#[cfg(test)]
#[path = "tests/scan.rs"]
mod tests;
