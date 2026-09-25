// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use std::io::{self, Cursor};
use std::path::PathBuf;

use super::*;
use crate::api::{NtfsAttributeType, FIRST_NORMAL_RECORD};
use crate::file::Record;
use crate::mft::test_records::*;
use crate::property::property;
use crate::stream;

const CS: u64 = CLUSTER_SIZE as u64;

fn volume(clusters: u64, cluster_size: u64) -> Volume {
    Volume::synthetic(
        PathBuf::from(VOLUME_PATH),
        cluster_size,
        clusters * cluster_size,
        RECORD_SIZE as u64,
        0,
    )
}

/// The bitmap of a volume of `clusters` clusters from `bits`, as `ClusterBitmap::read` reads a
/// `$Bitmap` stream of `size` bytes.
fn parse(clusters: u64, size: u64, bits: &[u8]) -> NtfsReaderResult<ClusterBitmap> {
    ClusterBitmap::read(&volume(clusters, CS), size, Cursor::new(bits.to_vec()))
}

fn invalid_details(result: NtfsReaderResult<ClusterBitmap>) -> &'static str {
    match result {
        Err(NtfsReaderError::InvalidClusterBitmap { details }) => details,
        other => panic!("not an InvalidClusterBitmap: {other:?}"),
    }
}

/// A bitmap of 4 KiB clusters from a picture: `A` is an allocated cluster, `F` a free one.
fn picture(states: &str) -> ClusterBitmap {
    let mut bits = vec![0u8; states.len().div_ceil(8)];
    for (cluster, state) in states.chars().enumerate() {
        match state {
            'A' => bits[cluster / 8] |= 1 << (cluster % 8),
            'F' => {}
            other => panic!("not a cluster state: {other}"),
        }
    }
    ClusterBitmap {
        bits,
        cluster_size: CS,
        cluster_count: states.len() as u64,
        volume_path: PathBuf::from(VOLUME_PATH),
    }
}

// ---- The bitmap ----------------------------------------------------------------------------

#[test]
fn a_bitmap_of_a_whole_number_of_bytes_is_read_bit_by_bit() {
    let bits = [0b1010_0110, 0xFF, 0x00, 0x01, 0x80, 0x5A, 0xA5, 0xF0];
    let bitmap = parse(64, 8, &bits).unwrap();
    assert_eq!(bitmap.cluster_size(), CS);
    assert_eq!(bitmap.cluster_count(), 64);
    for cluster in 0..64u64 {
        let want = bits[(cluster / 8) as usize] >> (cluster % 8) & 1 == 1;
        assert_eq!(bitmap.is_allocated(cluster), want, "cluster {cluster}");
    }
    // Bit 0 of byte 0 is cluster 0 (clear here), bit 7 of byte 4 is cluster 39 (set).
    assert!(!bitmap.is_allocated(0));
    assert!(bitmap.is_allocated(1));
    assert!(bitmap.is_allocated(39));
    // Past the end: not allocated, whatever the buffer holds there.
    assert!(!bitmap.is_allocated(64));
    assert!(!bitmap.is_allocated(u64::MAX));
}

// Padding bits past the last cluster say nothing, whatever they are, and a partial cluster at
// the end of the volume does not count as one.
#[test]
fn padding_bits_and_a_partial_last_cluster_are_ignored() {
    let volume = Volume::synthetic(PathBuf::from(VOLUME_PATH), CS, 20 * CS + 1000, 1024, 0);
    let bitmap = ClusterBitmap::read(&volume, 3, Cursor::new(vec![0xFF, 0x00, 0xFF])).unwrap();
    assert_eq!(bitmap.cluster_count(), 20);
    assert_eq!(bitmap.bits.len(), 3);
    assert!(bitmap.is_allocated(19));
    assert!(!bitmap.is_allocated(20), "a padding bit that is set");
    assert!(!bitmap.is_allocated(23), "a padding bit that is set");
    assert_eq!(bitmap.allocated_in(0, 20), 8 + 4);
    assert_eq!(bitmap.allocated_in(8, 20), 4);
}

// Real volumes round `$Bitmap` up to 8 bytes: what is past the bytes the volume needs is not read.
#[test]
fn a_bitmap_longer_than_the_volume_needs_is_read_only_as_far_as_needed() {
    let bitmap = parse(20, 8, &[0xFF; 8]).unwrap();
    assert_eq!(bitmap.bits.len(), 3);
    assert_eq!(bitmap.cluster_count(), 20);
}

#[test]
fn a_bitmap_shorter_than_the_volume_needs_is_an_error() {
    // 20 clusters need 3 bytes.
    assert!(invalid_details(parse(20, 2, &[0xFF; 2])).contains("shorter"));
    assert!(invalid_details(parse(20, 0, &[])).contains("shorter"));
    // The size says enough but the stream ends early.
    assert!(invalid_details(parse(20, 3, &[0xFF; 2])).contains("ended"));
    // Exactly enough is fine.
    parse(20, 3, &[0xFF; 3]).unwrap();
    parse(16, 2, &[0xFF; 2]).unwrap();
    assert!(invalid_details(parse(17, 2, &[0xFF; 2])).contains("shorter"));
}

#[test]
fn an_empty_bitmap_is_right_for_a_volume_without_a_whole_cluster() {
    let small = Volume::synthetic(PathBuf::from(VOLUME_PATH), CS, CS - 1, 1024, 0);
    let bitmap = ClusterBitmap::read(&small, 0, io::empty()).unwrap();
    assert_eq!(bitmap.cluster_count(), 0);
    assert!(!bitmap.is_allocated(0));
    assert_eq!(bitmap.allocated_in(0, 0), 0);
    // But not for a volume that has clusters.
    assert!(invalid_details(parse(8, 0, &[])).contains("shorter"));
}

#[test]
fn a_volume_of_unknown_size_is_an_error() {
    for (cluster_size, volume_size) in [(CS, 0), (0, CS)] {
        let unknown = Volume::synthetic(
            PathBuf::from(VOLUME_PATH),
            cluster_size,
            volume_size,
            1024,
            0,
        );
        let details = invalid_details(ClusterBitmap::read(&unknown, 8, Cursor::new([0u8; 8])));
        assert!(details.contains("unknown"), "{details}");
    }
}

// A `$Bitmap` claiming 1 EiB is read only for the bytes the volume needs: an endless reader
// would otherwise hang the read or exhaust memory.
#[test]
fn a_huge_claimed_size_is_not_read() {
    let bitmap = ClusterBitmap::read(&volume(64, CS), 1 << 60, io::repeat(0xFF)).unwrap();
    assert_eq!(bitmap.bits, [0xFF; 8]);
    assert_eq!(bitmap.allocated_in(0, 64), 64);
}

// The volume's size is untrusted too: it decides how much to read and how much to reserve.
#[test]
fn a_huge_volume_is_an_error_not_an_allocation() {
    let huge = Volume::synthetic(PathBuf::from(VOLUME_PATH), 512, u64::MAX, 1024, 0);
    // The stream is too short for it.
    let err = ClusterBitmap::read(&huge, 16, io::repeat(0xFF)).unwrap_err();
    assert!(
        matches!(err, NtfsReaderError::InvalidClusterBitmap { .. }),
        "{err:?}"
    );
    // The stream claims to be long enough: 4.5 PB of bitmap does not fit.
    let err = ClusterBitmap::read(&huge, 1 << 60, io::repeat(0xFF)).unwrap_err();
    assert!(
        matches!(err, NtfsReaderError::AllocationTooLarge { .. }),
        "{err:?}"
    );
}

// A sparse `$Bitmap`, or one with a lost extent record, is refused rather than read as zeroes
// (every cluster free): a hostile boot sector could pair a huge claimed volume with such a file
// and get an apparently empty volume that reserves memory for it.
#[test]
fn a_bitmap_that_is_sparse_or_has_lost_parts_is_refused() {
    let image = image_with_bitmap(64, &[0xFF; 8]);
    let sparse = bitmap_record(8, &[(1, None)]);
    let lost = bitmap_record(8 + CS, &[(1, Some(BITMAP_LCN as i64))]);
    let half = bitmap_record(2 * CS, &[(1, Some(BITMAP_LCN as i64)), (1, None)]);
    for (what, record) in [("sparse", sparse), ("a lost part", lost), ("a hole", half)] {
        let result = load(&mft_with_record_6(record, 64), &image);
        match result {
            Err(NtfsReaderError::InvalidClusterBitmap { details }) => {
                assert!(details.contains("sparse or missing"), "{what}: {details}")
            }
            other => panic!("{what}: a bitmap with holes was accepted: {other:?}"),
        }
    }
}

// Only a bounded amount is reserved before bytes arrive: both the `$Bitmap` size and the volume
// size are untrusted, so a short stream must not cost its claimed size.
#[test]
fn only_a_bounded_amount_is_reserved_before_the_bytes_arrive() {
    assert_eq!(reservation(100, 10), 10);
    assert_eq!(reservation(5, 10), 5);
    assert_eq!(reservation(u64::MAX, RESERVE_LIMIT), RESERVE_LIMIT as usize);
    // The buffer still grows to what is needed, past the reservation.
    let bits = read_bits(io::repeat(0xAA), 100, 10).unwrap();
    assert_eq!(bits, [0xAA; 100]);
    // A stream that ends early is an error, not a short bitmap.
    let err = read_bits(io::repeat(0xAA).take(50), 100, 10).unwrap_err();
    assert!(
        matches!(err, NtfsReaderError::InvalidClusterBitmap { .. }),
        "{err:?}"
    );
}

// The buffer grows in chunks of at most what it already holds (at least the first reservation),
// each reserved exactly, so a bitmap of `needed` bytes ends with capacity exactly `needed`;
// doubling from the first reservation could end up to twice that.
#[test]
fn a_bitmap_is_read_into_a_buffer_of_exactly_its_size() {
    for (needed, reserve) in [
        (300u64, 100u64),
        (301, 100),
        (299, 100),
        (100, 100),
        (7, 100),
        (1000, 64),
    ] {
        let bits = read_bits(io::repeat(0xAA), needed, reserve).unwrap();
        assert_eq!(bits.len() as u64, needed);
        assert_eq!(
            bits.capacity() as u64,
            needed,
            "needed {needed}, reserve {reserve}"
        );
    }
    assert!(read_bits(io::repeat(0xAA), 0, 100).unwrap().is_empty());
}

// Growth follows the bytes that arrive: a stream that stops early costs the first reservation
// plus what it delivered, never the size it claimed.
#[test]
fn the_buffer_grows_only_as_fast_as_the_bytes_arrive() {
    LARGEST_CAPACITY.set(0);
    let err = read_bits(io::repeat(0xAA).take(150), 100_000, 100).unwrap_err();
    assert!(
        matches!(err, NtfsReaderError::InvalidClusterBitmap { .. }),
        "{err:?}"
    );
    // 100 reserved, 100 arrived, another 100 reserved, 50 arrived, then the end.
    assert_eq!(LARGEST_CAPACITY.get(), 200);
}

// `ClusterBitmap::read` caps the reservation at `RESERVE_LIMIT`: a boot sector claiming a
// gigabyte bitmap, backed by a stream that delivers a thousand bytes, reserves 64 MiB at most.
#[test]
fn a_bitmap_that_claims_a_gigabyte_reserves_the_limit_and_no_more() {
    let clusters = 8u64 << 30;
    let huge = Volume::synthetic(PathBuf::from(VOLUME_PATH), 4096, clusters * 4096, 1024, 0);
    LARGEST_CAPACITY.set(0);
    let err = ClusterBitmap::read(&huge, 1 << 30, io::repeat(0xFF).take(1000)).unwrap_err();
    assert!(
        matches!(err, NtfsReaderError::InvalidClusterBitmap { .. }),
        "{err:?}"
    );
    assert_eq!(LARGEST_CAPACITY.get(), RESERVE_LIMIT as usize);
}

// The largest bitmap is 4 GiB: the bitmap of the largest volume NTFS on Windows formats is 512 MiB.
#[test]
fn a_bitmap_over_four_gib_is_too_large() {
    let clusters = (4u64 << 30) * 8 + 8;
    let huge = Volume::synthetic(PathBuf::from(VOLUME_PATH), 4096, clusters * 4096, 1024, 0);
    // Refused before a byte is read: a reader that fails says if one was.
    struct Bomb;
    impl Read for Bomb {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("the bitmap was read"))
        }
    }
    let err = ClusterBitmap::read(&huge, u64::MAX, Bomb).unwrap_err();
    assert!(
        matches!(err, NtfsReaderError::AllocationTooLarge { .. }),
        "{err:?}"
    );
}

#[test]
fn a_failing_read_is_an_error() {
    struct Fails;
    impl Read for Fails {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("bad sector"))
        }
    }
    let err = ClusterBitmap::read(&volume(64, CS), 8, Fails).unwrap_err();
    assert!(matches!(err, NtfsReaderError::Io(_)), "{err:?}");
}

// ---- Through record 6 --------------------------------------------------------------------

const BITMAP_LCN: u64 = 2;

/// A record 6 whose unnamed `$DATA` is `size` bytes made of `runs` (`(clusters, Some(lcn))`,
/// `None` for a hole).
fn bitmap_record(size: u64, runs: &[(u64, Option<i64>)]) -> Vec<u8> {
    bitmap_record_initialized(size, size, runs)
}

/// [`bitmap_record`] with its initialized size set to `initialized`.
fn bitmap_record_initialized(size: u64, initialized: u64, runs: &[(u64, Option<i64>)]) -> Vec<u8> {
    let mut record = new_record(BITMAP_RECORD, 1, 0);
    let clusters: u64 = runs.iter().map(|run| run.0).sum();
    let end = add_nonresident_data_runs(
        &mut record,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Data,
        "",
        0,
        clusters.wrapping_sub(1),
        size,
        &encode_runs(runs),
    );
    write_u64(&mut record, ATTRIBUTES_OFFSET + 56, initialized);
    let end = add_end_marker(&mut record, end);
    finish_record(&mut record, end);
    record
}

/// An `Mft` whose only record is `record6`, on a volume of `clusters` clusters, with its
/// `$BITMAP` bit set unless `record6` is freed.
fn mft_with_record_6(record6: Vec<u8>, clusters: u64) -> Mft {
    let number = BITMAP_RECORD as usize;
    let in_use = Record::new(BITMAP_RECORD, &record6).is_some_and(|record| record.is_used());
    let mut data = vec![0u8; FIRST_NORMAL_RECORD as usize * RECORD_SIZE];
    data[number * RECORD_SIZE..(number + 1) * RECORD_SIZE].copy_from_slice(&record6);
    let mut bitmap = vec![0u8; FIRST_NORMAL_RECORD as usize / 8];
    if in_use {
        bitmap[number / 8] |= 1 << (number % 8);
    }
    build_from_parts(volume(clusters, CS), data, bitmap)
}

/// A volume image of `clusters` clusters with `bits` at `BITMAP_LCN`, junk elsewhere.
fn image_with_bitmap(clusters: u64, bits: &[u8]) -> Vec<u8> {
    let mut image = vec![0xEE; (clusters * CS) as usize];
    let start = (BITMAP_LCN * CS) as usize;
    image[start..start + bits.len()].copy_from_slice(bits);
    image
}

/// `ClusterBitmap::new` over a synthetic volume: `load` with the stream opened over `image`.
fn load(mft: &Mft, image: &[u8]) -> NtfsReaderResult<ClusterBitmap> {
    ClusterBitmap::load(mft, |file| {
        let core = stream::open(file, None, Cursor::new(image))?;
        let holes = has_holes(&core.extents);
        Ok((core.initialized_size, core, holes))
    })
}

#[test]
fn the_bitmap_is_read_from_record_6() {
    let bits = [0x0F, 0xF0, 0x3C, 0xC3, 0x00, 0xFF, 0x81, 0x18];
    let image = image_with_bitmap(64, &bits);
    let record = bitmap_record(8, &[(1, Some(BITMAP_LCN as i64))]);
    let bitmap = load(&mft_with_record_6(record, 64), &image).unwrap();
    assert_eq!(bitmap.bits, bits);
    assert_eq!(bitmap.cluster_count(), 64);

    // A `$Bitmap` longer than the volume needs, all of it stored: the volume's clusters are all
    // that is read.
    let record = bitmap_record(CS, &[(1, Some(BITMAP_LCN as i64))]);
    let bitmap = load(&mft_with_record_6(record, 64), &image).unwrap();
    assert_eq!(bitmap.bits, bits);
}

#[test]
fn a_bitmap_file_that_cannot_be_used_is_an_error() {
    let image = image_with_bitmap(64, &[0xFF; 8]);
    let runs = [(1, Some(BITMAP_LCN as i64))];

    // No record 6.
    let mft = mft_with(vec![]);
    assert!(invalid_details(load(&mft, &image)).contains("record 6"));

    // A freed one, as a delete leaves it (flag and bitmap bit clear): it would read fine.
    let mut freed = bitmap_record(8, &runs);
    mark_freed(&mut freed);
    assert!(invalid_details(load(&mft_with_record_6(freed, 64), &image)).contains("record 6"));

    // A stream of 4 bytes for a volume that needs 8.
    let short = mft_with_record_6(bitmap_record(4, &runs), 64);
    assert!(invalid_details(load(&short, &image)).contains("shorter"));

    // A stream of 8 bytes with only 4 written: the other 4 read as zeroes, saying those clusters
    // are free.
    let uninitialized = mft_with_record_6(bitmap_record_initialized(8, 4, &runs), 64);
    assert!(invalid_details(load(&uninitialized, &image)).contains("shorter"));

    // The extent holding the bitmap was not found: refused, not read as zeroes.
    let lost = mft_with_record_6(bitmap_record(8, &[]), 64);
    assert!(invalid_details(load(&lost, &image)).contains("sparse or missing"));
}

// ---- Where a stream's bytes are ----------------------------------------------------------

fn extent(stream_offset: u64, length: u64, location: ExtentLocation) -> StreamExtent {
    StreamExtent {
        stream_offset,
        length,
        location,
    }
}

/// `extents` laid end to end from offset 0: `(length, location)`.
fn laid_out(parts: &[(u64, ExtentLocation)]) -> Vec<StreamExtent> {
    let mut at = 0;
    parts
        .iter()
        .map(|&(length, location)| {
            let placed = extent(at, length, location);
            at += length;
            placed
        })
        .collect()
}

fn size_of(extents: &[StreamExtent]) -> u64 {
    extents.iter().map(|extent| extent.length).sum()
}

/// The parts of an allocation, in the order free, allocated, resident, sparse, missing, beyond
/// the initialized size, outside the bitmap.
fn parts(allocation: &StreamAllocation) -> [u64; 7] {
    [
        allocation.in_free_clusters(),
        allocation.in_allocated_clusters(),
        allocation.resident(),
        allocation.sparse(),
        allocation.missing(),
        allocation.beyond_initialized(),
        allocation.outside_bitmap(),
    ]
}

/// The allocation of `extents` (which end where the stream does, initialized up to `initialized`).
#[track_caller]
fn classify(
    extents: &[StreamExtent],
    initialized: u64,
    bitmap: &ClusterBitmap,
) -> StreamAllocation {
    let size = size_of(extents);
    let allocation = allocation_of(extents, size, initialized, false, bitmap);
    assert_eq!(allocation.size(), size);
    assert_eq!(
        parts(&allocation).iter().sum::<u64>(),
        size,
        "{allocation:?}"
    );
    allocation
}

use ExtentLocation::{Missing, Resident, Sparse};

fn vol(offset: u64) -> ExtentLocation {
    ExtentLocation::Volume { offset }
}

#[test]
fn a_stream_in_free_clusters_is_free_and_one_in_allocated_clusters_is_allocated() {
    let bitmap = picture("FFFFAAAA");
    let free = classify(&laid_out(&[(4 * CS, vol(0))]), 4 * CS, &bitmap);
    assert_eq!(parts(&free), [4 * CS, 0, 0, 0, 0, 0, 0]);
    assert_eq!(free.state(), AllocationState::Free);

    let allocated = classify(&laid_out(&[(4 * CS, vol(4 * CS))]), 4 * CS, &bitmap);
    assert_eq!(parts(&allocated), [0, 4 * CS, 0, 0, 0, 0, 0]);
    assert_eq!(allocated.state(), AllocationState::Allocated);

    // Half and half: the run crosses from free clusters into allocated ones.
    let half = classify(&laid_out(&[(4 * CS, vol(2 * CS))]), 4 * CS, &bitmap);
    assert_eq!(parts(&half), [2 * CS, 2 * CS, 0, 0, 0, 0, 0]);
    assert_eq!(half.state(), AllocationState::PartlyAllocated);
}

// A run edge mid-cluster counts only the bytes in it: a stream ending 100 bytes into a cluster
// owns 100 bytes of it.
#[test]
fn a_run_that_starts_or_ends_inside_a_cluster_counts_only_its_bytes() {
    let bitmap = picture("AFAFFA");
    // Clusters 1 (free), 2 (allocated), and 100 bytes of 3 (free).
    let end = classify(&laid_out(&[(2 * CS + 100, vol(CS))]), 2 * CS + 100, &bitmap);
    assert_eq!(parts(&end), [CS + 100, CS, 0, 0, 0, 0, 0]);
    // 10 bytes of the allocated cluster 2 alone.
    let small = classify(&laid_out(&[(10, vol(2 * CS))]), 10, &bitmap);
    assert_eq!(parts(&small), [0, 10, 0, 0, 0, 0, 0]);
    // Starts 1000 bytes into cluster 1 (free) and ends 500 bytes into cluster 3 (free): the
    // 3096 bytes left of cluster 1, all of the allocated cluster 2, and 500 of cluster 3.
    let both = classify(
        &laid_out(&[(3096 + CS + 500, vol(CS + 1000))]),
        u64::MAX,
        &bitmap,
    );
    assert_eq!(parts(&both), [3096 + 500, CS, 0, 0, 0, 0, 0]);
    // Inside one cluster, from the middle to the middle.
    let inside = classify(&laid_out(&[(600, vol(2 * CS + 200))]), 600, &bitmap);
    assert_eq!(parts(&inside), [0, 600, 0, 0, 0, 0, 0]);
    // Ends exactly on a cluster boundary, and starts on one: no byte of the next cluster.
    let exact = classify(
        &laid_out(&[(CS, vol(CS)), (CS, vol(5 * CS))]),
        2 * CS,
        &bitmap,
    );
    assert_eq!(parts(&exact), [CS, CS, 0, 0, 0, 0, 0]);
    // The last byte of a cluster and the first of the next.
    let straddle = classify(&laid_out(&[(2, vol(2 * CS - 1))]), 2, &bitmap);
    assert_eq!(parts(&straddle), [1, 1, 0, 0, 0, 0, 0]);
}

#[test]
fn bytes_that_are_not_in_clusters_are_counted_apart() {
    let bitmap = picture("FAFF");
    let extents = laid_out(&[
        (100, Resident),
        (CS, vol(0)),
        (3 * CS, Sparse),
        (2 * CS + 7, Missing),
        (CS, vol(CS)),
    ]);
    let all = classify(&extents, u64::MAX, &bitmap);
    assert_eq!(parts(&all), [CS, CS, 100, 3 * CS, 2 * CS + 7, 0, 0]);
    // Lost bytes have no place, so the located free and allocated parts do not decide.
    assert_eq!(all.state(), AllocationState::Incomplete);

    let resident = classify(&laid_out(&[(100, Resident)]), 100, &bitmap);
    assert_eq!(parts(&resident), [0, 0, 100, 0, 0, 0, 0]);
    assert_eq!(resident.state(), AllocationState::NoStoredData);
    let sparse = classify(&laid_out(&[(5 * CS, Sparse)]), 5 * CS, &bitmap);
    assert_eq!(sparse.state(), AllocationState::NoStoredData);
    // Lost bytes are not free clusters.
    let lost = classify(&laid_out(&[(CS, Missing)]), CS, &bitmap);
    assert_eq!(parts(&lost), [0, 0, 0, 0, CS, 0, 0]);
    assert_eq!(lost.state(), AllocationState::Incomplete);
}

#[test]
fn an_empty_stream_has_nothing_stored() {
    let empty = classify(&[], 0, &picture("A"));
    assert_eq!(parts(&empty), [0; 7]);
    assert_eq!(empty.state(), AllocationState::NoStoredData);
}

// The reader returns zeroes from the initialized size on, whatever the clusters behind hold, so
// those bytes count as no cluster, allocated or not.
#[test]
fn bytes_at_or_beyond_the_initialized_size_are_counted_as_zeroes() {
    // Clusters 0 (allocated), 1 (free), then two more of each.
    let bitmap = picture("AFFAAF");
    let extents = laid_out(&[(4 * CS, vol(0))]);
    // Cut 100 bytes into cluster 1: cluster 0 whole and 100 bytes of the free cluster 1 are
    // stored, the rest reads as zeroes although cluster 3 is allocated.
    let cut = classify(&extents, CS + 100, &bitmap);
    assert_eq!(parts(&cut), [100, CS, 0, 0, 0, 3 * CS - 100, 0]);
    // Nothing was ever written.
    let never = classify(&extents, 0, &bitmap);
    assert_eq!(parts(&never), [0, 0, 0, 0, 0, 4 * CS, 0]);
    assert_eq!(never.state(), AllocationState::NoStoredData);
    // Fully written.
    let all = classify(&extents, 4 * CS, &bitmap);
    assert_eq!(parts(&all), [2 * CS, 2 * CS, 0, 0, 0, 0, 0]);

    // The cut falls in the middle of other kinds of extent.
    let mixed = laid_out(&[(CS, Sparse), (CS, Missing), (CS, Resident)]);
    let cut = classify(&mixed, CS + 10, &bitmap);
    assert_eq!(parts(&cut), [0, 0, 0, CS, 10, CS + (CS - 10), 0]);
    // A lost extent past the initialized size is not lost: nothing was written there.
    let missing = classify(&laid_out(&[(CS, vol(0)), (CS, Missing)]), CS, &bitmap);
    assert_eq!(parts(&missing), [0, CS, 0, 0, 0, CS, 0]);
}

#[test]
fn several_runs_are_added_up() {
    let bitmap = picture("AFFAFAAFFFAF");
    let extents = laid_out(&[
        (2 * CS, vol(9 * CS)), // clusters 9, 10: F A
        (CS, Sparse),
        (CS + 50, vol(3 * CS)), // cluster 3 (A) and 50 bytes of cluster 4 (F)
        (3 * CS, vol(CS)),      // clusters 1, 2, 3: F F A
        (CS, vol(11 * CS)),     // cluster 11: F
    ]);
    let all = classify(&extents, u64::MAX, &bitmap);
    assert_eq!(
        parts(&all),
        [CS + 50 + 2 * CS + CS, CS + CS + CS, 0, CS, 0, 0, 0]
    );
}

#[test]
fn bytes_in_clusters_the_bitmap_does_not_cover_are_counted_apart() {
    let bitmap = picture("AF");
    // Cluster 1 is inside, 2 and 3 are not.
    let straddling = classify(&laid_out(&[(3 * CS, vol(CS))]), u64::MAX, &bitmap);
    assert_eq!(parts(&straddling), [CS, 0, 0, 0, 0, 0, 2 * CS]);
    // Past the end altogether, exactly at its edge and in the middle of a cluster.
    for offset in [2 * CS, 2 * CS + 1, 10 * CS] {
        let outside = classify(&laid_out(&[(CS, vol(offset))]), u64::MAX, &bitmap);
        assert_eq!(parts(&outside), [0, 0, 0, 0, 0, 0, CS], "offset {offset}");
    }
    // An offset so large that offset + length overflows.
    let overflow = classify(&laid_out(&[(100, vol(u64::MAX - 5))]), u64::MAX, &bitmap);
    assert_eq!(parts(&overflow), [0, 0, 0, 0, 0, 0, 100]);
    // A bitmap with no clusters covers nothing.
    let none = classify(&laid_out(&[(CS, vol(0))]), u64::MAX, &picture(""));
    assert_eq!(parts(&none), [0, 0, 0, 0, 0, 0, CS]);
    assert_eq!(none.state(), AllocationState::Incomplete);
}

// The same, from the layout `open_stream` derives from real records: runs, a hole, an
// initialized size below the full size, and a size ending inside its last cluster.
#[test]
fn a_stream_opened_from_records_is_classified_by_its_extents() {
    let mut record = new_record(24, 1, 0);
    // Clusters 8 and 9, a hole of 2, cluster 12; 4.5 clusters in all, of which 3 were written.
    let runs = [(2, Some(8)), (2, None), (1, Some(12))];
    let deltas = [(2, Some(8i64)), (2, None), (1, Some(12 - 8))];
    let size = 4 * CS + CS / 2;
    let end = add_nonresident_data_runs(
        &mut record,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Data,
        "",
        0,
        runs.iter().map(|run| run.0).sum::<u64>() - 1,
        size,
        &encode_runs(&deltas),
    );
    write_u64(&mut record, ATTRIBUTES_OFFSET + 56, 3 * CS);
    let end = add_end_marker(&mut record, end);
    finish_record(&mut record, end);
    let (_, mut data, bitmap) = raw_parts(vec![record]);
    // `raw_parts` protected the record before the initialized size was written: redo it.
    let start = FIRST_NORMAL_RECORD as usize * RECORD_SIZE;
    protect_record(
        &mut data[start..start + RECORD_SIZE],
        UPDATE_SEQUENCE_NUMBER,
    );
    let mft = build_from_parts(volume(64, CS), data, bitmap);
    let file = mft.record(24).unwrap();
    let core = stream::open(&file, None, Cursor::new(Vec::<u8>::new())).unwrap();

    // Cluster 8 allocated, 9 free, 12 free.
    let mut states = vec!['F'; 64];
    states[8] = 'A';
    let bitmap = picture(&states.iter().collect::<String>());
    let allocation = allocation_of(
        &core.extents,
        core.size,
        core.initialized_size,
        false,
        &bitmap,
    );
    assert_eq!(core.initialized_size, 3 * CS);
    // Written: cluster 8 (allocated), 9 (free), the first hole cluster. The rest of the hole
    // is sparse, and the 4.5th cluster is past the initialized size.
    assert_eq!(parts(&allocation), [CS, CS, 0, CS, 0, CS + CS / 2, 0]);
    assert_eq!(allocation.size(), size);
    assert_eq!(allocation.state(), AllocationState::PartlyAllocated);
}

// For every layout and bitmap, the parts sum to the size and match counting the stream one byte
// at a time. The oracle reads raw bits, not the crate's own accessors, and its clusters need not
// align with anything.
#[test]
fn the_parts_add_up_to_the_size_and_match_a_byte_by_byte_count() {
    property(|u| {
        let cluster_size = u.int_in_range(1..=24u64)?;
        let clusters = u.int_in_range(0..=40u64)?;
        // Padding bits are random too.
        let mut bits = Vec::new();
        for _ in 0..clusters.div_ceil(8) {
            bits.push(u.arbitrary::<u8>()?);
        }
        let bitmap = ClusterBitmap {
            bits: bits.clone(),
            cluster_size,
            cluster_count: clusters,
            volume_path: PathBuf::from(VOLUME_PATH),
        };
        let raw = |cluster: u64| bits[(cluster / 8) as usize] >> (cluster % 8) & 1 == 1;

        let mut extents = Vec::new();
        let mut at = 0u64;
        for _ in 0..u.int_in_range(0..=6)? {
            let length = u.int_in_range(1..=3 * cluster_size + 10)?;
            let location = match u.int_in_range(0..=4)? {
                0 => Resident,
                1 => Sparse,
                2 => Missing,
                // Some of them lie partly or wholly outside the bitmap.
                _ => vol(u.int_in_range(0..=(clusters + 2) * cluster_size + 5)?),
            };
            extents.push(extent(at, length, location));
            at += length;
        }
        let size = at;
        let initialized = if u.ratio(1, 3)? {
            size
        } else {
            u.int_in_range(0..=size)?
        };

        let mut want = [0u64; 7];
        for position in 0..size {
            let extent = extents
                .iter()
                .find(|e| e.stream_offset <= position && position < e.stream_offset + e.length)
                .unwrap();
            let part = if position >= initialized {
                5
            } else {
                match extent.location {
                    Resident => 2,
                    Sparse => 3,
                    Missing => 4,
                    ExtentLocation::Volume { offset: base } => {
                        let cluster = (base + (position - extent.stream_offset)) / cluster_size;
                        if cluster >= clusters {
                            6
                        } else if raw(cluster) {
                            1
                        } else {
                            0
                        }
                    }
                }
            };
            want[part] += 1;
        }
        let got = allocation_of(&extents, size, initialized, false, &bitmap);
        assert_eq!(
            parts(&got),
            want,
            "clusters of {cluster_size}: {clusters}, bits {bits:02x?}, extents {extents:?}, \
                 initialized {initialized}"
        );
        assert_eq!(got.size(), size);
        assert_eq!(want.iter().sum::<u64>(), size);

        // The range count against counting bits.
        let first = u.int_in_range(0..=clusters)?;
        let end = u.int_in_range(first..=clusters)?;
        assert_eq!(
            bitmap.allocated_in(first, end),
            (first..end).filter(|&cluster| raw(cluster)).count() as u64,
            "bits {bits:02x?} {first}..{end}"
        );
        Ok(())
    });
}
