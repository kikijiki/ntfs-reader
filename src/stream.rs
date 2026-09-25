// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! [`StreamReader`]: `Read + Seek` over one data stream of a file, read from the raw volume.

use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::{
    api::NtfsAttributeType,
    attribute::NtfsAttribute,
    data_run::DataRun,
    errors::{NtfsReaderError, NtfsReaderResult},
    file::NtfsFile,
};

/// Attribute header flags: the low byte is the compression format (0 is none, 1 is LZNT1) and
/// 0x4000 is EFS encryption. 0x8000 (sparse) needs no handling: the runs say which parts are.
const COMPRESSION_MASK: u16 = 0x00ff;
const ENCRYPTED: u16 = 0x4000;
/// The stream where the WOF filter keeps the contents of a file it compressed (CompactOS).
const WOF_STREAM: &str = "WofCompressedData";

/// Where the bytes of a [`StreamExtent`] are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExtentLocation {
    /// Stored in the file's MFT record, not in clusters: the whole stream, when it is small.
    Resident,
    /// Stored on the volume.
    Volume {
        /// Byte offset from the start of the volume.
        offset: u64,
    },
    /// Not stored: reads as zeroes.
    Sparse,
    /// The record holding this part's extent was not found, so nothing says where its bytes are.
    /// Reading it fails with [`NtfsReaderError::StreamExtentMissing`]: recovery of a deleted file
    /// can be partial when an extension record was freed and reused. Distinct from lost data
    /// ([`StreamReader::data_lost`]), a stream whose runs NTFS wiped entirely.
    Missing,
}

/// A stretch of a stream and where its bytes are, in bytes. See [`StreamReader::extents`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct StreamExtent {
    /// Offset of the stretch from the start of the stream.
    pub stream_offset: u64,
    /// Length of the stretch.
    pub length: u64,
    /// Where its bytes are.
    pub location: ExtentLocation,
}

/// The raw volume, opened lazily on first read or seek: a stream with nothing in clusters
/// (resident, sparse, empty, lost) never needs elevated access. Reads must be sector aligned; a
/// read crossing the volume's end is short, one wholly past it returns nothing. [`Core`] handles
/// both.
pub(crate) struct LazyVolume {
    path: PathBuf,
    reader: Option<File>,
}

impl LazyVolume {
    fn new(path: &Path) -> Self {
        LazyVolume {
            path: path.to_path_buf(),
            reader: None,
        }
    }

    fn reader(&mut self) -> io::Result<&mut File> {
        match &mut self.reader {
            Some(reader) => Ok(reader),
            empty => Ok(empty.insert(File::open(&self.path)?)),
        }
    }
}

impl Read for LazyVolume {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader()?.read(buf)
    }
}

impl Seek for LazyVolume {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.reader()?.seek(pos)
    }
}

/// Reads and seeks one data stream of a file, from the raw volume: a locked file reads like any
/// other, and no `Vec` of the whole stream is built. Get one from [`NtfsFile::open_stream`].
///
/// - Sparse parts, and bytes at or beyond [`Self::initialized_size`], read as zeroes.
/// - Reading never goes past [`Self::size`]; a seek past it reads nothing.
/// - A part whose extent record is missing is an [`ExtentLocation::Missing`] extent: reading it
///   returns the bytes before it, then fails with [`NtfsReaderError::StreamExtentMissing`] inside
///   an `InvalidData` [`io::Error`] (unwrap it with [`io::Error::get_ref`] and a downcast, as
///   below). Seek past it to continue.
/// - Volume reads stop at the file system's size, a few KiB short of a volume handle's readable
///   end on Windows (readable only to the end of its last whole cluster). A read crossing that
///   end is short, one wholly past it returns 0 bytes; harmless, since a stream's data is whole
///   clusters.
/// - The reader owns its volume handle instead of borrowing the [`Mft`](crate::Mft), and opens it
///   lazily, on the first read of clustered bytes. A stream with none (resident, sparse, empty,
///   lost) never opens it.
///
/// Reads go through a normal volume handle, so they can return stale, cached data: a file written
/// moments ago may read old bytes until the volume is flushed ([`Mft::new`](crate::Mft::new) does
/// not invalidate the cache). After a delete or TRIM the reader can still return bytes the cache
/// holds after the disk has zeroes (measured once on a virtual disk: a raw
/// `FILE_FLAG_NO_BUFFERING` read saw zeroes at 10 s, this reader held old bytes at 90 s); there is
/// no reliable fix, so treat the bytes as what the cache had. For a deleted file the clusters
/// read may already be reused; the reader does not check.
///
/// It caches the last span read from the volume (up to 256 KiB, aligned sectors, never past the
/// volume's end) and serves reads inside it from memory, even after seeking back in: they return
/// what the volume held when the span was read, not what is there now.
///
/// ```no_run
/// use std::io::Read;
/// use ntfs_reader::{Mft, NtfsReaderError, Volume};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let mft = Mft::new(Volume::new(r"\\.\C:")?)?;
/// for file in mft.deleted_files().filter(|file| !file.is_directory()).take(10) {
///     // A deleted file may have no default stream to open: skip it.
///     let Ok(mut stream) = file.open_stream(None) else {
///         continue;
///     };
///     let mut recovered = Vec::new();
///     if let Err(err) = stream.read_to_end(&mut recovered) {
///         // The bytes before the missing part are in `recovered`.
///         let missing_at = err
///             .get_ref()
///             .and_then(|inner| inner.downcast_ref::<NtfsReaderError>())
///             .and_then(|error| match error {
///                 NtfsReaderError::StreamExtentMissing { offset } => Some(*offset),
///                 _ => None,
///             });
///         match missing_at {
///             Some(offset) => println!("recovered {offset} bytes, then an extent is missing"),
///             None => return Err(err.into()),
///         }
///     }
/// }
/// # Ok(())
/// # }
/// ```
pub struct StreamReader {
    core: Core<LazyVolume>,
    volume_path: PathBuf,
}

impl fmt::Debug for StreamReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.core.fmt(f)
    }
}

impl StreamReader {
    /// The logical size of the stream in bytes.
    pub fn size(&self) -> u64 {
        self.core.size
    }

    /// How much of the stream was ever written: bytes at or beyond this offset read as zeroes,
    /// whatever the clusters hold. Never more than [`Self::size`]; equal to it for a resident
    /// stream.
    pub fn initialized_size(&self) -> u64 {
        self.core.initialized_size
    }

    /// Where the bytes of the stream are, in order: the extents cover the stream from 0 to
    /// [`Self::size`] without gaps or overlaps. A resident stream is one
    /// [`ExtentLocation::Resident`] extent, an empty one has none. Adjacent holes merge into one
    /// extent; other adjacent runs do not merge. Nothing here reads the volume.
    pub fn extents(&self) -> &[StreamExtent] {
        &self.core.extents
    }

    /// The volume's cluster size in bytes, the unit the runs behind [`Self::extents`] are made of.
    pub fn cluster_size(&self) -> u64 {
        self.core.cluster_size
    }

    /// Whether this stream's data was lost: the file is deleted, had an `$ATTRIBUTE_LIST`, and
    /// this stream is not resident. NTFS was seen to zero the size and data runs of such a
    /// stream, so it opens as an empty one ([`Self::size`] 0, no extents), indistinguishable from
    /// a stream that really was empty except by this flag. A resident stream is never lost. See
    /// [`NtfsFile::stream_data_lost`].
    pub fn data_lost(&self) -> bool {
        self.core.data_lost
    }

    /// The path of the volume the stream is on, as the [`Mft`](crate::Mft) it came from has it.
    pub(crate) fn volume_path(&self) -> &Path {
        &self.volume_path
    }
}

impl Read for StreamReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.core.read(buf)
    }
}

impl Seek for StreamReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.core.seek(pos)
    }
}

impl NtfsFile<'_> {
    /// Opens a data stream of this file for reading: `None` is the default (unnamed) stream, the
    /// file's contents; `Some(name)` an alternate data stream (see [`Self::data_streams`] for the
    /// names). The name must match exactly, case included.
    ///
    /// Reads the stream from the raw volume, resident or not, fragmented or sparse, whether or
    /// not Windows has the file open, so it needs the same elevated access as [`Mft::new`]. See
    /// [`StreamReader`] for when it opens the volume, and for its cached, possibly stale reads.
    ///
    /// The extents come from [`Self::records`], the base record and its extension records, so
    /// this works on any file the accessors work on.
    ///
    /// Errors:
    ///
    /// - [`NtfsReaderError::StreamNotFound`]: no stream of that name.
    /// - [`NtfsReaderError::CompressedStream`] and [`NtfsReaderError::EncryptedStream`]: the bytes
    ///   on the volume are not the contents, and this crate neither decompresses nor decrypts.
    ///   Sparse streams are fine.
    /// - [`NtfsReaderError::InvalidDataRun`]: the runs are corrupt, run outside the volume, the
    ///   extents overlap or have no VCN-0 extent (so the size is unknown), or there are more than
    ///   4,194,304 (2^22) extents. An absent extent is not an error: it is an
    ///   [`ExtentLocation::Missing`] extent.
    /// - [`NtfsReaderError::WofCompressedStream`]: the default stream of a CompactOS (WOF)
    ///   compressed file is a sparse placeholder reading as zeroes; its contents are in the named
    ///   stream `WofCompressedData`, opened by name (raw compressed bytes, not decompressed by
    ///   this crate). Recognised by that stream, not by its reparse tag.
    ///
    /// Placeholders that are **not** detected read as zeroes or junk, not as an error: a file
    /// optimised by Data Deduplication (contents in the chunk store), a Cloud Files placeholder
    /// (OneDrive and other sync providers), and an HSM stub all read as they are on the volume.
    /// They carry a reparse point (`FILE_ATTRIBUTE_REPARSE_POINT` in
    /// [`FileInfo::file_attributes`](crate::FileInfo)), which is how to spot them, but symbolic
    /// links and junctions have one too.
    ///
    /// ```no_run
    /// use std::io::Read;
    /// use ntfs_reader::{Mft, Volume};
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let mft = Mft::new(Volume::new(r"\\.\C:")?)?;
    /// for file in mft.files().filter(|file| !file.is_directory()).take(10) {
    ///     let mut stream = file.open_stream(None)?;
    ///     let mut head = Vec::new();
    ///     stream.by_ref().take(16).read_to_end(&mut head)?;
    ///     println!("{} bytes, starts with {head:02x?}", stream.size());
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// [`Mft::new`]: crate::Mft::new
    pub fn open_stream(&self, name: Option<&OsStr>) -> NtfsReaderResult<StreamReader> {
        let volume_path = self.mft().volume().path();
        open(self, name, LazyVolume::new(volume_path)).map(|core| StreamReader {
            core,
            volume_path: volume_path.to_path_buf(),
        })
    }
}

/// [`NtfsFile::open_stream`] with the volume supplied as `volume`, which a resident stream drops
/// unused. Tests give it a `Cursor` over a synthetic volume.
pub(crate) fn open<R>(
    file: &NtfsFile<'_>,
    name: Option<&OsStr>,
    volume: R,
) -> NtfsReaderResult<Core<R>> {
    let Layout {
        size,
        initialized_size,
        extents,
        resident,
        data_lost,
    } = Layout::new(file, name)?;
    let backing = match resident {
        Some(bytes) => Backing::Resident(bytes),
        None => Backing::Volume {
            reader: volume,
            at: None,
            scratch: Scratch::new(),
            window: Window::default(),
        },
    };
    Ok(Core {
        size,
        initialized_size,
        cluster_size: file.mft().volume().cluster_size(),
        volume_size: file.mft().volume().volume_size(),
        data_lost,
        extents,
        backing,
        position: 0,
    })
}

/// The most extents a stream may have.
const MAX_EXTENTS: usize = 1 << 22;

/// The extents of a stream, laid out end to end. A hole following a hole is the same hole, and no
/// stream has more than `max` extents, so a corrupt volume's runs cost memory in proportion to
/// what they claim, not to what the volume can hold.
struct ExtentList {
    extents: Vec<StreamExtent>,
    max: usize,
}

impl ExtentList {
    fn new(max: usize) -> Self {
        ExtentList {
            extents: Vec::new(),
            max,
        }
    }

    fn push(&mut self, extent: StreamExtent) -> NtfsReaderResult<()> {
        if let Some(last) = self.extents.last_mut() {
            let continues =
                last.stream_offset.checked_add(last.length) == Some(extent.stream_offset);
            if continues
                && last.location == ExtentLocation::Sparse
                && extent.location == ExtentLocation::Sparse
            {
                last.length = last
                    .length
                    .checked_add(extent.length)
                    .ok_or(invalid("total run length overflow"))?;
                return Ok(());
            }
        }
        if self.extents.len() >= self.max {
            return Err(invalid("the stream has too many extents"));
        }
        self.extents.push(extent);
        Ok(())
    }

    fn into_vec(self) -> Vec<StreamExtent> {
        self.extents
    }
}

/// What a stream is made of, worked out from the file's records and before touching the volume.
struct Layout {
    size: u64,
    initialized_size: u64,
    extents: Vec<StreamExtent>,
    /// The bytes of a resident stream.
    resident: Option<Vec<u8>>,
    /// See [`StreamReader::data_lost`].
    data_lost: bool,
}

fn is_stream(attribute: &NtfsAttribute<'_>, name: Option<&OsStr>) -> bool {
    if attribute.attribute_type() != Some(NtfsAttributeType::Data) {
        return false;
    }
    match name {
        None => attribute.header.name_length == 0,
        Some(name) => attribute
            .name()
            .is_some_and(|attribute_name| attribute_name == name),
    }
}

fn invalid(details: &'static str) -> NtfsReaderError {
    NtfsReaderError::InvalidDataRun { details }
}

impl Layout {
    fn new(file: &NtfsFile<'_>, name: Option<&OsStr>) -> NtfsReaderResult<Self> {
        // A WOF file's default stream is a sparse placeholder of the right size; silently
        // returning it would be wrong data. Independent of the reparse tag.
        if name.is_none()
            && file.attributes().any(|attribute| {
                attribute.attribute_type() == Some(NtfsAttributeType::Data)
                    && attribute.name().is_some_and(|name| name == WOF_STREAM)
            })
        {
            return Err(NtfsReaderError::WofCompressedStream);
        }

        let mut attributes: Vec<NtfsAttribute<'_>> = file
            .attributes()
            .filter(|attribute| is_stream(attribute, name))
            .collect();
        if attributes.is_empty() {
            return Err(NtfsReaderError::StreamNotFound {
                name: name.map(OsStr::to_os_string),
            });
        }

        // Resident data is never compressed, whatever the flag says (a small file in a
        // compressed directory carries it), but it can be encrypted.
        for attribute in &attributes {
            let flags = attribute.header.flags;
            if flags & ENCRYPTED != 0 {
                return Err(NtfsReaderError::EncryptedStream);
            }
            if !attribute.is_resident() && flags & COMPRESSION_MASK != 0 {
                return Err(NtfsReaderError::CompressedStream);
            }
        }

        if attributes.iter().any(NtfsAttribute::is_resident) {
            let [attribute] = attributes.as_slice() else {
                return Err(invalid("resident stream is not the only attribute"));
            };
            let bytes = attribute
                .resident()
                .ok_or(invalid("resident attribute missing value"))?;
            let size = bytes.len() as u64;
            let extents = if size == 0 {
                Vec::new()
            } else {
                vec![StreamExtent {
                    stream_offset: 0,
                    length: size,
                    location: ExtentLocation::Resident,
                }]
            };
            return Ok(Layout {
                size,
                initialized_size: size,
                extents,
                resident: Some(bytes.to_vec()),
                data_lost: false,
            });
        }

        Self::from_extents(file, &mut attributes)
    }

    /// Lays the non-resident extents of one stream end to end. Only the VCN-0 extent carries a
    /// size, so without it nothing bounds the stream. Each extent must start where the previous
    /// one ends: a gap is a stretch whose extent record was not found, and becomes a `Missing`
    /// extent (also when the extents stop short of the size); an overlap is corrupt.
    fn from_extents(
        file: &NtfsFile<'_>,
        attributes: &mut [NtfsAttribute<'_>],
    ) -> NtfsReaderResult<Self> {
        let volume = file.mft().volume();
        let cluster_size = volume.cluster_size();
        let volume_size = volume.volume_size();

        attributes.sort_by_key(|attribute| {
            attribute
                .nonresident_header()
                .map_or(i64::MAX, |header| header.lowest_vcn)
        });

        let first = attributes[0]
            .nonresident_header()
            .filter(|header| header.lowest_vcn == 0)
            .ok_or(invalid("missing VCN-0 extent"))?;
        let size = first.data_size;
        // Written data never passes the size; a header that says it does is corrupt, not a
        // reason to read past the end.
        let initialized_size = first.initialized_size.min(size);

        let mut extents = ExtentList::new(MAX_EXTENTS);
        // The end of everything laid out so far, before cutting it at `size`.
        let mut covered = 0u64;
        let push = |extents: &mut ExtentList, offset: u64, length: u64, location| {
            if offset < size {
                extents.push(StreamExtent {
                    stream_offset: offset,
                    length: length.min(size - offset),
                    location,
                })
            } else {
                Ok(())
            }
        };

        for attribute in attributes.iter() {
            let header = attribute
                .nonresident_header()
                .ok_or(invalid("resident stream is not the only attribute"))?;
            let (lowest_vcn, highest_vcn) = (header.lowest_vcn, header.highest_vcn);
            let start = u64::try_from(lowest_vcn)
                .ok()
                .and_then(|vcn| vcn.checked_mul(cluster_size))
                .ok_or(invalid("extent starts at an invalid VCN"))?;

            let runs = attribute.nonresident_extent_runs(volume)?;
            let run_bytes = runs
                .iter()
                .try_fold(0u64, |total, run| total.checked_add(run.length()))
                .ok_or(invalid("total run length overflow"))?;
            // What the header claims must match what the runs cover. An empty run list passes:
            // an empty extent's claim is no reason to reject the stream.
            let claimed = i128::from(highest_vcn) - i128::from(lowest_vcn) + 1;
            if !runs.is_empty() && claimed != i128::from(run_bytes / cluster_size) {
                return Err(invalid("extent's runs do not match its VCN range"));
            }

            let end = start
                .checked_add(run_bytes)
                .ok_or(invalid("total run length overflow"))?;
            if start < covered {
                return Err(invalid("extents overlap in VCN order"));
            }
            if start > covered {
                push(
                    &mut extents,
                    covered,
                    start - covered,
                    ExtentLocation::Missing,
                )?;
                covered = start;
            }
            for run in runs {
                let location = match run {
                    DataRun::Data { offset, length } => {
                        // A run outside the volume is corrupt (or a deleted file's junk) and must
                        // not become a read there. 0 means the size is unknown.
                        if volume_size != 0
                            && offset
                                .checked_add(length)
                                .is_none_or(|end| end > volume_size)
                        {
                            return Err(invalid("data run lies outside the volume"));
                        }
                        ExtentLocation::Volume { offset }
                    }
                    DataRun::Sparse { .. } => ExtentLocation::Sparse,
                };
                push(&mut extents, covered, run.length(), location)?;
                covered += run.length();
            }
            debug_assert_eq!(covered, end);
        }
        if covered < size {
            push(
                &mut extents,
                covered,
                size - covered,
                ExtentLocation::Missing,
            )?;
        }

        Ok(Layout {
            size,
            initialized_size,
            extents: extents.into_vec(),
            resident: None,
            data_lost: file.stream_data_lost(),
        })
    }
}

/// Where a [`Core`] gets bytes from.
enum Backing<R> {
    Resident(Vec<u8>),
    Volume {
        reader: R,
        /// Where `reader` is, if known, so that reading on from the previous read costs no seek.
        at: Option<u64>,
        scratch: Scratch,
        /// What `scratch` holds.
        window: Window,
    },
}

/// Which bytes of the volume the [`Scratch`] holds: `len` bytes from byte `start` of the volume.
#[derive(Default, Clone, Copy)]
struct Window {
    start: u64,
    len: usize,
}

impl Window {
    /// The part of the window from volume byte `offset` on, as `(index into the scratch, bytes)`.
    fn from(&self, offset: u64) -> Option<(usize, usize)> {
        let index = usize::try_from(offset.checked_sub(self.start)?).ok()?;
        (index < self.len).then(|| (index, self.len - index))
    }
}

/// The one buffer the volume is read into, and the window it currently holds. Windows refuses a
/// raw volume read unless it starts at a sector-size multiple and covers a whole number of
/// sectors, so bytes arrive in aligned spans of at least [`Self::READ_AHEAD`], into this
/// page-aligned buffer, and are copied out to the caller's own (unaligned) buffer. Reads inside
/// the window cost no volume read.
struct Scratch {
    bytes: Vec<u8>,
    start: usize,
}

impl Scratch {
    /// The largest span read from the volume at once.
    const SIZE: usize = 256 * 1024;
    /// The alignment of the offset and length of every volume read, and of the buffer: the page
    /// size, a multiple of every sector size.
    const ALIGNMENT: usize = 4096;
    /// The least read from the volume when a read misses the window, so that reading a stream in
    /// small pieces costs a read per 64 KiB, not per piece.
    const READ_AHEAD: usize = 64 * 1024;

    /// Allocates nothing until the first [`Self::take`]: a stream that never reads the volume
    /// costs no 256 KiB.
    fn new() -> Self {
        Scratch {
            bytes: Vec::new(),
            start: 0,
        }
    }

    /// At most `length` bytes of aligned memory.
    fn take(&mut self, length: usize) -> &mut [u8] {
        if self.bytes.is_empty() {
            self.bytes = vec![0u8; Self::SIZE + Self::ALIGNMENT];
            // The heap block does not move when the `Vec` does, so `start` stays right.
            self.start = self.bytes.as_ptr().align_offset(Self::ALIGNMENT);
            // `align_offset` may give up (usize::MAX) in theory; a start past the slack would
            // make `take` index out of the buffer.
            assert!(
                self.start <= Self::ALIGNMENT,
                "the scratch buffer cannot be aligned"
            );
        }
        let length = length.min(Self::SIZE);
        &mut self.bytes[self.start..self.start + length]
    }

    /// The bytes of the window, from `index`.
    fn window(&self, index: usize, len: usize) -> &[u8] {
        &self.bytes[self.start + index..self.start + index + len]
    }

    /// Reads the volume from byte `start` (a multiple of [`Self::ALIGNMENT`]) for `length` bytes
    /// into the buffer, and returns how many arrived. Short reads are continued, an end of file
    /// ends it, and a failure after some bytes arrived is left for the next read to meet again.
    fn fill<R: Read + Seek>(
        &mut self,
        reader: &mut R,
        at: &mut Option<u64>,
        start: u64,
        length: usize,
    ) -> io::Result<usize> {
        if *at != Some(start) {
            *at = None;
            reader.seek(SeekFrom::Start(start))?;
        }
        let landing = self.take(length);
        let mut filled = 0;
        while filled < length {
            match reader.read(&mut landing[filled..]) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) if filled == 0 => {
                    *at = None;
                    return Err(err);
                }
                Err(_) => {
                    *at = None;
                    return Ok(filled);
                }
            }
        }
        *at = Some(start + filled as u64);
        Ok(filled)
    }
}

/// The reading itself, over any volume reader so that the unit tests can use a `Cursor`.
pub(crate) struct Core<R> {
    pub(crate) size: u64,
    pub(crate) initialized_size: u64,
    cluster_size: u64,
    /// The size of the volume, or 0 if it is not known: no read goes past it.
    volume_size: u64,
    /// See [`StreamReader::data_lost`].
    data_lost: bool,
    pub(crate) extents: Vec<StreamExtent>,
    backing: Backing<R>,
    position: u64,
}

impl<R> fmt::Debug for Core<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamReader")
            .field("size", &self.size)
            .field("initialized_size", &self.initialized_size)
            .field("extents", &self.extents.len())
            .field("position", &self.position)
            .finish_non_exhaustive()
    }
}

/// How many bytes from volume byte `offset` (where stream extent `index` holds `within`) lie on
/// the volume without a break: the rest of that extent, plus the extents after it as long as each
/// starts where the last one ended. Capped at [`Scratch::READ_AHEAD`].
fn contiguous_run(extents: &[StreamExtent], index: usize, within: u64, offset: u64) -> u64 {
    let limit = Scratch::READ_AHEAD as u64;
    let mut run = extents[index].length - within;
    for next in &extents[index + 1..] {
        match next.location {
            ExtentLocation::Volume { offset: start }
                if run < limit && offset.checked_add(run) == Some(start) =>
            {
                run = run.saturating_add(next.length);
            }
            _ => break,
        }
    }
    run
}

impl<R: Read + Seek> Core<R> {
    /// Reads from the extent at the position, without crossing into the next one or past the
    /// initialized size. At least one byte, or an error, for a non-empty `buf` that starts before
    /// the end of the stream.
    fn read_extent(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let position = self.position;
        let index = self
            .extents
            .partition_point(|extent| extent.stream_offset + extent.length <= position);
        let Some(&extent) = self.extents.get(index) else {
            return Err(io::Error::other("stream position is outside every extent"));
        };
        let within = position - extent.stream_offset;
        let mut length = (extent.length - within).min(buf.len() as u64);
        let written = position < self.initialized_size;
        if written {
            length = length.min(self.initialized_size - position);
        }
        // `length` is at most `buf.len()`.
        let buf = &mut buf[..length as usize];

        if !written {
            buf.fill(0);
            return Ok(buf.len());
        }
        match (extent.location, &mut self.backing) {
            (ExtentLocation::Sparse, _) => {
                buf.fill(0);
                Ok(buf.len())
            }
            (ExtentLocation::Missing, _) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                NtfsReaderError::StreamExtentMissing { offset: position },
            )),
            (ExtentLocation::Resident, Backing::Resident(bytes)) => {
                let bytes = usize::try_from(within)
                    .ok()
                    .and_then(|start| bytes.get(start..start.checked_add(buf.len())?))
                    .ok_or_else(|| io::Error::other("resident extent is longer than its bytes"))?;
                buf.copy_from_slice(bytes);
                Ok(buf.len())
            }
            (
                ExtentLocation::Volume { offset: base },
                Backing::Volume {
                    reader,
                    at,
                    scratch,
                    window,
                },
            ) => {
                let offset = base
                    .checked_add(within)
                    .ok_or_else(|| io::Error::other("run offset overflow"))?;
                if window.from(offset).is_none() {
                    // Aligned down to the byte's block, and up to a whole number of blocks, but
                    // never past the file system size (a few KiB beyond the readable cluster area
                    // is harmless: such a read is short, not an error). The volume size is a
                    // whole number of sectors, so the last span is too.
                    let alignment = Scratch::ALIGNMENT as u64;
                    let start = offset / alignment * alignment;
                    if self.volume_size != 0 && offset >= self.volume_size {
                        return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                    }
                    let before = offset - start;
                    let wanted = before.saturating_add(buf.len() as u64);
                    // Read ahead only over what is there: to the end of the extent, then into the
                    // next ones only where each starts where the last ended. A fragmented file
                    // must not pay `READ_AHEAD` per fragment.
                    let run = contiguous_run(&self.extents, index, within, offset)
                        .min(self.initialized_size - position);
                    let ahead = before.saturating_add(run.min(Scratch::READ_AHEAD as u64));
                    let mut length = wanted
                        .max(ahead)
                        .min(Scratch::SIZE as u64)
                        .next_multiple_of(alignment);
                    if self.volume_size != 0 {
                        length = length.min(self.volume_size - start);
                    }
                    *window = Window { start, len: 0 };
                    let filled = scratch.fill(reader, at, start, length as usize)?;
                    *window = Window { start, len: filled };
                }
                let Some((index, available)) = window.from(offset) else {
                    // The volume ended before the byte.
                    return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                };
                let count = buf.len().min(available);
                buf[..count].copy_from_slice(scratch.window(index, count));
                Ok(count)
            }
            _ => Err(io::Error::other(
                "extent does not match where the stream is kept",
            )),
        }
    }
}

impl<R: Read + Seek> Read for Core<R> {
    /// Fills `buf` across extents. A failure after some bytes were read is not reported: those
    /// bytes are returned, and the next read meets the failure again.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut done = 0;
        while done < buf.len() && self.position < self.size {
            let left = self.size - self.position;
            let end = done + (buf.len() - done).min(usize::try_from(left).unwrap_or(usize::MAX));
            match self.read_extent(&mut buf[done..end]) {
                Ok(0) => break,
                Ok(read) => {
                    done += read;
                    self.position += read as u64;
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(_) if done > 0 => break,
                Err(err) => return Err(err),
            }
        }
        Ok(done)
    }
}

impl<R: Read + Seek> Seek for Core<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(offset) => i128::from(offset),
            SeekFrom::End(delta) => i128::from(self.size) + i128::from(delta),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
        };
        self.position = u64::try_from(target).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek to a negative or overflowing position",
            )
        })?;
        Ok(self.position)
    }
}

#[cfg(test)]
#[path = "tests/stream.rs"]
mod tests;
