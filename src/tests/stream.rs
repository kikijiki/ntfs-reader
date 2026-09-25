// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use std::io::Cursor;
use std::path::PathBuf;

use super::*;
use crate::bitmap::{allocation_of, ClusterBitmap};
use crate::mft::test_records::*;
use crate::mft::Mft;
use crate::property::property;
use crate::volume::Volume;

const CS: u64 = CLUSTER_SIZE as u64;
/// The sector size of the strict volume.
const SECTOR: u64 = 512;
/// Clusters in the synthetic volume image.
const IMAGE_CLUSTERS: u64 = 64;

/// The byte the synthetic volume holds at `offset`. Different at every offset that matters, so
/// reading the wrong place cannot pass for the right one.
fn volume_byte(offset: u64) -> u8 {
    (offset.wrapping_mul(2_654_435_761) >> 13) as u8 ^ (offset >> 12) as u8
}

fn volume_image() -> Vec<u8> {
    (0..IMAGE_CLUSTERS * CS).map(volume_byte).collect()
}

/// One part of a non-resident stream's runs: a length in clusters and the LCN it starts at
/// (`None` is a sparse run).
type Run = (u64, Option<u64>);

/// The mapping pairs of `runs`, with absolute LCNs turned into the deltas the format stores.
fn mapping_pairs(runs: &[Run]) -> Vec<u8> {
    let mut previous = 0i64;
    let deltas: Vec<(u64, Option<i64>)> = runs
        .iter()
        .map(|&(clusters, lcn)| {
            let delta = lcn.map(|lcn| {
                let delta = lcn as i64 - previous;
                previous = lcn as i64;
                delta
            });
            (clusters, delta)
        })
        .collect();
    encode_runs(&deltas)
}

/// A non-resident `$DATA` extent (`name` "" is the default stream) starting at `lowest_vcn`,
/// whose header covers exactly its runs. `size` matters only for VCN 0.
fn data_extent(
    record: &mut [u8],
    offset: usize,
    name: &str,
    lowest_vcn: u64,
    size: u64,
    runs: &[Run],
) -> usize {
    let clusters: u64 = runs.iter().map(|run| run.0).sum();
    add_nonresident_data_runs(
        record,
        offset,
        NtfsAttributeType::Data,
        name,
        lowest_vcn,
        (lowest_vcn + clusters).wrapping_sub(1),
        size,
        &mapping_pairs(runs),
    )
}

/// Where the attribute that starts at `offset` keeps its header fields, for a test to
/// overwrite.
const FLAGS: usize = 12;
const INITIALIZED_SIZE: usize = 56;

fn finish(mut record: Vec<u8>, end: usize) -> Vec<u8> {
    let end = add_end_marker(&mut record, end);
    finish_record(&mut record, end);
    record
}

/// A base record (number 24, sequence 1) holding one non-resident default stream.
fn one_stream(size: u64, runs: &[Run]) -> Vec<Vec<u8>> {
    let mut base = new_record(24, 1, 0);
    let end = data_extent(&mut base, ATTRIBUTES_OFFSET, "", 0, size, runs);
    vec![finish(base, end)]
}

fn base_reference() -> u64 {
    reference(1, 24)
}

/// Opens stream `name` of record 24 over the synthetic volume image.
fn open_24<'i>(
    mft: &Mft,
    name: Option<&str>,
    image: &'i [u8],
) -> NtfsReaderResult<Core<Cursor<&'i [u8]>>> {
    let file = mft.record(24).expect("record 24");
    let name = name.map(std::ffi::OsString::from);
    open(&file, name.as_deref(), Cursor::new(image))
}

fn open_records(records: Vec<Vec<u8>>, image: &[u8]) -> NtfsReaderResult<Core<Cursor<&[u8]>>> {
    open_24(&mft_with(records), None, image)
}

fn read_all<R: Read>(reader: &mut R) -> Vec<u8> {
    let mut data = Vec::new();
    reader.read_to_end(&mut data).expect("read");
    data
}

/// What a stream made of `runs` should read as: the image at the runs' clusters, zeroes for
/// sparse runs and from `initialized_size` on, `size` bytes in all. Worked out from the
/// description, not with the crate.
fn expected(size: u64, initialized_size: u64, runs: &[Run]) -> Vec<u8> {
    let image = volume_image();
    let mut bytes = Vec::new();
    for &(clusters, lcn) in runs {
        for i in 0..clusters * CS {
            bytes.push(lcn.map_or(0, |lcn| image[(lcn * CS + i) as usize]));
        }
    }
    bytes.truncate(size as usize);
    for byte in bytes.iter_mut().skip(initialized_size as usize) {
        *byte = 0;
    }
    bytes
}

fn ntfs_error(err: &io::Error) -> &NtfsReaderError {
    err.get_ref()
        .and_then(|inner| inner.downcast_ref::<NtfsReaderError>())
        .unwrap_or_else(|| panic!("not an NtfsReaderError: {err:?}"))
}

/// The extents tile 0..size exactly, in order.
#[track_caller]
fn assert_tiles<R>(core: &Core<R>) {
    let mut end = 0;
    for extent in &core.extents {
        assert_eq!(extent.stream_offset, end, "{:?}", core.extents);
        assert!(extent.length > 0, "{:?}", core.extents);
        end += extent.length;
    }
    assert_eq!(end, core.size, "{:?}", core.extents);
}

/// A volume that must not be touched: any read or seek fails the test.
struct Untouched;

impl Read for Untouched {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        panic!("the volume was read")
    }
}

impl Seek for Untouched {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        panic!("the volume was sought")
    }
}

fn volume(offset: u64, length: u64) -> StreamExtent {
    StreamExtent {
        stream_offset: 0,
        length,
        location: ExtentLocation::Volume { offset },
    }
}

fn extent(stream_offset: u64, length: u64, location: ExtentLocation) -> StreamExtent {
    StreamExtent {
        stream_offset,
        length,
        location,
    }
}

// Opening a raw volume needs elevated access, and the scratch buffer costs memory: neither is
// paid before the first read that needs the volume.
#[test]
fn the_volume_and_the_scratch_buffer_are_not_touched_before_they_are_needed() {
    let mut volume = LazyVolume::new(Path::new("/no/such/volume"));
    assert!(volume.reader.is_none(), "opened at construction");
    let err = volume.read(&mut [0u8; 1]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    let err = volume.seek(SeekFrom::Start(0)).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);

    let mut scratch = Scratch::new();
    assert_eq!(
        scratch.bytes.capacity(),
        0,
        "allocated before the first use"
    );
    assert_eq!(scratch.take(10).len(), 10);
    assert!(scratch.bytes.len() >= Scratch::SIZE);
    assert!(scratch.take(1 << 30).len() <= Scratch::SIZE);
    assert_eq!(scratch.take(8).as_ptr() as usize % Scratch::ALIGNMENT, 0);
}

#[test]
fn a_resident_stream_is_served_from_the_record() {
    let mut base = new_record(24, 1, 0);
    let end = add_resident_attribute(
        &mut base,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Data,
        2,
        "",
        b"hello, resident",
    );
    let mft = mft_with(vec![finish(base, end)]);
    let file = mft.record(24).unwrap();
    let mut core = open(&file, None, Untouched).unwrap();

    assert_eq!(core.size, 15);
    assert_eq!(core.initialized_size, 15);
    assert_eq!(core.extents, [extent(0, 15, ExtentLocation::Resident)]);
    assert_tiles(&core);
    assert_eq!(read_all(&mut core), b"hello, resident");
    core.seek(SeekFrom::Start(7)).unwrap();
    assert_eq!(read_all(&mut core), b"resident");
}

#[test]
fn an_empty_stream_has_no_extents() {
    let mut base = new_record(24, 1, 0);
    let end = add_resident_attribute(
        &mut base,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Data,
        2,
        "",
        b"",
    );
    let mft = mft_with(vec![finish(base, end)]);
    let file = mft.record(24).unwrap();
    let mut core = open(&file, None, Untouched).unwrap();
    assert!(core.extents.is_empty());
    assert!(read_all(&mut core).is_empty());

    // A non-resident stream of size 0, with runs the size does not use.
    let image = volume_image();
    let mut core = open_records(one_stream(0, &[(2, Some(4))]), &image).unwrap();
    assert!(core.extents.is_empty());
    assert!(read_all(&mut core).is_empty());
}

#[test]
fn one_run_reads_its_clusters_and_stops_at_the_size() {
    let image = volume_image();
    // Three clusters, of which 2.5 are the stream: the junk after the size is never returned.
    let size = 2 * CS + CS / 2;
    let mut core = open_records(one_stream(size, &[(3, Some(10))]), &image).unwrap();

    assert_eq!(core.size, size);
    assert_eq!(core.initialized_size, size);
    assert_eq!(core.cluster_size, CS);
    assert_eq!(core.extents, [volume(10 * CS, size)]);
    assert_tiles(&core);
    let data = read_all(&mut core);
    assert_eq!(data, expected(size, size, &[(3, Some(10))]));
    assert_eq!(data.len() as u64, size);
    // At the end it stays at the end.
    assert_eq!(core.read(&mut [0u8; 16]).unwrap(), 0);
}

#[test]
fn fragmented_runs_out_of_order_read_in_stream_order() {
    let image = volume_image();
    let runs = [(2, Some(30)), (1, Some(5)), (3, Some(20)), (1, Some(6))];
    let size = 7 * CS;
    let mut core = open_records(one_stream(size, &runs), &image).unwrap();

    assert_eq!(
        core.extents,
        [
            extent(0, 2 * CS, ExtentLocation::Volume { offset: 30 * CS }),
            extent(2 * CS, CS, ExtentLocation::Volume { offset: 5 * CS }),
            extent(3 * CS, 3 * CS, ExtentLocation::Volume { offset: 20 * CS }),
            extent(6 * CS, CS, ExtentLocation::Volume { offset: 6 * CS }),
        ]
    );
    assert_tiles(&core);
    assert_eq!(read_all(&mut core), expected(size, size, &runs));
}

#[test]
fn sparse_runs_read_as_zeroes_in_the_middle_and_at_the_end() {
    let image = volume_image();
    let runs = [(2, Some(8)), (3, None), (1, Some(40)), (4, None)];
    let size = 10 * CS;
    let mut core = open_records(one_stream(size, &runs), &image).unwrap();

    assert_eq!(
        core.extents,
        [
            extent(0, 2 * CS, ExtentLocation::Volume { offset: 8 * CS }),
            extent(2 * CS, 3 * CS, ExtentLocation::Sparse),
            extent(5 * CS, CS, ExtentLocation::Volume { offset: 40 * CS }),
            extent(6 * CS, 4 * CS, ExtentLocation::Sparse),
        ]
    );
    assert_tiles(&core);
    let data = read_all(&mut core);
    assert_eq!(data, expected(size, size, &runs));
    assert!(data[(2 * CS) as usize..(5 * CS) as usize]
        .iter()
        .all(|&byte| byte == 0));
    // The sparse tail never touches the volume: a volume that ends before it is enough.
    let short = &image[..(41 * CS) as usize];
    let mut core = open_records(one_stream(size, &runs), short).unwrap();
    assert_eq!(read_all(&mut core), data);
}

#[test]
fn a_sparse_stream_may_be_larger_than_the_volume() {
    // 32 GiB of hole on a volume that is a few clusters: the size is not bounded by the volume.
    let mft = {
        let (_, data, bitmap) = raw_parts(one_stream(32 << 30, &[(8 << 20, None)]));
        let volume = Volume::synthetic(PathBuf::from(VOLUME_PATH), CS, 64 * CS, 1024, 0);
        build_from_parts(volume, data, bitmap)
    };
    let mut core = open_24(&mft, None, &[]).unwrap();
    assert_eq!(core.size, 32 << 30);
    assert_eq!(core.extents, [extent(0, 32 << 30, ExtentLocation::Sparse)]);
    core.seek(SeekFrom::Start((32 << 30) - 4)).unwrap();
    assert_eq!(read_all(&mut core), [0; 4]);
}

#[test]
fn bytes_at_or_beyond_the_initialized_size_read_as_zeroes() {
    let image = volume_image();
    let runs = [(4, Some(12))];
    let size = 4 * CS;
    // Not a multiple of a cluster or of a sector, so the cut falls inside a read.
    let initialized = CS + 100;
    let mut records = one_stream(size, &runs);
    set_initialized_size(&mut records[0], initialized);
    let mut core = open_records(records, &image).unwrap();

    assert_eq!(core.size, size);
    assert_eq!(core.initialized_size, initialized);
    // The extents still say where the clusters are: only reading changes.
    assert_eq!(core.extents, [volume(12 * CS, size)]);
    let data = read_all(&mut core);
    assert_eq!(data, expected(size, initialized, &runs));
    assert!(data[initialized as usize..].iter().all(|&byte| byte == 0));
    assert_ne!(
        data[initialized as usize - 1],
        0,
        "test data has to be non-zero"
    );
    assert_ne!(
        image[(12 * CS + initialized) as usize],
        0,
        "the junk is on the volume"
    );
}

/// Sets the initialized size of the stream in `records[0]`'s first attribute, and re-protects
/// the record.
fn set_initialized_size(record: &mut [u8], initialized: u64) {
    protect_record(record, 0);
    write_u64(record, ATTRIBUTES_OFFSET + INITIALIZED_SIZE, initialized);
    protect_record(record, UPDATE_SEQUENCE_NUMBER);
}

#[test]
fn an_initialized_size_beyond_the_size_is_clamped() {
    let image = volume_image();
    let mut records = one_stream(CS, &[(1, Some(3))]);
    set_initialized_size(&mut records[0], u64::MAX);
    let mut core = open_records(records, &image).unwrap();
    assert_eq!(core.initialized_size, CS);
    assert_eq!(read_all(&mut core), expected(CS, CS, &[(1, Some(3))]));
}

// The zero rule wins over an extent that is lost: nothing was ever written there.
#[test]
fn missing_bytes_beyond_the_initialized_size_read_as_zeroes() {
    let image = volume_image();
    let mut records = one_stream(4 * CS, &[(1, Some(3))]);
    set_initialized_size(&mut records[0], CS);
    let mut core = open_records(records, &image).unwrap();
    assert_eq!(core.extents[1].location, ExtentLocation::Missing);
    let mut want = expected(CS, CS, &[(1, Some(3))]);
    want.resize(4 * CS as usize, 0);
    assert_eq!(read_all(&mut core), want);
}

/// A base record holding the default stream's VCN-0 extent and, in extension records 25 and
/// 26 (listed as `(record, lowest VCN, runs)`), further extents of it.
fn spread_over_records(size: u64, base: &[Run], more: &[(u64, u64, &[Run])]) -> Vec<Vec<u8>> {
    let mut records = one_stream(size, base);
    for &(number, lowest_vcn, runs) in more {
        let mut record = new_record(number, 1, base_reference());
        let end = data_extent(&mut record, ATTRIBUTES_OFFSET, "", lowest_vcn, 0, runs);
        records.push(finish(record, end));
    }
    records
}

#[test]
fn extents_in_the_base_and_extension_records_are_joined_in_vcn_order() {
    let image = volume_image();
    let (a, b, c): (&[Run], &[Run], &[Run]) = (
        &[(2, Some(20)), (1, Some(3))],
        &[(2, Some(9)), (1, None)],
        &[(3, Some(50))],
    );
    let size = 9 * CS;
    // The extent continuing the base sits in the higher-numbered record, the last one in the
    // lower: record order is not VCN order.
    let records = spread_over_records(size, a, &[(25, 6, c), (26, 3, b)]);
    let mut core = open_records(records, &image).unwrap();

    let all: Vec<Run> = [a, b, c].concat();
    assert_eq!(
        core.extents,
        [
            extent(0, 2 * CS, ExtentLocation::Volume { offset: 20 * CS }),
            extent(2 * CS, CS, ExtentLocation::Volume { offset: 3 * CS }),
            extent(3 * CS, 2 * CS, ExtentLocation::Volume { offset: 9 * CS }),
            extent(5 * CS, CS, ExtentLocation::Sparse),
            extent(6 * CS, 3 * CS, ExtentLocation::Volume { offset: 50 * CS }),
        ]
    );
    assert_tiles(&core);
    assert_eq!(read_all(&mut core), expected(size, size, &all));
}

// A record is 1 KiB, but a hostile volume can chain extension records: thousands of two-byte
// sparse runs would be thousands of extents. Adjacent holes merge into one, within a record and
// across records of the same stream.
#[test]
fn adjacent_sparse_runs_are_one_extent() {
    let image = volume_image();
    let mut runs = vec![(1, Some(8))];
    runs.extend(std::iter::repeat_n((1, None), 300));
    runs.push((1, Some(40)));
    let size = 302 * CS;
    let core = open_records(one_stream(size, &runs), &image).unwrap();
    assert_eq!(
        core.extents,
        [
            extent(0, CS, ExtentLocation::Volume { offset: 8 * CS }),
            extent(CS, 300 * CS, ExtentLocation::Sparse),
            extent(301 * CS, CS, ExtentLocation::Volume { offset: 40 * CS }),
        ]
    );
    assert_tiles(&core);

    // A hole that ends one record and one that starts the next are one hole.
    let records = spread_over_records(
        6 * CS,
        &[(1, Some(8)), (2, None)],
        &[(25, 3, &[(1, None), (2, Some(20))])],
    );
    let core = open_records(records, &image).unwrap();
    assert_eq!(
        core.extents,
        [
            extent(0, CS, ExtentLocation::Volume { offset: 8 * CS }),
            extent(CS, 3 * CS, ExtentLocation::Sparse),
            extent(4 * CS, 2 * CS, ExtentLocation::Volume { offset: 20 * CS }),
        ]
    );
    assert_tiles(&core);
}

// A stream never has more than `MAX_EXTENTS` extents: a corrupt volume cannot make one cost
// unbounded memory.
#[test]
fn a_stream_of_too_many_extents_is_an_error() {
    let extent_at = |index: u64| {
        extent(
            index * CS,
            CS,
            ExtentLocation::Volume {
                offset: index * 2 * CS,
            },
        )
    };
    let mut list = ExtentList::new(3);
    for index in 0..3 {
        list.push(extent_at(index)).unwrap();
    }
    let err = list.push(extent_at(3)).unwrap_err();
    assert!(
        matches!(&err, NtfsReaderError::InvalidDataRun { details } if details.contains("too many")),
        "{err:?}"
    );
    // A hole that only grows the last one adds nothing.
    let mut list = ExtentList::new(1);
    list.push(extent(0, CS, ExtentLocation::Sparse)).unwrap();
    list.push(extent(CS, CS, ExtentLocation::Sparse)).unwrap();
    assert_eq!(list.into_vec(), [extent(0, 2 * CS, ExtentLocation::Sparse)]);
    assert_eq!(MAX_EXTENTS, 1 << 22);
}

#[test]
fn a_single_extension_record_continues_the_base() {
    let image = volume_image();
    let records = spread_over_records(5 * CS, &[(2, Some(7))], &[(25, 2, &[(3, Some(1))])]);
    let mut core = open_records(records, &image).unwrap();
    assert_eq!(core.extents.len(), 2);
    assert_eq!(
        read_all(&mut core),
        expected(5 * CS, 5 * CS, &[(2, Some(7)), (3, Some(1))])
    );
}

// A lost extension record: its VCNs become a `Missing` extent. Bytes before it are returned,
// the error names the offset where they stop, and reading resumes after a seek.
#[test]
fn a_missing_middle_extent_is_reported_and_stops_the_read_at_its_offset() {
    let image = volume_image();
    let head: &[Run] = &[(2, Some(4))];
    let tail: &[Run] = &[(2, Some(30))];
    // VCNs 2..5 are on a record that is gone.
    let records = spread_over_records(7 * CS, head, &[(25, 5, tail)]);
    let mut core = open_records(records, &image).unwrap();

    assert_eq!(
        core.extents,
        [
            extent(0, 2 * CS, ExtentLocation::Volume { offset: 4 * CS }),
            extent(2 * CS, 3 * CS, ExtentLocation::Missing),
            extent(5 * CS, 2 * CS, ExtentLocation::Volume { offset: 30 * CS }),
        ]
    );
    assert_tiles(&core);

    // One big read returns what is before the gap, no more.
    let mut buf = vec![0u8; 7 * CS as usize];
    let read = core.read(&mut buf).unwrap();
    assert_eq!(read as u64, 2 * CS);
    assert_eq!(&buf[..read], expected(2 * CS, 2 * CS, head));
    // The next read is the error, at the offset of the first byte it cannot give.
    let err = core.read(&mut buf).unwrap_err();
    assert!(
        matches!(
            ntfs_error(&err),
            NtfsReaderError::StreamExtentMissing { offset } if *offset == 2 * CS
        ),
        "{err:?}"
    );
    // A read that starts inside the gap names its own offset.
    core.seek(SeekFrom::Start(3 * CS + 17)).unwrap();
    let err = core.read(&mut buf).unwrap_err();
    assert!(
        matches!(
            ntfs_error(&err),
            NtfsReaderError::StreamExtentMissing { offset } if *offset == 3 * CS + 17
        ),
        "{err:?}"
    );
    assert_eq!(core.position, 3 * CS + 17, "a failed read does not move");
    // Past the gap it reads again.
    core.seek(SeekFrom::Start(5 * CS)).unwrap();
    assert_eq!(read_all(&mut core), expected(2 * CS, 2 * CS, tail));
    // read_exact across the gap fails, with the same error.
    core.seek(SeekFrom::Start(CS)).unwrap();
    let err = core.read_exact(&mut buf[..(2 * CS) as usize]).unwrap_err();
    assert!(matches!(
        ntfs_error(&err),
        NtfsReaderError::StreamExtentMissing { .. }
    ));
}

// The extents stop short of the size: the last extension record is the lost one.
#[test]
fn extents_that_stop_short_of_the_size_end_in_a_missing_extent() {
    let image = volume_image();
    let mut core = open_records(spread_over_records(6 * CS, &[(2, Some(4))], &[]), &image).unwrap();
    assert_eq!(
        core.extents,
        [
            extent(0, 2 * CS, ExtentLocation::Volume { offset: 4 * CS }),
            extent(2 * CS, 4 * CS, ExtentLocation::Missing),
        ]
    );
    assert_tiles(&core);
    let mut buf = vec![0u8; 6 * CS as usize];
    assert_eq!(core.read(&mut buf).unwrap() as u64, 2 * CS);
    assert!(core.read(&mut buf).is_err());
}

// ---- A deleted file's stream ----
//
// A user recovers from a deleted file what its stream reads, so every shape of stream is opened
// twice, live and after NTFS deletes it (every record freed, see `deleted_mft`); the deleted one
// must match the live one's size, initialized size, extents and bytes. The live read is checked
// against the description first, so two empty reads cannot agree by accident.

/// The records of a file after NTFS deleted it: every one is freed (flag off, sequence up by
/// one, `$BITMAP` bit clear) and nothing else changes, so an extension record still holds the
/// base reference the live base had.
fn deleted_mft(mut records: Vec<Vec<u8>>) -> Mft {
    let numbers: Vec<u64> = (24..24 + records.len() as u64).collect();
    for record in &mut records {
        delete_record(record);
    }
    mft_with_freed(records, &numbers)
}

/// Opens stream `name` of record 24 in `records` as it is live and as it is once deleted, and
/// asserts the two are the same in every respect. Returns the bytes, which are the live ones.
#[track_caller]
fn a_deleted_stream_is_the_live_stream(
    records: Vec<Vec<u8>>,
    name: Option<&str>,
    image: &[u8],
) -> Vec<u8> {
    let count = records.len();
    let deleted = deleted_mft(records.clone());
    let file = deleted.record(24).expect("record 24");
    assert!(
        file.is_deleted() && !file.is_used(),
        "the fixture is not a deleted file"
    );
    assert_eq!(
        file.records().count(),
        count,
        "the records of the deleted file"
    );

    let mut live = open_24(&mft_with(records), name, image).expect("open the live stream");
    let mut gone = open_24(&deleted, name, image).expect("open the deleted stream");
    assert_tiles(&live);
    assert_tiles(&gone);
    assert_eq!(gone.size, live.size, "size");
    assert_eq!(
        gone.initialized_size, live.initialized_size,
        "initialized size"
    );
    assert_eq!(gone.extents, live.extents, "extents");
    let (live_bytes, gone_bytes) = (read_all(&mut live), read_all(&mut gone));
    assert!(
        gone_bytes == live_bytes,
        "the deleted stream reads other bytes"
    );
    live_bytes
}

#[test]
fn a_deleted_stream_spread_over_extension_records_reads_like_the_live_one() {
    let image = volume_image();
    let (a, b, c): (&[Run], &[Run], &[Run]) = (
        &[(2, Some(20)), (1, Some(3))],
        &[(2, Some(9)), (1, None)],
        &[(3, Some(50))],
    );
    let size = 9 * CS;
    // The extension record that continues the base has the higher number, as in the live test.
    let records = spread_over_records(size, a, &[(25, 6, c), (26, 3, b)]);
    let bytes = a_deleted_stream_is_the_live_stream(records, None, &image);
    assert_eq!(bytes, expected(size, size, &[a, b, c].concat()));
}

#[test]
fn a_deleted_fragmented_stream_reads_like_the_live_one() {
    let image = volume_image();
    let runs = [(2, Some(30)), (1, Some(5)), (3, Some(20)), (1, Some(6))];
    let size = 7 * CS - 100;
    let bytes = a_deleted_stream_is_the_live_stream(one_stream(size, &runs), None, &image);
    assert_eq!(bytes, expected(size, size, &runs));
}

#[test]
fn a_deleted_sparse_stream_that_was_not_fully_written_reads_like_the_live_one() {
    let image = volume_image();
    // A hole in the middle, data after it, and an initialized size below the size.
    let runs = [(2, Some(12)), (3, None), (2, Some(40)), (1, None)];
    let (size, initialized) = (8 * CS, 6 * CS + 500);
    let mut records = one_stream(size, &runs);
    set_initialized_size(&mut records[0], initialized);
    let bytes = a_deleted_stream_is_the_live_stream(records, None, &image);
    assert_eq!(bytes, expected(size, initialized, &runs));
    assert!(bytes[initialized as usize..].iter().all(|&byte| byte == 0));
}

#[test]
fn a_deleted_named_stream_reads_like_the_live_one_next_to_a_resident_default() {
    let image = volume_image();
    let mut base = new_record(24, 1, 0);
    let mut end = add_resident_attribute(
        &mut base,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Data,
        2,
        "",
        b"the resident default stream",
    );
    end = data_extent(&mut base, end, "ads", 0, 4 * CS, &[(2, Some(10))]);
    let mut extension = new_record(25, 1, base_reference());
    let extension_end = data_extent(
        &mut extension,
        ATTRIBUTES_OFFSET,
        "ads",
        2,
        0,
        &[(1, None), (1, Some(33))],
    );
    let records = vec![finish(base, end), finish(extension, extension_end)];

    let default = a_deleted_stream_is_the_live_stream(records.clone(), None, &image);
    assert_eq!(default, b"the resident default stream");
    let ads = a_deleted_stream_is_the_live_stream(records, Some("ads"), &image);
    assert_eq!(
        ads,
        expected(4 * CS, 4 * CS, &[(2, Some(10)), (1, None), (1, Some(33))])
    );
}

// A file shrunk after its runs spilled into an extension record: NTFS frees that record while
// the base is alive, and it should be empty. If it were not, its `$DATA` segment (VCN 1 and up
// here, 50 clusters at LCN 10) would still name the base, and deleting the base would attribute
// it to the deleted file. The stream is what the base says (one cluster); the stale runs, beyond
// its size, are not part of it. The measured behaviour of the freed record is in
// `tests/deleted_metadata_tests.rs`; this pins what the crate does if it holds runs anyway.
#[test]
fn a_stale_extent_left_in_a_freed_extension_record_is_not_part_of_the_deleted_stream() {
    let image = volume_image();
    let mut base = new_record(24, 1, 0);
    let end = data_extent(&mut base, ATTRIBUTES_OFFSET, "", 0, CS, &[(1, Some(3))]);
    let mut stale = new_record(25, 1, base_reference());
    let stale_end = data_extent(&mut stale, ATTRIBUTES_OFFSET, "", 1, 0, &[(50, Some(10))]);
    let records = vec![finish(base, end), finish(stale, stale_end)];

    let deleted = deleted_mft(records);
    let file = deleted.record(24).expect("record 24");
    assert!(file.is_deleted());
    assert_eq!(
        file.records().count(),
        2,
        "the freed extension record that names the base is attributed to it"
    );
    let mut stream = open_24(&deleted, None, &image).expect("open the deleted stream");
    assert_tiles(&stream);
    assert_eq!(stream.size, CS);
    assert_eq!(
        stream.extents,
        [StreamExtent {
            stream_offset: 0,
            length: CS,
            location: ExtentLocation::Volume { offset: 3 * CS },
        }],
        "the stale runs are not extents of the stream"
    );
    assert_eq!(read_all(&mut stream), expected(CS, CS, &[(1, Some(3))]));
}

// The extension record with the rest of the stream is gone: the deleted file has the same
// missing extent the live one would, and reads the same bytes up to it.
#[test]
fn a_deleted_stream_with_a_lost_extent_reads_like_the_live_one_up_to_it() {
    let image = volume_image();
    let records = spread_over_records(6 * CS, &[(2, Some(4))], &[]);
    let count = records.len();
    let deleted = deleted_mft(records.clone());
    assert!(deleted.record(24).unwrap().is_deleted());
    let mut live = open_24(&mft_with(records), None, &image).unwrap();
    let mut gone = open_24(&deleted, None, &image).unwrap();
    assert_eq!(count, 1);
    assert_eq!(gone.extents, live.extents);
    assert_eq!(gone.extents[1].location, ExtentLocation::Missing);
    for core in [&mut live, &mut gone] {
        let mut bytes = Vec::new();
        let err = core.read_to_end(&mut bytes).unwrap_err();
        assert!(matches!(
            ntfs_error(&err),
            NtfsReaderError::StreamExtentMissing { offset } if *offset == 2 * CS
        ));
        assert_eq!(bytes, expected(2 * CS, 2 * CS, &[(2, Some(4))]));
    }
}

#[test]
fn a_deleted_resident_stream_reads_like_the_live_one() {
    let image = volume_image();
    let mut base = new_record(24, 1, 0);
    let end = add_resident_attribute(
        &mut base,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Data,
        2,
        "",
        b"a hundred bytes or so",
    );
    let bytes = a_deleted_stream_is_the_live_stream(vec![finish(base, end)], None, &image);
    assert_eq!(bytes, b"a hundred bytes or so");
}

#[test]
fn a_stream_without_its_vcn_0_extent_is_an_error() {
    let image = volume_image();
    // Only the extension record survived: no size, nothing to bound a read by.
    let mut record = new_record(24, 1, 0);
    let end = data_extent(&mut record, ATTRIBUTES_OFFSET, "", 4, 0, &[(2, Some(4))]);
    let err = open_records(vec![finish(record, end)], &image).unwrap_err();
    assert!(
        matches!(err, NtfsReaderError::InvalidDataRun { details } if details.contains("VCN-0")),
        "{err:?}"
    );
}

#[test]
fn overlapping_and_repeated_extents_are_errors() {
    let image = volume_image();
    let a: &[Run] = &[(3, Some(4))];
    // Starts at VCN 2, inside the base's 0..3.
    let overlap = spread_over_records(6 * CS, a, &[(25, 2, &[(2, Some(9))])]);
    // The same VCN twice, as a stale copy of an extension record would give.
    let repeated = spread_over_records(
        6 * CS,
        a,
        &[(25, 3, &[(3, Some(9))]), (26, 3, &[(3, Some(9))])],
    );
    for records in [overlap, repeated] {
        let err = open_records(records, &image).unwrap_err();
        assert!(
            matches!(err, NtfsReaderError::InvalidDataRun { details } if details.contains("overlap")),
            "{err:?}"
        );
    }
}

#[test]
fn a_stream_is_chosen_by_name() {
    let image = volume_image();
    let mut base = new_record(24, 1, 0);
    let mut end = ATTRIBUTES_OFFSET;
    // Default resident, one alternate stream resident, one not, in an order that does not put
    // the default first.
    end = add_resident_attribute(
        &mut base,
        end,
        NtfsAttributeType::Data,
        3,
        "note",
        b"a note",
    );
    end = add_resident_attribute(
        &mut base,
        end,
        NtfsAttributeType::Data,
        2,
        "",
        b"the contents",
    );
    end = data_extent(&mut base, end, "big", 0, 2 * CS, &[(2, Some(6))]);
    let mft = mft_with(vec![finish(base, end)]);

    let mut default = open_24(&mft, None, &image).unwrap();
    assert_eq!(read_all(&mut default), b"the contents");
    let mut note = open_24(&mft, Some("note"), &image).unwrap();
    assert_eq!(read_all(&mut note), b"a note");
    let mut big = open_24(&mft, Some("big"), &image).unwrap();
    assert_eq!(
        read_all(&mut big),
        expected(2 * CS, 2 * CS, &[(2, Some(6))])
    );

    for missing in [Some("nope"), Some("Note"), Some("")] {
        let err = open_24(&mft, missing, &image).unwrap_err();
        assert!(
            matches!(&err, NtfsReaderError::StreamNotFound { name } if name.as_deref() == missing.map(std::ffi::OsStr::new)),
            "{missing:?}: {err:?}"
        );
    }
    assert!(err_message(&open_24(&mft, Some("nope"), &image).unwrap_err()).contains("nope"));
}

fn err_message(err: &NtfsReaderError) -> String {
    err.to_string()
}

#[test]
fn a_file_without_a_default_stream_has_none_to_open() {
    let image = volume_image();
    let mut base = new_record(24, 1, 0);
    let end = add_resident_attribute(
        &mut base,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Data,
        2,
        "only-named",
        b"x",
    );
    let mft = mft_with(vec![finish(base, end)]);
    let err = open_24(&mft, None, &image).unwrap_err();
    assert!(
        matches!(err, NtfsReaderError::StreamNotFound { name: None }),
        "{err:?}"
    );
}

#[test]
fn a_named_stream_is_told_apart_by_the_extents_of_its_own_name() {
    let image = volume_image();
    // Both streams have a VCN-0 extent and each continues in its own record: their extents must
    // not merge with each other's.
    let mut base = new_record(24, 1, 0);
    let mut end = data_extent(&mut base, ATTRIBUTES_OFFSET, "", 0, 2 * CS, &[(1, Some(2))]);
    end = data_extent(&mut base, end, "ads", 0, 2 * CS, &[(1, Some(10))]);
    let mut ext_default = new_record(25, 1, base_reference());
    let e1 = data_extent(
        &mut ext_default,
        ATTRIBUTES_OFFSET,
        "",
        1,
        0,
        &[(1, Some(3))],
    );
    let mut ext_ads = new_record(26, 1, base_reference());
    let e2 = data_extent(
        &mut ext_ads,
        ATTRIBUTES_OFFSET,
        "ads",
        1,
        0,
        &[(1, Some(11))],
    );
    let mft = mft_with(vec![
        finish(base, end),
        finish(ext_default, e1),
        finish(ext_ads, e2),
    ]);

    let mut default = open_24(&mft, None, &image).unwrap();
    assert_eq!(
        read_all(&mut default),
        expected(2 * CS, 2 * CS, &[(1, Some(2)), (1, Some(3))])
    );
    let mut ads = open_24(&mft, Some("ads"), &image).unwrap();
    assert_eq!(
        read_all(&mut ads),
        expected(2 * CS, 2 * CS, &[(1, Some(10)), (1, Some(11))])
    );
}

#[test]
fn a_resident_stream_with_other_attributes_of_its_name_is_an_error() {
    let image = volume_image();
    let mut two_resident = new_record(24, 1, 0);
    let mut end = add_resident_attribute(
        &mut two_resident,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Data,
        2,
        "",
        b"one",
    );
    end = add_resident_attribute(
        &mut two_resident,
        end,
        NtfsAttributeType::Data,
        3,
        "",
        b"two",
    );
    let mut mixed = new_record(24, 1, 0);
    let mut end2 = add_resident_attribute(
        &mut mixed,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Data,
        2,
        "",
        b"one",
    );
    end2 = data_extent(&mut mixed, end2, "", 0, CS, &[(1, Some(2))]);
    for record in [finish(two_resident, end), finish(mixed, end2)] {
        let err = open_records(vec![record], &image).unwrap_err();
        assert!(
            matches!(err, NtfsReaderError::InvalidDataRun { .. }),
            "{err:?}"
        );
    }
}

// CompactOS: the default stream is a sparse placeholder sized to the file; real content is in
// `WofCompressedData`. Reading the placeholder would return zeroes for a file that has data.
#[test]
fn the_default_stream_of_a_wof_file_is_refused_and_the_wof_stream_reads_raw() {
    let image = volume_image();
    let mut base = new_record(24, 1, 0);
    let mut end = data_extent(&mut base, ATTRIBUTES_OFFSET, "", 0, 3 * CS, &[(3, None)]);
    end = data_extent(&mut base, end, "WofCompressedData", 0, CS, &[(1, Some(9))]);
    end = add_resident_attribute(&mut base, end, NtfsAttributeType::Data, 7, "other", b"x");
    let mft = mft_with(vec![finish(base, end)]);

    let err = open_24(&mft, None, &image).unwrap_err();
    assert!(
        matches!(err, NtfsReaderError::WofCompressedStream),
        "{err:?}"
    );
    let mut wof = open_24(&mft, Some("WofCompressedData"), &image).unwrap();
    assert_eq!(read_all(&mut wof), expected(CS, CS, &[(1, Some(9))]));
    assert_eq!(
        read_all(&mut open_24(&mft, Some("other"), &image).unwrap()),
        b"x"
    );

    // The rule is the stream, not the reparse tag or the case: a file with the stream in
    // an extension record is refused too, and a stream named differently is an ordinary one.
    let mut base = new_record(24, 1, 0);
    let end = data_extent(&mut base, ATTRIBUTES_OFFSET, "", 0, CS, &[(1, None)]);
    let mut extension = new_record(25, 1, base_reference());
    let e = data_extent(
        &mut extension,
        ATTRIBUTES_OFFSET,
        "WofCompressedData",
        0,
        CS,
        &[(1, Some(2))],
    );
    let mft = mft_with(vec![finish(base, end), finish(extension, e)]);
    assert!(matches!(
        open_24(&mft, None, &image).unwrap_err(),
        NtfsReaderError::WofCompressedStream
    ));
    let mut base = new_record(24, 1, 0);
    let mut end = data_extent(&mut base, ATTRIBUTES_OFFSET, "", 0, CS, &[(1, Some(1))]);
    end = data_extent(&mut base, end, "wofcompresseddata", 0, CS, &[(1, Some(2))]);
    let mft = mft_with(vec![finish(base, end)]);
    assert!(open_24(&mft, None, &image).is_ok());
}

#[test]
fn compressed_and_encrypted_streams_are_refused() {
    let image = volume_image();
    let with_flags = |flags: u16, resident: bool| {
        let mut record = new_record(24, 1, 0);
        let end = if resident {
            add_resident_attribute(
                &mut record,
                ATTRIBUTES_OFFSET,
                NtfsAttributeType::Data,
                2,
                "",
                b"data",
            )
        } else {
            data_extent(&mut record, ATTRIBUTES_OFFSET, "", 0, CS, &[(1, Some(2))])
        };
        write_u16(&mut record, ATTRIBUTES_OFFSET + FLAGS, flags);
        vec![finish(record, end)]
    };

    // LZNT1 compressed, and a compression format other than LZNT1: still not raw.
    for flags in [0x0001, 0x0002] {
        let err = open_records(with_flags(flags, false), &image).unwrap_err();
        assert!(
            matches!(err, NtfsReaderError::CompressedStream),
            "{flags:#x}: {err:?}"
        );
    }
    for resident in [false, true] {
        for flags in [0x4000, 0x4000 | 0x8000] {
            let err = open_records(with_flags(flags, resident), &image).unwrap_err();
            assert!(
                matches!(err, NtfsReaderError::EncryptedStream),
                "{flags:#x}: {err:?}"
            );
        }
    }
    // A sparse stream is fine, and so is the compression flag on data that is resident (a
    // small file in a compressed directory carries it, and its bytes are not compressed).
    let mut sparse = open_records(with_flags(0x8000, false), &image).unwrap();
    assert_eq!(read_all(&mut sparse), expected(CS, CS, &[(1, Some(2))]));
    let mut resident = open_records(with_flags(0x0001, true), &image).unwrap();
    assert_eq!(read_all(&mut resident), b"data");

    // Refusing one stream leaves the file's others readable.
    let mut record = new_record(24, 1, 0);
    let mut end = data_extent(&mut record, ATTRIBUTES_OFFSET, "", 0, CS, &[(1, Some(2))]);
    write_u16(&mut record, ATTRIBUTES_OFFSET + FLAGS, 0x0001);
    end = add_resident_attribute(
        &mut record,
        end,
        NtfsAttributeType::Data,
        3,
        "plain",
        b"fine",
    );
    let mft = mft_with(vec![finish(record, end)]);
    assert!(matches!(
        open_24(&mft, None, &image).unwrap_err(),
        NtfsReaderError::CompressedStream
    ));
    assert_eq!(
        read_all(&mut open_24(&mft, Some("plain"), &image).unwrap()),
        b"fine"
    );
}

/// A non-resident `$DATA` attribute whose run list is exactly `runs`: no terminator is added,
/// so the list can be cut short or run to the end of the attribute. `runs` must be a
/// multiple of 8 bytes long.
fn raw_extent(record: &mut [u8], offset: usize, size: u64, runs: &[u8]) -> usize {
    assert_eq!(runs.len() % 8, 0);
    let length = 64 + runs.len();
    write_u32(record, offset, NtfsAttributeType::Data as u32);
    write_u32(record, offset + 4, length as u32);
    record[offset + 8] = 1;
    write_u16(record, offset + 10, 64);
    write_u64(record, offset + 24, 1);
    write_u16(record, offset + 32, 64);
    write_u64(record, offset + 40, size);
    write_u64(record, offset + 48, size);
    write_u64(record, offset + 56, size);
    record[offset + 64..offset + length].copy_from_slice(runs);
    offset + length
}

#[test]
fn corrupt_runs_are_errors_not_panics() {
    let image = volume_image();
    let padded = |bytes: &[u8]| {
        let mut bytes = bytes.to_vec();
        bytes.resize(bytes.len().next_multiple_of(8), 0);
        bytes
    };
    let cases: [(&str, Vec<u8>); 8] = [
        // Two runs that fill the attribute to its last byte: nothing ends the list.
        (
            "unterminated",
            vec![0x21, 0x01, 0x04, 0x00, 0x21, 0x01, 0x02, 0x00],
        ),
        // The second run wants 4 offset bytes and 3 are left.
        (
            "cut short",
            vec![0x11, 0x02, 0x04, 0x41, 0x02, 0x05, 0x06, 0x07],
        ),
        ("zero width length", padded(&[0x10, 0x02, 0x04])),
        ("zero cluster count", padded(&[0x11, 0x00, 0x04])),
        (
            "length field wider than 8",
            padded(&[0x19, 1, 2, 3, 4, 5, 6, 7, 8, 9]),
        ),
        (
            "offset field wider than 8",
            padded(&[0x91, 0x02, 1, 2, 3, 4, 5, 6, 7, 8, 9]),
        ),
        // The first run starts before the start of the volume.
        ("negative first LCN", padded(&[0x11, 0x02, 0xff])),
        // 0x7fff_ffff_ffff_ffff clusters of 4 KiB do not fit in 64 bits of bytes.
        (
            "length overflow",
            padded(&[0x81, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f, 0x02]),
        ),
    ];
    for (what, runs) in cases {
        let mut record = new_record(24, 1, 0);
        let end = raw_extent(&mut record, ATTRIBUTES_OFFSET, 2 * CS, &runs);
        let err = open_records(vec![finish(record, end)], &image).unwrap_err();
        assert!(
            matches!(err, NtfsReaderError::InvalidDataRun { .. }),
            "{what}: {err:?}"
        );
    }

    // A well-formed list in the same builder reads fine: the cases above fail for their own
    // reason.
    let mut record = new_record(24, 1, 0);
    let end = raw_extent(
        &mut record,
        ATTRIBUTES_OFFSET,
        2 * CS,
        &padded(&[0x11, 0x02, 0x04]),
    );
    let mut core = open_records(vec![finish(record, end)], &image).unwrap();
    assert_eq!(
        read_all(&mut core),
        expected(2 * CS, 2 * CS, &[(2, Some(4))])
    );
}

#[test]
fn a_header_that_disagrees_with_its_runs_is_an_error() {
    let image = volume_image();
    // The header covers VCNs 0..=5, the runs are 2 clusters.
    let mut record = new_record(24, 1, 0);
    let end = add_nonresident_data_runs(
        &mut record,
        ATTRIBUTES_OFFSET,
        NtfsAttributeType::Data,
        "",
        0,
        5,
        2 * CS,
        &mapping_pairs(&[(2, Some(4))]),
    );
    let err = open_records(vec![finish(record, end)], &image).unwrap_err();
    assert!(
        matches!(err, NtfsReaderError::InvalidDataRun { details } if details.contains("VCN range")),
        "{err:?}"
    );
}

// The extension record that is left starts below VCN 0: nothing says where the stream starts.
#[test]
fn an_extent_starting_below_vcn_0_is_an_error() {
    let image = volume_image();
    let mut record = new_record(24, 1, 0);
    let end = data_extent(&mut record, ATTRIBUTES_OFFSET, "", 0, CS, &[(1, Some(4))]);
    write_u64(&mut record, ATTRIBUTES_OFFSET + 16, u64::MAX);
    let err = open_records(vec![finish(record, end)], &image).unwrap_err();
    assert!(
        matches!(err, NtfsReaderError::InvalidDataRun { .. }),
        "{err:?}"
    );
}

#[test]
fn a_run_outside_the_volume_is_an_error() {
    let with_volume = |runs: &[Run], size: u64| {
        let (_, data, bitmap) = raw_parts(one_stream(size, runs));
        let volume = Volume::synthetic(PathBuf::from(VOLUME_PATH), CS, 16 * CS, 1024, 0);
        build_from_parts(volume, data, bitmap)
    };
    let image = volume_image();
    // Fits exactly: ends at the last byte of the volume.
    let mft = with_volume(&[(2, Some(14))], 2 * CS);
    assert_eq!(
        read_all(&mut open_24(&mft, None, &image).unwrap()),
        expected(2 * CS, 2 * CS, &[(2, Some(14))])
    );
    // One cluster too far, and starting past the end, and a size that overflows.
    for runs in [
        &[(2, Some(15))][..],
        &[(1, Some(16))],
        &[(1, Some(1 << 50))],
        &[(1, Some(2)), (1u64 << 60, Some(3))],
    ] {
        let mft = with_volume(runs, 2 * CS);
        let err = open_24(&mft, None, &image).unwrap_err();
        assert!(
            matches!(err, NtfsReaderError::InvalidDataRun { details } if details.contains("outside the volume") || details.contains("overflow")),
            "{runs:?}: {err:?}"
        );
    }
}

#[test]
fn an_absurd_size_costs_nothing_and_fails_at_the_read() {
    let image = volume_image();
    // The size field says 16 EiB and the runs are two clusters: a Missing tail, no allocation.
    let size = !(CS - 1);
    let mut core = open_records(one_stream(size, &[(2, Some(4))]), &image).unwrap();
    assert_eq!(core.extents.len(), 2);
    assert_eq!(
        core.extents[1],
        extent(2 * CS, size - 2 * CS, ExtentLocation::Missing)
    );
    assert_tiles(&core);
    let mut buf = vec![0u8; 3 * CS as usize];
    assert_eq!(core.read(&mut buf).unwrap() as u64, 2 * CS);
    assert!(core.read(&mut buf).is_err());
    // Seeking around at that size does not overflow.
    assert_eq!(core.seek(SeekFrom::End(0)).unwrap(), size);
    assert_eq!(core.seek(SeekFrom::End(CS as i64 - 1)).unwrap(), u64::MAX);
    assert!(core.seek(SeekFrom::Current(1)).is_err());
    assert_eq!(core.stream_position().unwrap(), u64::MAX);
    assert_eq!(core.read(&mut buf).unwrap(), 0);
}

#[test]
fn seeks_follow_the_std_rules() {
    let image = volume_image();
    let runs = [(2, Some(5)), (1, None)];
    let size = 3 * CS - 10;
    let all = expected(size, size, &runs);
    let mut core = open_records(one_stream(size, &runs), &image).unwrap();

    assert_eq!(core.seek(SeekFrom::Start(100)).unwrap(), 100);
    let mut buf = [0u8; 20];
    core.read_exact(&mut buf).unwrap();
    assert_eq!(buf, all[100..120]);
    assert_eq!(core.stream_position().unwrap(), 120);
    assert_eq!(core.seek(SeekFrom::Current(-20)).unwrap(), 100);
    assert_eq!(
        core.seek(SeekFrom::Current(2 * CS as i64)).unwrap(),
        100 + 2 * CS
    );
    assert_eq!(core.seek(SeekFrom::End(-30)).unwrap(), size - 30);
    assert_eq!(read_all(&mut core), all[all.len() - 30..]);
    assert_eq!(core.seek(SeekFrom::End(0)).unwrap(), size);

    // Past the end is allowed and reads nothing, and coming back works.
    assert_eq!(core.seek(SeekFrom::End(5)).unwrap(), size + 5);
    assert_eq!(core.read(&mut buf).unwrap(), 0);
    assert_eq!(core.seek(SeekFrom::Start(1 << 40)).unwrap(), 1 << 40);
    assert_eq!(core.read(&mut buf).unwrap(), 0);
    assert_eq!(core.seek(SeekFrom::Start(3)).unwrap(), 3);
    core.read_exact(&mut buf).unwrap();
    assert_eq!(buf, all[3..23]);

    // Before the start is an error, and leaves the position where it was.
    assert_eq!(core.seek(SeekFrom::Start(50)).unwrap(), 50);
    for pos in [
        SeekFrom::Current(-51),
        SeekFrom::End(-(size as i64) - 1),
        SeekFrom::Current(i64::MIN),
        SeekFrom::End(i64::MIN),
    ] {
        let err = core.seek(pos).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{pos:?}");
        assert_eq!(core.stream_position().unwrap(), 50, "{pos:?}");
    }
    assert_eq!(core.seek(SeekFrom::Current(-50)).unwrap(), 0);
    assert_eq!(core.seek(SeekFrom::End(-(size as i64))).unwrap(), 0);
}

#[test]
fn read_sizes_that_cross_run_boundaries_read_the_same_bytes() {
    let image = volume_image();
    let runs = [
        (1, Some(20)),
        (1, Some(2)),
        (2, None),
        (2, Some(33)),
        (1, Some(1)),
    ];
    let size = 7 * CS - 123;
    let initialized = 5 * CS + 7;
    let mut records = one_stream(size, &runs);
    set_initialized_size(&mut records[0], initialized);
    let all = expected(size, initialized, &runs);
    let core = || open_records(records.clone(), &image).unwrap();

    // One huge buffer, larger than the stream.
    let mut huge = vec![0xaau8; size as usize + 5000];
    let mut c = core();
    assert_eq!(c.read(&mut huge).unwrap(), size as usize);
    assert_eq!(huge[..size as usize], all[..]);
    assert_eq!(
        huge[size as usize..],
        vec![0xaau8; 5000][..],
        "nothing written past the size"
    );
    assert_eq!(c.read(&mut huge).unwrap(), 0);

    // One byte at a time.
    let mut c = core();
    let mut bytes = Vec::new();
    let mut one = [0u8];
    while c.read(&mut one).unwrap() == 1 {
        bytes.push(one[0]);
    }
    assert_eq!(bytes, all);

    // Every size from 1 to a cluster and a half around the run boundaries.
    for chunk in (1..=CS as usize + CS as usize / 2)
        .step_by(97)
        .chain([4095, 4096, 4097, 8191, 8192, 8193])
    {
        let mut c = core();
        let mut bytes = Vec::new();
        let mut buf = vec![0u8; chunk];
        loop {
            let read = c.read(&mut buf).unwrap();
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..read]);
        }
        assert_eq!(bytes, all, "chunk {chunk}");
    }
}

/// Counts seeks and reads of the volume, and can hand out at most `max` bytes per read.
struct Spy<'i> {
    inner: Cursor<&'i [u8]>,
    seeks: usize,
    max: usize,
}

impl Read for Spy<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = buf.len().min(self.max);
        self.inner.read(&mut buf[..n])
    }
}

impl Seek for Spy<'_> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.seeks += 1;
        self.inner.seek(pos)
    }
}

fn spy_core<'i>(records: Vec<Vec<u8>>, image: &'i [u8], max: usize) -> Core<Spy<'i>> {
    let mft = mft_with(records);
    let file = mft.record(24).unwrap();
    open(
        &file,
        None,
        Spy {
            inner: Cursor::new(image),
            seeks: 0,
            max,
        },
    )
    .unwrap()
}

fn seeks(core: &Core<Spy<'_>>) -> usize {
    match &core.backing {
        Backing::Volume { reader, .. } => reader.seeks,
        Backing::Resident(_) => unreachable!(),
    }
}

// Reading on from where the last read stopped costs no seek, nor does a read inside the window
// the reader already holds in memory, whatever the stream position. A read outside that window
// costs one seek.
#[test]
fn reading_on_from_the_last_read_does_not_seek_the_volume() {
    let image = volume_image();
    let mut core = spy_core(one_stream(3 * CS, &[(3, Some(10))]), &image, usize::MAX);
    let mut buf = [0u8; 1000];
    while core.read(&mut buf).unwrap() > 0 {}
    assert_eq!(seeks(&core), 1);

    // Contiguous runs in different records: the second starts where the first ended.
    let mut core = spy_core(
        one_stream(4 * CS, &[(2, Some(10)), (2, Some(12))]),
        &image,
        usize::MAX,
    );
    let mut buf = [0u8; 1000];
    while core.read(&mut buf).unwrap() > 0 {}
    assert_eq!(seeks(&core), 1);

    // Seeking back into what was read is free.
    core.seek(SeekFrom::Start(5)).unwrap();
    core.read_exact(&mut buf).unwrap();
    assert_eq!(seeks(&core), 1);

    // A run far from the window is another span, and costs one.
    let mut core = spy_core(
        one_stream(4 * CS, &[(2, Some(10)), (2, Some(50))]),
        &image,
        usize::MAX,
    );
    while core.read(&mut buf).unwrap() > 0 {}
    assert_eq!(seeks(&core), 2);
}

#[test]
fn a_volume_that_returns_short_reads_is_read_in_full() {
    let image = volume_image();
    let runs = [(2, Some(7)), (1, None), (2, Some(1))];
    let size = 5 * CS;
    let mut core = spy_core(one_stream(size, &runs), &image, 3);
    assert_eq!(read_all(&mut core), expected(size, size, &runs));
}

#[test]
fn a_volume_that_ends_early_or_fails_is_an_error() {
    let image = volume_image();
    // The run is past the end of the (unbounded, size 0) synthetic volume's bytes.
    let short = &image[..(4 * CS) as usize];
    let mut core = open_records(one_stream(2 * CS, &[(2, Some(10))]), short).unwrap();
    let err = core.read(&mut [0u8; 16]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);

    // A read that fails half way returns what came before and then the failure.
    struct Fails(Cursor<Vec<u8>>);
    impl Read for Fails {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0.position() >= CS {
                return Err(io::Error::other("bad sector"));
            }
            let n = buf.len().min((CS - self.0.position()) as usize);
            self.0.read(&mut buf[..n])
        }
    }
    impl Seek for Fails {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.0.seek(pos)
        }
    }
    let mft = mft_with(one_stream(3 * CS, &[(3, Some(0))]));
    let file = mft.record(24).unwrap();
    let mut core = open(&file, None, Fails(Cursor::new(image.clone()))).unwrap();
    let mut buf = vec![0u8; 3 * CS as usize];
    assert_eq!(core.read(&mut buf).unwrap() as u64, CS);
    assert_eq!(buf[..CS as usize], image[..CS as usize]);
    assert_eq!(core.read(&mut buf).unwrap_err().to_string(), "bad sector");
    assert_eq!(core.position, CS);
}

/// A raw volume handle, as far as the tests know one. Enforced: every read starts on a multiple
/// of `CS` (the crate's own alignment, stricter than the sector size a volume needs), is a whole
/// number of sectors (`SECTOR`) long, and lands in a page-aligned buffer (stricter than Windows
/// requires, since the reader owns its buffer). Windows fails any other read with
/// ERROR_INVALID_PARAMETER; a `Cursor` accepts anything, so only this volume can catch one.
///
/// A read crossing the end of the volume is an ERROR here, stricter than Windows: measured on a
/// raw handle, such a read returns the bytes up to the end (a short read), and a `Cursor` does
/// the same, so neither could show whether the reader clamps its reads. A short read is harmless
/// only to a reader that copes with one; failing the crossing read makes the clamp in [`Core`]
/// (no span past the end of the volume) what keeps the tests green. A read wholly past the end
/// returns 0 bytes, as on Windows; a read of 4 GiB or more is refused.
struct Strict {
    inner: Cursor<Vec<u8>>,
}

impl Read for Strict {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let (at, len) = (self.inner.position(), buf.len() as u64);
        let address = buf.as_ptr() as usize;
        if !at.is_multiple_of(CS)
            || !len.is_multiple_of(SECTOR)
            || !address.is_multiple_of(Scratch::ALIGNMENT)
        {
            return Err(io::Error::other(format!(
                "unaligned volume read: {len} bytes at {at} into {address:#x} (os error 87)"
            )));
        }
        // std clamps the length handed to Windows to `u32::MAX`, not a multiple of the sector
        // size: a read of 4 GiB or more fails as unaligned.
        if len > u64::from(u32::MAX) {
            return Err(io::Error::other(
                "a read of 4 GiB or more (os error 87)".to_string(),
            ));
        }
        // Windows returns a short read for one crossing the end of the volume; here it fails, so
        // the reader's own clamp is what the tests depend on (see the type's doc comment).
        let end = self.inner.get_ref().len() as u64;
        if at < end && at + len > end {
            return Err(io::Error::other(format!(
                "a volume read of {len} bytes at {at} crosses the end of the volume at {end}"
            )));
        }
        // Wholly past the end it is 0 bytes (std maps ERROR_HANDLE_EOF to Ok(0)), as for a
        // `Cursor`.
        self.inner.read(buf)
    }
}

impl Seek for Strict {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// A stream of record 24 read straight from a strict volume of the image: the reader owns
/// its alignment, so nothing sits between it and the volume. The volume is as big as the image,
/// which is how the reader knows where the end is.
fn strict_core(records: Vec<Vec<u8>>, image: &[u8]) -> Core<Strict> {
    let (_, data, bitmap) = raw_parts(records);
    let volume = Volume::synthetic(
        PathBuf::from(VOLUME_PATH),
        CS,
        image.len() as u64,
        RECORD_SIZE as u64,
        0,
    );
    let mft = build_from_parts(volume, data, bitmap);
    let file = mft.record(24).unwrap();
    let strict = Strict {
        inner: Cursor::new(image.to_vec()),
    };
    open(&file, None, strict).unwrap()
}

// The one property a `Cursor` cannot check: through the real stack, no read reaches the volume
// unaligned, whatever the stream offset, buffer size or seek pattern. (A 20000-byte stream read
// at 6666 with a 10000-byte buffer failed on a real volume with os error 87.)
#[test]
fn every_read_that_reaches_the_volume_is_sector_aligned() {
    let image = volume_image();
    let runs = [(3, Some(5)), (2, None), (4, Some(20)), (1, Some(2))];
    let size = 10 * CS - 777;
    let records = one_stream(size, &runs);
    let all = expected(size, size, &runs);

    // The failing case: seek into the middle of a cluster, then a buffer of 10000.
    let mut core = strict_core(one_stream(20_000, &[(5, Some(9))]), &image);
    let want = expected(20_000, 20_000, &[(5, Some(9))]);
    let mut buf = vec![0u8; 20_000];
    core.read_exact(&mut buf).unwrap();
    assert_eq!(buf, want);
    core.seek(SeekFrom::Start(6666)).unwrap();
    let mut buf = vec![0u8; 10_000];
    core.read_exact(&mut buf).unwrap();
    assert_eq!(buf, want[6666..16_666]);

    // Every start and a spread of buffer sizes, sequentially after a seek.
    for start in (0..size).step_by(511).chain([size - 1, CS - 1, CS, CS + 1]) {
        for len in [1, 100, 4095, 4096, 4097, 8191, 8192, 8193, 10_000, 40_000] {
            let mut core = strict_core(records.clone(), &image);
            core.seek(SeekFrom::Start(start)).unwrap();
            let mut buf = vec![0u8; len];
            let mut got = 0;
            while got < len {
                match core.read(&mut buf[got..]).unwrap() {
                    0 => break,
                    n => got += n,
                }
            }
            let end = (start as usize + len).min(all.len());
            assert_eq!(
                buf[..got],
                all[start as usize..end],
                "start {start} len {len}"
            );
        }
    }
}

// The same property over random layouts, sizes, seeks and buffer sizes: what reaches the volume
// is always aligned, and the bytes are right too.
#[test]
fn random_reads_through_a_strict_volume_are_aligned_and_right() {
    let image = volume_image();
    property(|u| {
        let mut runs: Vec<Run> = Vec::new();
        for _ in 0..u.int_in_range(1..=6)? {
            let clusters = u.int_in_range(1..=6u64)?;
            let lcn = if u.ratio(1, 5)? {
                None
            } else {
                Some(u.int_in_range(0..=IMAGE_CLUSTERS - clusters)?)
            };
            runs.push((clusters, lcn));
        }
        let total: u64 = runs.iter().map(|run| run.0).sum::<u64>() * CS;
        let size = u.int_in_range(1..=total)?;
        let all = expected(size, size, &runs);
        let mut core = strict_core(one_stream(size, &runs), &image);
        for _ in 0..u.int_in_range(1..=8)? {
            let at = u.int_in_range(0..=size)?;
            let mut buf = vec![0u8; u.int_in_range(1..=3 * CS as usize)?];
            core.seek(SeekFrom::Start(at)).unwrap();
            let read = core.read(&mut buf).unwrap_or_else(|err| {
                panic!(
                    "runs {runs:?} size {size} read {} at {at}: {err}",
                    buf.len()
                )
            });
            // Must return everything asked for that exists, not just correct bytes: a read
            // that returns nothing (or too little) fails here.
            let want = (buf.len() as u64).min(size - at) as usize;
            assert_eq!(
                read,
                want,
                "runs {runs:?} size {size} read {} at {at}",
                buf.len()
            );
            assert_eq!(buf[..read], all[at as usize..at as usize + read], "at {at}");
        }
        Ok(())
    });
}

// A stream whose last cluster is the volume's last: the reader must never ask for more than is
// left, since the double fails a read crossing the end (and returns 0 for one wholly past it,
// as Windows does). The stream still reads completely.
#[test]
fn a_stream_in_the_last_clusters_of_the_volume_reads_through_a_strict_volume() {
    let image = volume_image();
    let last = IMAGE_CLUSTERS - 1;
    let layouts: [(&[Run], u64); 4] = [
        (&[(1, Some(last))], CS),
        (&[(1, Some(last))], CS - 777),
        (&[(2, Some(last - 1))], 2 * CS),
        (&[(1, None), (1, Some(last))], 2 * CS - 1),
    ];
    for (runs, size) in layouts {
        let all = expected(size, size, runs);
        let records = one_stream(size, runs);

        let mut core = strict_core(records.clone(), &image);
        assert_eq!(read_all(&mut core), all, "runs {runs:?} size {size}");
        assert_eq!(core.read(&mut [0u8; 64]).unwrap(), 0, "at the end");

        for start in (0..=size).step_by(509).chain([size - 1, size]) {
            for len in [1, 100, 4095, 4096, 4097, 8192, 8193, 20_000] {
                let mut core = strict_core(records.clone(), &image);
                core.seek(SeekFrom::Start(start)).unwrap();
                let mut buf = vec![0u8; len];
                let mut got = 0;
                while got < len {
                    match core.read(&mut buf[got..]).unwrap() {
                        0 => break,
                        n => got += n,
                    }
                }
                let want = (len as u64).min(size - start) as usize;
                let (start, end) = (start as usize, start as usize + want);
                assert_eq!(
                    got, want,
                    "runs {runs:?} size {size} start {start} len {len}"
                );
                assert_eq!(buf[..got], all[start..end], "start {start} len {len}");
            }
        }
    }
}

// A volume of 512-byte clusters whose size is not a whole number of 4 KiB blocks: the last
// cluster sits in the block the volume ends in. The last span is clamped to the end, not
// rounded past it, so no read crosses it.
#[test]
fn a_stream_in_the_last_cluster_of_a_volume_that_ends_inside_a_block_reads_through_a_strict_volume()
{
    let image = volume_image();
    for cut in [512usize, 2048, 3584] {
        let volume = &image[..image.len() - cut];
        let last = (volume.len() / 512 - 1) as u64;
        let (_, data, bitmap) = raw_parts(one_stream(512, &[(1, Some(last))]));
        let geometry = Volume::synthetic(
            PathBuf::from(VOLUME_PATH),
            512,
            volume.len() as u64,
            RECORD_SIZE as u64,
            0,
        );
        let mft = build_from_parts(geometry, data, bitmap);
        let strict = Strict {
            inner: Cursor::new(volume.to_vec()),
        };
        let mut core = open(&mft.record(24).unwrap(), None, strict).unwrap();
        let want = &volume[volume.len() - 512..];
        assert_eq!(read_all(&mut core), want, "cut {cut}");
        core.seek(SeekFrom::Start(511)).unwrap();
        let mut byte = [0u8; 1];
        core.read_exact(&mut byte).unwrap();
        assert_eq!(byte[0], want[511], "cut {cut}");
    }
}

#[test]
fn the_scratch_buffer_starts_on_a_page_boundary_and_is_clamped() {
    // Kept alive together, so the allocator cannot hand back the same block every time.
    let mut scratches: Vec<Scratch> = (0..16).map(|_| Scratch::new()).collect();
    for scratch in &mut scratches {
        for length in [1, 4096, Scratch::SIZE, usize::MAX] {
            let landing = scratch.take(length);
            assert_eq!(landing.len(), length.min(Scratch::SIZE));
            assert!(landing.as_ptr().addr().is_multiple_of(Scratch::ALIGNMENT));
        }
    }
}

// Reading a stream in random pieces gives what reading it in one go gives, matching the
// description. The stream has random runs, holes, an initialized size below the full size, and
// a size ending inside its last cluster.
#[test]
fn reading_in_random_chunks_equals_reading_in_one_go() {
    let image = volume_image();
    property(|u| {
        let mut runs: Vec<Run> = Vec::new();
        for _ in 0..u.int_in_range(0..=8)? {
            let clusters = u.int_in_range(1..=6u64)?;
            let lcn = if u.ratio(1, 4)? {
                None
            } else {
                Some(u.int_in_range(0..=IMAGE_CLUSTERS - clusters)?)
            };
            runs.push((clusters, lcn));
        }
        let total: u64 = runs.iter().map(|run| run.0).sum::<u64>() * CS;
        let size = u.int_in_range(0..=total)?;
        let initialized = if u.ratio(1, 2)? {
            size
        } else {
            u.int_in_range(0..=size)?
        };
        let mut records = one_stream(size, &runs);
        set_initialized_size(&mut records[0], initialized);
        let expected = expected(size, initialized, &runs);

        let mut core = open_records(records, &image).unwrap();
        assert_tiles(&core);
        let whole = read_all(&mut core);
        assert_eq!(
            whole, expected,
            "runs {runs:?} size {size} initialized {initialized}"
        );

        core.seek(SeekFrom::Start(0)).unwrap();
        let mut pieces = Vec::new();
        loop {
            let mut buf = vec![0u8; u.int_in_range(1..=3 * CS as usize)?];
            let read = core.read(&mut buf).unwrap();
            if read == 0 {
                break;
            }
            pieces.extend_from_slice(&buf[..read]);
            // Sometimes jump about and check the piece against the description.
            if u.ratio(1, 4)? {
                let at = u.int_in_range(0..=size + 10)?;
                core.seek(SeekFrom::Start(at)).unwrap();
                let mut buf = vec![0u8; u.int_in_range(1..=2 * CS as usize)?];
                let read = core.read(&mut buf).unwrap();
                let end = (at as usize + read).min(expected.len());
                let want = expected.get(at as usize..end).unwrap_or(&[]);
                assert_eq!(&buf[..read], want, "at {at}");
                let resume = pieces.len() as u64;
                core.seek(SeekFrom::Start(resume)).unwrap();
            }
        }
        assert_eq!(
            pieces, whole,
            "runs {runs:?} size {size} initialized {initialized}"
        );
        Ok(())
    });
}

// ---- Streams spread over records, with lost extents, live and deleted ----
//
// The tests above generate one extent at VCN 0. A real stream spreads its extents across
// extension records, some of which a delete can lose, and a deleted file is read from freed
// records. Here a stream is described by its extents (start, runs, which record holds it,
// whether that record is gone); the layout and bytes it reads are worked out cluster by cluster
// from that description alone, not by walking the runs as the crate does, and the crate must
// agree for both the live and the deleted file.

/// One extent of a generated stream.
#[derive(Debug, Clone)]
struct GenExtent {
    /// Where it starts, in clusters from the start of the stream (its lowest VCN).
    start: u64,
    runs: Vec<Run>,
    /// In the base record (else in an extension record of its own).
    in_base: bool,
    /// The record that held it is gone.
    lost: bool,
}

impl GenExtent {
    fn clusters(&self) -> u64 {
        self.runs.iter().map(|run| run.0).sum()
    }
}

#[derive(Debug, Clone)]
struct GenStream {
    size: u64,
    initialized: u64,
    /// The extents in the order they are laid in the records: the first is in the base record
    /// and starts at VCN 0, unless the stream has none there.
    extents: Vec<GenExtent>,
}

fn generate_stream(u: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<GenStream> {
    let size_clusters = u.int_in_range(0..=20u64)?;
    let size = match size_clusters {
        0 => u.int_in_range(0..=CS)?,
        clusters => clusters * CS - u.int_in_range(0..=CS - 1)?,
    };
    let initialized = match u.int_in_range(0..=3)? {
        0 => u.int_in_range(0..=size)?,
        _ => size,
    };
    let no_first = u.ratio(1, 12)?;
    let mut extents = Vec::new();
    for index in 0..u.int_in_range(1..=5)? {
        let mut runs = Vec::new();
        for _ in 0..u.int_in_range(1..=3)? {
            let clusters = u.int_in_range(1..=5u64)?;
            let lcn = if u.ratio(1, 4)? {
                None
            } else {
                Some(u.int_in_range(0..=IMAGE_CLUSTERS - clusters)?)
            };
            runs.push((clusters, lcn));
        }
        let start = match (index, no_first) {
            (0, false) => 0,
            (0, true) => u.int_in_range(1..=6u64)?,
            // Mostly where the stream goes on, sometimes a gap, sometimes an overlap.
            _ => match u.int_in_range(0..=5)? {
                0 => u.int_in_range(0..=size_clusters + 2)?,
                _ => {
                    let end = extents
                        .iter()
                        .map(|e: &GenExtent| e.start + e.clusters())
                        .max();
                    let gap = if u.ratio(1, 4)? {
                        u.int_in_range(0..=3u64)?
                    } else {
                        0
                    };
                    end.unwrap_or(0) + gap
                }
            },
        };
        extents.push(GenExtent {
            start,
            runs,
            in_base: index == 0 || u.ratio(1, 4)?,
            lost: index > 0 && u.ratio(1, 4)?,
        });
    }
    Ok(GenStream {
        size,
        initialized,
        extents,
    })
}

/// The records of a generated stream: the base (24, sequence 1) and one extension record (25,
/// 26, ...) for each extent that is not in the base. Lost extents are not written.
fn stream_records(gen: &GenStream) -> Vec<Vec<u8>> {
    let mut base = new_record(24, 1, 0);
    let mut end = ATTRIBUTES_OFFSET;
    let mut extension_records = Vec::new();
    let mut first_in_base = true;
    for extent in gen.extents.iter().filter(|extent| !extent.lost) {
        // The extent that starts at VCN 0 carries the size.
        let size = if extent.start == 0 { gen.size } else { 0 };
        if extent.in_base {
            let attribute_end = data_extent(&mut base, end, "", extent.start, size, &extent.runs);
            if first_in_base && extent.start == 0 {
                set_initialized_size(&mut base, gen.initialized);
            }
            first_in_base = false;
            end = attribute_end;
        } else {
            let number = 25 + extension_records.len() as u64;
            let mut record = new_record(number, 1, base_reference());
            let record_end = data_extent(
                &mut record,
                ATTRIBUTES_OFFSET,
                "",
                extent.start,
                size,
                &extent.runs,
            );
            extension_records.push(finish(record, record_end));
        }
    }
    // A base with only extension records has no attribute: give it a name so it is a file.
    if end == ATTRIBUTES_OFFSET {
        end = add_file_name(&mut base, end, "empty", 0);
    }
    let mut records = vec![finish(base, end)];
    records.extend(extension_records);
    records
}

/// The initialized size the stream reads with. The extent that carries it is the one at VCN 0:
/// the generator sets it in the base record's first attribute, and an extension record's
/// extent has the builder's own (the whole size).
fn initialized_size(gen: &GenStream) -> u64 {
    let first_in_base = gen.extents.iter().find(|e| !e.lost && e.in_base);
    let zero = gen.extents.iter().find(|e| !e.lost && e.start == 0);
    match (first_in_base, zero) {
        (Some(a), Some(b)) if std::ptr::eq(a, b) => gen.initialized.min(gen.size),
        _ => gen.size,
    }
}

/// What the description says about a stream.
#[derive(Debug)]
enum Modelled {
    /// The layout does not open: the extents overlap, or none starts at VCN 0.
    Error(&'static str),
    /// The extents in order, and where every byte is: `Some(Some(byte offset in the volume))`
    /// for stored bytes, `Some(None)` for a hole, `None` for bytes whose extent is lost.
    Layout {
        extents: Vec<StreamExtent>,
        clusters: Vec<Option<Option<u64>>>,
    },
}

fn model_stream(gen: &GenStream) -> Modelled {
    let present: Vec<&GenExtent> = gen.extents.iter().filter(|extent| !extent.lost).collect();
    if present.iter().all(|extent| extent.start != 0) {
        return Modelled::Error("VCN-0");
    }
    // Two extents overlap when their ranges of clusters do.
    for (a, first) in present.iter().enumerate() {
        for second in &present[a + 1..] {
            if first.start < second.start + second.clusters()
                && second.start < first.start + first.clusters()
            {
                return Modelled::Error("overlap");
            }
        }
    }
    // Where every cluster of the stream is: painted from the extents, so that what is in no
    // extent is left unpainted (lost).
    let total = gen.size.div_ceil(CS);
    let mut clusters: Vec<Option<Option<u64>>> = vec![None; total as usize];
    // The runs in the order of their clusters, as the extents of the stream.
    let mut ordered: Vec<&GenExtent> = present.clone();
    ordered.sort_by_key(|extent| extent.start);
    let mut extents: Vec<StreamExtent> = Vec::new();
    let mut covered = 0u64;
    let push = |extents: &mut Vec<StreamExtent>, from: u64, length: u64, location| {
        if from < gen.size {
            let length = length.min(gen.size - from);
            if let Some(last) = extents.last_mut() {
                if last.stream_offset + last.length == from
                    && last.location == ExtentLocation::Sparse
                    && location == ExtentLocation::Sparse
                {
                    last.length += length;
                    return;
                }
            }
            extents.push(StreamExtent {
                stream_offset: from,
                length,
                location,
            });
        }
    };
    for extent in ordered {
        if extent.start > covered {
            push(
                &mut extents,
                covered * CS,
                (extent.start - covered) * CS,
                ExtentLocation::Missing,
            );
            covered = extent.start;
        }
        for &(length, lcn) in &extent.runs {
            let location = lcn.map_or(ExtentLocation::Sparse, |lcn| ExtentLocation::Volume {
                offset: lcn * CS,
            });
            push(&mut extents, covered * CS, length * CS, location);
            for cluster in covered..covered + length {
                if let Some(slot) = clusters.get_mut(cluster as usize) {
                    *slot = Some(lcn.map(|lcn| (lcn + (cluster - covered)) * CS));
                }
            }
            covered += length;
        }
    }
    if covered * CS < gen.size {
        push(
            &mut extents,
            covered * CS,
            gen.size - covered * CS,
            ExtentLocation::Missing,
        );
    }
    Modelled::Layout { extents, clusters }
}

/// The bytes a whole-stream read returns, and the offset it stops at if a lost extent blocks it:
/// a byte at or beyond the initialized size is zero, a hole is zero, a stored byte is the
/// volume's, and a byte of a lost extent below the initialized size cannot be read.
fn model_read(
    gen: &GenStream,
    clusters: &[Option<Option<u64>>],
    image: &[u8],
) -> (Vec<u8>, Option<u64>) {
    let initialized = initialized_size(gen);
    let mut bytes = Vec::new();
    for position in 0..gen.size {
        if position >= initialized {
            bytes.push(0);
            continue;
        }
        match clusters[(position / CS) as usize] {
            Some(Some(offset)) => bytes.push(image[(offset + position % CS) as usize]),
            Some(None) => bytes.push(0),
            None => return (bytes, Some(position)),
        }
    }
    (bytes, None)
}

/// An `Mft` over a volume of the image's size (so that a run past its end is an error), with
/// the records of `records` freed when `freed`.
fn mft_over_image(records: Vec<Vec<u8>>, freed: bool) -> Mft {
    let numbers: Vec<u64> = (24..24 + records.len() as u64).collect();
    let mut records = records;
    if freed {
        for record in &mut records {
            delete_record(record);
        }
    }
    let (_, data, mut bitmap) = raw_parts(records);
    if freed {
        for number in numbers {
            bitmap[number as usize / 8] &= !(1 << (number % 8));
        }
    }
    let volume = Volume::synthetic(
        PathBuf::from(VOLUME_PATH),
        CS,
        IMAGE_CLUSTERS * CS,
        RECORD_SIZE as u64,
        0,
    );
    build_from_parts(volume, data, bitmap)
}

#[derive(Default)]
struct StreamStats {
    cases: usize,
    with_extension_records: usize,
    with_lost_extent: usize,
    read_stops_at_a_lost_extent: usize,
    lost_beyond_initialized: usize,
    uninitialized_tail: usize,
    overlaps: usize,
    without_vcn_0: usize,
    deleted: usize,
}

// Live and deleted, any number of extents in any number of records, any of them lost: the
// extents match the description and tile the stream, and the stream reads what the description
// says, up to the lost extent.
#[test]
fn a_stream_spread_over_records_with_lost_extents_reads_what_the_description_says() {
    let image = volume_image();
    let stats = std::cell::RefCell::new(StreamStats::default());
    property(|u| {
        let gen = generate_stream(u)?;
        let freed = u.ratio(1, 2)?;
        let records = stream_records(&gen);
        let mft = mft_over_image(records.clone(), freed);
        let modelled = model_stream(&gen);
        let mut stats = stats.borrow_mut();
        stats.cases += 1;
        stats.deleted += usize::from(freed);
        stats.with_extension_records += usize::from(records.len() > 1);
        stats.with_lost_extent += usize::from(gen.extents.iter().any(|e| e.lost));

        match (open_24(&mft, None, &image), modelled) {
            (Err(NtfsReaderError::InvalidDataRun { details }), Modelled::Error(expected)) => {
                assert!(details.contains(expected), "{details:?} for {gen:?}");
                stats.overlaps += usize::from(expected == "overlap");
                stats.without_vcn_0 += usize::from(expected == "VCN-0");
            }
            (Err(NtfsReaderError::StreamNotFound { .. }), _) => {
                // No attribute at all: only when nothing was written, which the base's
                // filler name prevents unless every extent is lost.
                assert!(
                    gen.extents.iter().all(|extent| extent.lost),
                    "no stream for {gen:?}"
                );
            }
            (Err(other), model) => {
                panic!("{other:?} where the description says {model:?}: {gen:?}")
            }
            (Ok(_), Modelled::Error(expected)) => {
                panic!("opened a stream the description says is an error ({expected}): {gen:?}")
            }
            (Ok(mut core), Modelled::Layout { extents, clusters }) => {
                assert_tiles(&core);
                assert_eq!(core.size, gen.size, "{gen:?}");
                assert_eq!(core.initialized_size, initialized_size(&gen), "{gen:?}");
                assert_eq!(
                    core.extents, extents,
                    "extents of {gen:?} (deleted {freed})"
                );
                let (expected, stops_at) = model_read(&gen, &clusters, &image);
                stats.uninitialized_tail += usize::from(initialized_size(&gen) < gen.size);
                stats.lost_beyond_initialized += usize::from(
                    stops_at.is_none()
                        && extents
                            .iter()
                            .any(|e| e.location == ExtentLocation::Missing),
                );
                let mut got = Vec::new();
                match (core.read_to_end(&mut got), stops_at) {
                    (Ok(_), None) => {}
                    (Err(error), Some(offset)) => {
                        stats.read_stops_at_a_lost_extent += 1;
                        assert!(
                            matches!(
                                ntfs_error(&error),
                                NtfsReaderError::StreamExtentMissing { offset: at } if *at == offset
                            ),
                            "{error:?}, expected the loss at {offset}: {gen:?}"
                        );
                    }
                    (result, stops_at) => {
                        panic!("read {result:?}, the description says it stops at {stops_at:?}: {gen:?}")
                    }
                }
                assert!(
                    got == expected,
                    "the bytes read differ for {gen:?} (deleted {freed})"
                );
            }
        }
        Ok(())
    });
    let stats = stats.into_inner();
    assert!(stats.cases > 100, "{} cases", stats.cases);
    assert!(
        stats.with_extension_records > 50,
        "{}",
        stats.with_extension_records
    );
    assert!(stats.with_lost_extent > 20, "{}", stats.with_lost_extent);
    assert!(
        stats.read_stops_at_a_lost_extent > 10,
        "{}",
        stats.read_stops_at_a_lost_extent
    );
    assert!(
        stats.lost_beyond_initialized > 3,
        "{}",
        stats.lost_beyond_initialized
    );
    assert!(
        stats.uninitialized_tail > 20,
        "{}",
        stats.uninitialized_tail
    );
    assert!(stats.overlaps > 5, "{}", stats.overlaps);
    assert!(stats.without_vcn_0 > 5, "{}", stats.without_vcn_0);
    assert!(stats.deleted > 50, "{}", stats.deleted);
}

// A record with random bytes changed: opening the stream must not panic, and the result is
// either an error or a stream whose extents tile and whose parts add up.
#[test]
fn a_corrupt_stream_is_an_error_or_a_stream_whose_parts_add_up() {
    let image = volume_image();
    let outcomes = std::cell::RefCell::new((0usize, 0usize, 0usize));
    property(|u| {
        let gen = generate_stream(u)?;
        let mut records = stream_records(&gen);
        for _ in 0..u.int_in_range(1..=6)? {
            let record = u.int_in_range(0..=records.len() - 1)?;
            // Mostly the attribute area, sometimes anywhere in the record.
            let last = if u.ratio(3, 4)? { 400 } else { RECORD_SIZE - 1 };
            let at = u.int_in_range(ATTRIBUTES_OFFSET..=last)?;
            let flip = u.int_in_range(1..=255u8)?;
            records[record][at] ^= flip;
        }
        let freed = u.ratio(1, 2)?;
        let mft = mft_over_image(records, freed);
        let Some(file) = mft.record(24) else {
            return Ok(());
        };
        let mut outcomes = outcomes.borrow_mut();
        match open(&file, None, Cursor::new(image.as_slice())) {
            Err(_) => outcomes.0 += 1,
            Ok(mut core) => {
                outcomes.1 += 1;
                assert_tiles(&core);
                assert!(core.initialized_size <= core.size);
                // No stored extent reaches outside the volume: such a run is an error, not a
                // read there.
                for extent in &core.extents {
                    if let ExtentLocation::Volume { offset } = extent.location {
                        assert!(
                            offset + extent.length <= IMAGE_CLUSTERS * CS,
                            "{extent:?} is outside the volume"
                        );
                    }
                }
                // Reading never panics; it may fail.
                // (A size the corruption made enormous is only read from the start.)
                let _ = core.by_ref().take(CS * 8).read_to_end(&mut Vec::new());

                let bitmap = ClusterBitmap::read(
                    mft.volume(),
                    IMAGE_CLUSTERS.div_ceil(8),
                    Cursor::new(vec![0xF0u8; IMAGE_CLUSTERS as usize / 8]),
                )
                .expect("a bitmap");
                let allocation = allocation_of(
                    &core.extents,
                    core.size,
                    core.initialized_size,
                    core.data_lost,
                    &bitmap,
                );
                let parts = allocation.in_free_clusters()
                    + allocation.in_allocated_clusters()
                    + allocation.resident()
                    + allocation.sparse()
                    + allocation.missing()
                    + allocation.beyond_initialized()
                    + allocation.outside_bitmap();
                assert_eq!(parts, core.size, "{:?}", core.extents);

                // The public way in gives the same answer and the same accounting.
                let stream = file.open_stream(None).expect("it opened above");
                let allocation = stream.allocation(&bitmap).expect("this volume's bitmap");
                assert_eq!(allocation.size(), core.size);
                assert_eq!(stream.extents(), core.extents.as_slice());
            }
        }
        outcomes.2 += 1;
        Ok(())
    });
    let (errors, opened, cases) = outcomes.into_inner();
    assert!(cases > 100, "{cases} cases");
    assert!(errors > 10, "only {errors} corrupt streams were refused");
    assert!(opened > 50, "only {opened} corrupt streams still opened");
}
