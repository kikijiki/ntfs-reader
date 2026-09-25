// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! [`ClusterBitmap`]: which clusters of a volume are allocated, and [`StreamAllocation`]: what
//! that says about where a stream's bytes are. Opt in, separate from [`Mft`].

use std::fmt;
use std::io::Read;
use std::path::PathBuf;

use crate::{
    errors::{NtfsReaderError, NtfsReaderResult},
    file::NtfsFile,
    mft::Mft,
    stream::{ExtentLocation, StreamExtent, StreamReader},
    volume::Volume,
};

/// The record of `$Bitmap`, the file that holds the volume's cluster bitmap.
const BITMAP_RECORD: u64 = 6;

/// The most bytes of a bitmap reserved before any of them has been read: 64 MiB, the bitmap of a
/// 2 TiB volume with 4 KiB clusters.
const RESERVE_LIMIT: u64 = 64 << 20;

/// The largest bitmap read: 4 GiB, the bitmap of 2^35 clusters (128 PiB at 4 KiB clusters), 8x the
/// bitmap of the largest volume NTFS on Windows formats (2^32 clusters, 512 MiB). A volume size
/// that asks for more is a corrupt boot sector, not a volume.
const MAX_BITMAP: u64 = 4 << 30;

/// How much to reserve up front for a bitmap of `needed` bytes: `needed`, but at most `limit`.
fn reservation(needed: u64, limit: u64) -> usize {
    usize::try_from(needed.min(limit)).unwrap_or(usize::MAX)
}

// The largest capacity the buffer of `read_bits` had after a reservation, on this thread: what
// the tests look at to see how much a read reserved before the bytes arrived.
#[cfg(test)]
thread_local! {
    static LARGEST_CAPACITY: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Exactly `needed` bytes from `reader`. The first reservation is `reservation(needed, reserve)`;
/// after that the buffer grows by what it already holds, by at least `reserve` and no more than
/// what's missing, each step reserved only once the last one is filled. A hostile size costs only
/// what arrives (plus the first reservation); a bitmap read in full ends with a capacity of
/// exactly `needed`.
fn read_bits(mut reader: impl Read, needed: u64, reserve: u64) -> NtfsReaderResult<Vec<u8>> {
    let too_large = || NtfsReaderError::AllocationTooLarge { size: needed };
    if needed > MAX_BITMAP {
        return Err(too_large());
    }
    let length = usize::try_from(needed).map_err(|_| too_large())?;
    let first = reservation(needed, reserve).max(1);
    let mut bits = Vec::new();
    let mut step = first;
    while bits.len() < length {
        bits.try_reserve_exact(step).map_err(|_| too_large())?;
        #[cfg(test)]
        LARGEST_CAPACITY.set(LARGEST_CAPACITY.get().max(bits.capacity()));
        let before = bits.len();
        (&mut reader).take(step as u64).read_to_end(&mut bits)?;
        if bits.len() - before < step {
            return Err(invalid("$Bitmap ended before its size"));
        }
        step = (length - bits.len()).min(bits.len().max(first));
    }
    Ok(bits)
}

/// Whether a stream has a part that is stored nowhere: a hole or a missing extent. Both read as
/// zeroes or fail, and a real `$Bitmap` has neither.
fn has_holes(extents: &[StreamExtent]) -> bool {
    extents.iter().any(|extent| {
        matches!(
            extent.location,
            ExtentLocation::Sparse | ExtentLocation::Missing
        )
    })
}

fn invalid(details: &'static str) -> NtfsReaderError {
    NtfsReaderError::InvalidClusterBitmap { details }
}

/// Which clusters of a volume are allocated, read from its `$Bitmap` (record 6): one bit per
/// cluster, in memory. Answers "is cluster N in use" and, through [`StreamReader::allocation`],
/// how much of a stream sits in clusters that are.
///
/// A snapshot of the moment it was read: it says **allocated or free, nothing more**. A free
/// cluster is not one that still holds its old bytes: on a disk that receives TRIM (SSDs, thin
/// provisioned virtual disks, any volume with delete notifications on) freed clusters can be
/// erased while the bitmap keeps saying free. Measured: 5 to 11 s after the delete on a physical
/// NVMe SSD and 6 to 20 s on a virtual disk, but not at all in one busy run on the virtual disk.
/// Erased reads as zeroes, or as noise on a BitLocker volume (the zeroes are decrypted). An
/// allocated cluster belongs to something else and usually holds what its new owner wrote. Read
/// the bytes for content; never infer one from the other.
///
/// One bit per cluster: 32 MiB per TB at 4 KiB clusters. That is why it is not part of [`Mft`]; it
/// is read once, by [`Self::new`], through the same reader as [`NtfsFile::open_stream`], needing
/// the same elevated access.
pub struct ClusterBitmap {
    /// Exactly `cluster_count.div_ceil(8)` bytes: bit `n % 8` of byte `n / 8` is cluster `n`, set
    /// when allocated. Bits past the last cluster are padding and are never looked at.
    bits: Vec<u8>,
    cluster_size: u64,
    cluster_count: u64,
    /// The path of the volume it was read from, to refuse a stream of another one.
    volume_path: PathBuf,
}

impl fmt::Debug for ClusterBitmap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClusterBitmap")
            .field("cluster_size", &self.cluster_size)
            .field("cluster_count", &self.cluster_count)
            .field("volume_path", &self.volume_path)
            .finish_non_exhaustive()
    }
}

impl ClusterBitmap {
    /// Reads the cluster bitmap of the volume `mft` was loaded from: the unnamed `$DATA` of record
    /// 6 (`$Bitmap`), from the raw volume.
    ///
    /// Like [`Mft::new`], this reads through a normal volume handle: flush the volume first (see
    /// [`Mft::new`]) or a bitmap read just after a delete may still show its clusters allocated.
    ///
    /// The volume has `volume size / cluster size` clusters, so its bitmap has that many bits.
    /// Only the bytes those bits need are read: a `$Bitmap` claiming to be larger is not read past
    /// that, and its last byte's padding bits are ignored.
    ///
    /// Errors:
    ///
    /// - [`NtfsReaderError::InvalidClusterBitmap`]: record 6 is missing or not in use, the volume's
    ///   size is unknown, `$Bitmap` is shorter than the volume needs (counting only bytes below its
    ///   initialized size), or it has sparse or missing parts (a real one has none; such a part,
    ///   like a byte past the initialized size, would read as free clusters).
    /// - [`NtfsReaderError::AllocationTooLarge`]: the bitmap the volume's size asks for is over
    ///   4 GiB, or does not fit in memory. The size comes from the boot sector and is not trusted:
    ///   at most 64 MiB are reserved before bytes arrive, and the buffer grows as they do.
    /// - The errors of [`NtfsFile::open_stream`] and of reading the stream (as
    ///   [`NtfsReaderError::Io`]).
    ///
    /// ```no_run
    /// use ntfs_reader::{ClusterBitmap, Mft, Volume};
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let mft = Mft::new(Volume::new(r"\\.\C:")?)?;
    /// let bitmap = ClusterBitmap::new(&mft)?;
    /// let allocated = (0..bitmap.cluster_count())
    ///     .filter(|&cluster| bitmap.is_allocated(cluster))
    ///     .count();
    /// println!("{allocated} of {} clusters are allocated", bitmap.cluster_count());
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(mft: &Mft) -> NtfsReaderResult<Self> {
        Self::load(mft, |file| {
            let stream = file.open_stream(None)?;
            let holes = has_holes(stream.extents());
            // Past the initialized size the stream reads as zeroes, which would say its clusters
            // are free: only the bytes that were written count.
            Ok((stream.initialized_size(), stream, holes))
        })
    }

    /// [`Self::new`] with the `$Bitmap` stream supplied by `open`: its initialized size, a reader
    /// of its bytes, and whether it has holes (see [`has_holes`]). Tests pass a stream over a
    /// synthetic volume.
    fn load<R: Read>(
        mft: &Mft,
        open: impl FnOnce(&NtfsFile<'_>) -> NtfsReaderResult<(u64, R, bool)>,
    ) -> NtfsReaderResult<Self> {
        let file = mft
            .record(BITMAP_RECORD)
            .filter(NtfsFile::is_used)
            .ok_or(invalid("record 6 ($Bitmap) is missing or not in use"))?;
        let (size, reader, holes) = open(&file)?;
        if holes {
            return Err(invalid("$Bitmap has sparse or missing parts"));
        }
        Self::read(mft.volume(), size, reader)
    }

    /// The bitmap of `volume` from the `size` bytes of a `$Bitmap` stream behind `reader`. Reads
    /// only the bytes the volume needs. Both sizes come from the volume and are not trusted, so at
    /// most [`RESERVE_LIMIT`] bytes are reserved before any arrive, and the buffer grows as they
    /// do.
    pub(crate) fn read(volume: &Volume, size: u64, reader: impl Read) -> NtfsReaderResult<Self> {
        let (cluster_size, volume_size) = (volume.cluster_size(), volume.volume_size());
        if cluster_size == 0 || volume_size == 0 {
            return Err(invalid("the volume's size is unknown"));
        }
        let cluster_count = volume_size / cluster_size;
        let needed = cluster_count.div_ceil(8);
        if size < needed {
            return Err(invalid("$Bitmap is shorter than the volume needs"));
        }
        let bits = read_bits(reader, needed, RESERVE_LIMIT)?;
        Ok(ClusterBitmap {
            bits,
            cluster_size,
            cluster_count,
            volume_path: volume.path().to_path_buf(),
        })
    }

    /// The volume's cluster size in bytes.
    pub fn cluster_size(&self) -> u64 {
        self.cluster_size
    }

    /// The number of clusters on the volume: size divided by [`Self::cluster_size`], rounded down.
    /// A partial cluster at the volume's very end is not counted.
    pub fn cluster_count(&self) -> u64 {
        self.cluster_count
    }

    /// Whether cluster `cluster` (a cluster number from the volume's start, the LCN of a data run)
    /// was allocated when the bitmap was read. Says nothing about what the cluster holds, see the
    /// type's documentation. `false` for `cluster` at or above [`Self::cluster_count`], like
    /// [`Mft::is_allocated`] past the last record: a cluster past the volume's end is not free
    /// either, so check the count first where that matters.
    pub fn is_allocated(&self, cluster: u64) -> bool {
        cluster < self.cluster_count
            && self.bits[(cluster / 8) as usize] & (1 << (cluster % 8)) != 0
    }

    /// How many of the clusters `first..end` are allocated. `end` is at most the cluster count.
    fn allocated_in(&self, first: u64, end: u64) -> u64 {
        debug_assert!(end <= self.cluster_count);
        if first >= end {
            return 0;
        }
        let (first_byte, last_byte) = ((first / 8) as usize, ((end - 1) / 8) as usize);
        // The bits of the first byte from `first` up, and of the last one below `end`.
        let head = 0xFFu8 << (first % 8);
        let tail = 0xFFu8 >> (8 - ((end - 1) % 8 + 1));
        let ones = |byte: u8| u64::from(byte.count_ones());
        if first_byte == last_byte {
            return ones(self.bits[first_byte] & head & tail);
        }
        ones(self.bits[first_byte] & head)
            + ones(self.bits[last_byte] & tail)
            + self.bits[first_byte + 1..last_byte]
                .iter()
                .map(|&byte| ones(byte))
                .sum::<u64>()
    }
}

/// What the bitmap says about a stream's stored bytes: see [`StreamAllocation::state`]. Says
/// nothing about whether the bytes are intact, only where they are and whether that place is in
/// use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AllocationState {
    /// Every stored byte sits in a free cluster. It may still hold the old bytes or already be
    /// zeroed: this does not say which. Nor does it say the clusters are untouched since the file
    /// was deleted: if another file took them and was deleted too, both report `Free`, and this
    /// one's clusters hold that file's bytes.
    Free,
    /// Some stored bytes sit in free clusters and some in allocated ones: part of the stream has
    /// been given to something else.
    PartlyAllocated,
    /// Every stored byte sits in an allocated cluster: the normal state for a live file. For a
    /// deleted one, the clusters now belong to something else and usually hold its bytes.
    Allocated,
    /// The stream has no bytes in clusters: it is empty, resident, sparse, or entirely past its
    /// initialized size. A stream that lost its runs is not this, see [`Self::Lost`].
    NoStoredData,
    /// Part of the stream has no known place: its extent record is missing
    /// ([`StreamAllocation::missing`]) or points past the clusters the bitmap covers
    /// ([`StreamAllocation::outside_bitmap`]). Whatever the located parts say, the answer for the
    /// stream as a whole is unknown, so this is never [`Self::Free`] or [`Self::Allocated`]: read
    /// [`StreamAllocation::in_free_clusters`] and [`StreamAllocation::in_allocated_clusters`] for
    /// the parts that were located.
    Incomplete,
    /// The stream's data was lost when its file was deleted ([`StreamAllocation::data_lost`]):
    /// nothing to judge, and not truly empty even though it reads as one. Only when nothing else
    /// is known to be missing: [`Self::Incomplete`] wins.
    Lost,
}

/// Where the bytes of a stream are, and what a [`ClusterBitmap`] says about the clusters they are
/// in: [`StreamReader::allocation`]. Every byte of the stream is counted in exactly one of the
/// parts, so they add up to [`Self::size`].
///
/// This reports **allocation, never recoverability**: see [`ClusterBitmap`] for what free and
/// allocated do and do not say about a cluster's bytes (TRIM, reuse by another file, and so on).
/// The [`StreamReader`] does not look at the bitmap and reports exactly what it reads, so zeroes
/// are a valid answer, not an error.
///
/// The parts follow what the reader returns: a byte at or beyond the initialized size reads as
/// zeroes whatever the extent behind it says, so it is counted there and nowhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamAllocation {
    size: u64,
    in_free_clusters: u64,
    in_allocated_clusters: u64,
    resident: u64,
    sparse: u64,
    missing: u64,
    beyond_initialized: u64,
    outside_bitmap: u64,
    data_lost: bool,
}

impl StreamAllocation {
    /// The logical size of the stream, [`StreamReader::size`]: the sum of every other part.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Bytes in clusters the bitmap marks free.
    pub fn in_free_clusters(&self) -> u64 {
        self.in_free_clusters
    }

    /// Bytes in clusters the bitmap marks allocated.
    pub fn in_allocated_clusters(&self) -> u64 {
        self.in_allocated_clusters
    }

    /// Bytes stored in the file's MFT record, not in clusters.
    pub fn resident(&self) -> u64 {
        self.resident
    }

    /// Bytes of holes: not stored anywhere, they read as zeroes.
    pub fn sparse(&self) -> u64 {
        self.sparse
    }

    /// Bytes whose extent record was not found, so nothing says where they are: reading them
    /// fails with [`NtfsReaderError::StreamExtentMissing`].
    pub fn missing(&self) -> u64 {
        self.missing
    }

    /// Bytes at or beyond the initialized size: never written, read as zeroes whatever their
    /// clusters hold.
    pub fn beyond_initialized(&self) -> u64 {
        self.beyond_initialized
    }

    /// Whether the stream is one whose data was lost: [`StreamReader::data_lost`].
    pub fn data_lost(&self) -> bool {
        self.data_lost
    }

    /// Bytes stored in clusters the bitmap does not cover, past the last cluster it counts. Zero
    /// for any stream [`NtfsFile::open_stream`] accepts against a bitmap of the same volume, since
    /// it refuses runs that leave the volume: only bytes in the partial cluster at the volume's
    /// very end (which the bitmap does not count) can land here.
    pub fn outside_bitmap(&self) -> u64 {
        self.outside_bitmap
    }

    /// The state of the stream as a whole. The first row that fits decides:
    ///
    /// | [`Self::missing`] + [`Self::outside_bitmap`] | [`Self::data_lost`] | [`Self::in_free_clusters`] | [`Self::in_allocated_clusters`] | State |
    /// |---|---|---|---|---|
    /// | more than 0 | any | any | any | [`AllocationState::Incomplete`] |
    /// | 0 | yes | any | any | [`AllocationState::Lost`] |
    /// | 0 | no | more than 0 | more than 0 | [`AllocationState::PartlyAllocated`] |
    /// | 0 | no | more than 0 | 0 | [`AllocationState::Free`] |
    /// | 0 | no | 0 | more than 0 | [`AllocationState::Allocated`] |
    /// | 0 | no | 0 | 0 | [`AllocationState::NoStoredData`] |
    ///
    /// Resident, sparse, or beyond-initialized-size bytes have no cluster to be judged by and
    /// change nothing. A part with no known place makes the whole answer unknown whatever the
    /// rest says: a stream with one free cluster and a missing extension record is `Incomplete`,
    /// not `Free`. A stream that lost its data reads as an empty one, so it would be
    /// `NoStoredData` like a genuinely empty stream: `Lost` tells them apart.
    pub fn state(&self) -> AllocationState {
        if self.missing + self.outside_bitmap > 0 {
            return AllocationState::Incomplete;
        }
        if self.data_lost {
            return AllocationState::Lost;
        }
        match (self.in_free_clusters > 0, self.in_allocated_clusters > 0) {
            (true, true) => AllocationState::PartlyAllocated,
            (true, false) => AllocationState::Free,
            (false, true) => AllocationState::Allocated,
            (false, false) => AllocationState::NoStoredData,
        }
    }

    /// Counts the `stored` bytes that start at byte `offset` of the volume by the state of the
    /// clusters they are in. Partial clusters at either end count for the bytes in them only.
    fn add_volume_bytes(&mut self, bitmap: &ClusterBitmap, offset: u64, stored: u64) {
        let cluster_size = bitmap.cluster_size;
        // `cluster_count` clusters fit within the volume's size.
        let covered = bitmap.cluster_count * cluster_size;
        let end = offset.checked_add(stored).map(|end| end.min(covered));
        let Some(end) = end.filter(|&end| offset < end) else {
            self.outside_bitmap += stored;
            return;
        };
        self.outside_bitmap += stored - (end - offset);

        let (first, last) = (offset / cluster_size, (end - 1) / cluster_size);
        let mut count = |cluster: u64, bytes: u64| {
            if bitmap.allocated_in(cluster, cluster + 1) == 1 {
                self.in_allocated_clusters += bytes;
            } else {
                self.in_free_clusters += bytes;
            }
        };
        if first == last {
            count(first, end - offset);
            return;
        }
        count(first, (first + 1) * cluster_size - offset);
        count(last, end - last * cluster_size);
        let middle = last - first - 1;
        let allocated = bitmap.allocated_in(first + 1, last);
        self.in_allocated_clusters += allocated * cluster_size;
        self.in_free_clusters += (middle - allocated) * cluster_size;
    }
}

/// Classifies every byte of a stream that `extents` tile from 0 to `size`: see [`StreamAllocation`].
pub(crate) fn allocation_of(
    extents: &[StreamExtent],
    size: u64,
    initialized_size: u64,
    data_lost: bool,
    bitmap: &ClusterBitmap,
) -> StreamAllocation {
    let mut allocation = StreamAllocation {
        size,
        in_free_clusters: 0,
        in_allocated_clusters: 0,
        resident: 0,
        sparse: 0,
        missing: 0,
        beyond_initialized: 0,
        outside_bitmap: 0,
        data_lost,
    };
    for extent in extents {
        // The reader returns zeroes from the initialized size on, wherever the extent claims
        // bytes are; the extent counts only up to there.
        let stored = initialized_size
            .saturating_sub(extent.stream_offset)
            .min(extent.length);
        allocation.beyond_initialized += extent.length - stored;
        match extent.location {
            ExtentLocation::Resident => allocation.resident += stored,
            ExtentLocation::Sparse => allocation.sparse += stored,
            ExtentLocation::Missing => allocation.missing += stored,
            ExtentLocation::Volume { offset } => {
                allocation.add_volume_bytes(bitmap, offset, stored);
            }
        }
    }
    allocation
}

impl StreamReader {
    /// How much of this stream sits in clusters `bitmap` says are free, how much in allocated
    /// ones, and how much is not in clusters at all. Reads nothing from the volume: it walks
    /// [`Self::extents`] and counts, per extent, the bytes falling in each cluster, so a run
    /// starting or ending inside a cluster counts only its own bytes.
    ///
    /// `bitmap` must be the bitmap of the volume the stream is on ([`ClusterBitmap::new`] of the
    /// same [`Mft`]). For a live file it reports the same shape, everything allocated.
    ///
    /// This is allocation, not recoverability: see [`StreamAllocation`]. A free cluster may hold
    /// old bytes or zeroes; the reader reports exactly what it reads either way.
    ///
    /// Errors: [`NtfsReaderError::InvalidClusterBitmap`] when the bitmap was read from a volume
    /// with another cluster size, or from another volume (path compared ignoring ASCII case).
    /// Either would judge the wrong clusters and answer with false confidence.
    ///
    /// ```no_run
    /// use ntfs_reader::{AllocationState, ClusterBitmap, Mft, Volume};
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let mft = Mft::new(Volume::new(r"\\.\C:")?)?;
    /// let bitmap = ClusterBitmap::new(&mft)?;
    /// for file in mft.deleted_files().filter(|file| !file.is_directory()).take(10) {
    ///     let Ok(stream) = file.open_stream(None) else { continue };
    ///     let allocation = stream.allocation(&bitmap)?;
    ///     match allocation.state() {
    ///         AllocationState::Free => {
    ///             println!("{} bytes in free clusters", allocation.in_free_clusters())
    ///         }
    ///         AllocationState::PartlyAllocated => println!(
    ///             "{} of {} bytes were given to other files",
    ///             allocation.in_allocated_clusters(),
    ///             allocation.size()
    ///         ),
    ///         AllocationState::Allocated => println!("all in clusters that are in use again"),
    ///         AllocationState::Lost => println!("NTFS zeroed its runs when it deleted the file"),
    ///         AllocationState::NoStoredData => println!("nothing stored in clusters"),
    ///         AllocationState::Incomplete => println!(
    ///             "{} bytes have no known place",
    ///             allocation.missing() + allocation.outside_bitmap()
    ///         ),
    ///         _ => println!("a state this version does not know"),
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn allocation(&self, bitmap: &ClusterBitmap) -> NtfsReaderResult<StreamAllocation> {
        if bitmap.cluster_size != self.cluster_size() {
            return Err(invalid("the bitmap's cluster size is not the stream's"));
        }
        if !bitmap
            .volume_path
            .as_os_str()
            .eq_ignore_ascii_case(self.volume_path().as_os_str())
        {
            return Err(invalid("the bitmap is of another volume than the stream"));
        }
        Ok(allocation_of(
            self.extents(),
            self.size(),
            self.initialized_size(),
            self.data_lost(),
            bitmap,
        ))
    }
}

#[cfg(test)]
#[path = "tests/bitmap.rs"]
mod tests;
