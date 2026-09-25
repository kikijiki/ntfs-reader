// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! Timing of the deleted-file paths on synthetic input (no volume): listing and summarising
//! deleted files with shared caches, opening and classifying a stream's allocation, reading one
//! with inner reads and bytes counted, and reading a cluster bitmap at NTFS's largest size on
//! Windows (512 MiB).

use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::{cell::Cell, hint::black_box, rc::Rc};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

mod support;
use support::{deleted_dataset, fragmented_file, sized_volume};

use ntfs_reader::internals::{self, test_records::*};
use ntfs_reader::{DefaultPathCache, DeletedPathCache, FileInfo, NtfsAttributeType};

/// `deleted_files()` plus one `FileInfo` per file, sharing a live and a deleted cache across the
/// scan. `records` records, one in ten deleted, under `depth` directories that are live or
/// deleted too.
fn bench_deleted_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("deleted_scan");
    group.sample_size(10);
    for records in [20_000usize, 200_000] {
        for depth in [8usize, 32] {
            for freed_dirs in [false, true] {
                let mft = deleted_dataset(records, depth, 10, freed_dirs);
                let what = if freed_dirs {
                    "freed_dirs"
                } else {
                    "live_dirs"
                };
                group.bench_function(
                    BenchmarkId::new(format!("{what}_depth{depth}"), records),
                    |b| {
                        b.iter(|| {
                            let (mut live, mut deleted) =
                                (DefaultPathCache::new(), DeletedPathCache::new());
                            let mut count = 0usize;
                            for file in mft.deleted_files() {
                                black_box(FileInfo::with_caches(&file, &mut live, &mut deleted));
                                count += 1;
                            }
                            // The deleted directories are deleted files too.
                            assert_eq!(
                                count,
                                records.div_ceil(10) + if freed_dirs { depth } else { 0 }
                            );
                        })
                    },
                );
            }
        }
    }
    group.finish();
}

/// `open_stream` (the layout of the extents) and `StreamReader::allocation` (one pass over them
/// against a bitmap) for a file in 1, 4096 and 100,000 extents.
fn bench_open_and_allocation(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_layout");
    group.sample_size(20);
    for extents in [1usize, 4096, 100_000] {
        let mft = fragmented_file(extents);
        let file = mft.record(FIRST_RECORD).expect("the file");
        let volume = mft.volume();
        let clusters = volume.volume_size() / volume.cluster_size();
        let bitmap = internals::cluster_bitmap(
            volume,
            clusters.div_ceil(8),
            io::repeat(0b0101_0101).take(clusters.div_ceil(8)),
        )
        .expect("a bitmap");
        group.bench_function(BenchmarkId::new("open_stream", extents), |b| {
            b.iter(|| black_box(file.open_stream(None).expect("open")))
        });
        let stream = file.open_stream(None).expect("open");
        assert_eq!(stream.extents().len(), extents);
        group.bench_function(BenchmarkId::new("allocation", extents), |b| {
            b.iter(|| black_box(stream.allocation(&bitmap).expect("allocation")))
        });
    }
    group.finish();
}

/// A volume image that counts what it is asked for.
struct Counting {
    inner: Cursor<Vec<u8>>,
    reads: Rc<Cell<u64>>,
    bytes: Rc<Cell<u64>>,
}

impl Read for Counting {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buf)?;
        self.reads.set(self.reads.get() + 1);
        self.bytes.set(self.bytes.get() + count as u64);
        Ok(count)
    }
}

impl Seek for Counting {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.inner.seek(position)
    }
}

/// Reading the default stream of a file in one 8 MiB contiguous run, and of one in 2,048
/// fragments of 4 KiB, over an in-memory image, in pieces of 8 KiB, 1 MiB, and 4 KiB at random
/// offsets. Inner reads and bytes are counted and printed once per case.
fn bench_stream_reader(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_reader");
    group.sample_size(20);
    let clusters = 4_200u64;
    let image = Rc::new(vec![0xA5u8; (clusters * CLUSTER_SIZE as u64) as usize]);
    for (shape, extents) in [("contiguous", 1usize), ("fragmented", 2_048)] {
        let (mft, size) = if extents == 1 {
            (
                contiguous_file(2_048, clusters),
                2_048 * CLUSTER_SIZE as u64,
            )
        } else {
            (
                fragmented_file(extents),
                extents as u64 * CLUSTER_SIZE as u64,
            )
        };
        let file = mft.record(FIRST_RECORD).expect("the file");
        for (pieces, piece) in [("8k", 8 << 10), ("1m", 1 << 20), ("random_4k", 4 << 10)] {
            let counts = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
            let mut reader = internals::open_stream_over(
                &file,
                None,
                Counting {
                    inner: Cursor::new(image.to_vec()),
                    reads: counts.0.clone(),
                    bytes: counts.1.clone(),
                },
            )
            .expect("open");
            let mut buffer = vec![0u8; piece];
            let mut position = 0x2545_F491_4F6C_DD1Du64;
            let mut read_once = |reader: &mut _| -> u64 {
                let reader: &mut dyn ReadSeek = reader;
                if pieces == "random_4k" {
                    position = position
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1);
                    let at = (position >> 33) % (size - piece as u64);
                    reader.seek(SeekFrom::Start(at)).expect("seek");
                    reader.read_exact(&mut buffer).expect("read");
                    piece as u64
                } else {
                    reader.seek(SeekFrom::Start(0)).expect("seek");
                    let mut total = 0;
                    loop {
                        let count = reader.read(&mut buffer).expect("read");
                        if count == 0 {
                            break;
                        }
                        total += count as u64;
                    }
                    total
                }
            };
            // One cold pass (1,000 reads for the random case), counters on: what the volume was
            // asked for against what the caller got.
            let (passes, mut requested) = (if pieces == "random_4k" { 1_000 } else { 1 }, 0);
            for _ in 0..passes {
                requested += read_once(&mut reader);
            }
            eprintln!(
                "stream_reader/{shape}/{pieces}: {} inner reads, {} bytes from the volume, for {requested} bytes read",
                counts.0.get(),
                counts.1.get()
            );
            group.throughput(Throughput::Bytes(if pieces == "random_4k" {
                piece as u64
            } else {
                size
            }));
            group.bench_function(BenchmarkId::new(shape, pieces), |b| {
                b.iter(|| black_box(read_once(&mut reader)))
            });
        }
    }
    group.finish();
}

trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

/// A file whose default stream is one run of `clusters` clusters at cluster 2, on a volume of
/// `volume_clusters` clusters.
fn contiguous_file(clusters: u64, volume_clusters: u64) -> ntfs_reader::Mft {
    let mut record = new_record(FIRST_RECORD, 1, 0);
    let mut offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, "contiguous.bin", 0);
    offset = add_nonresident_data_runs(
        &mut record,
        offset,
        NtfsAttributeType::Data,
        "",
        0,
        clusters - 1,
        clusters * CLUSTER_SIZE as u64,
        &encode_runs(&[(clusters, Some(2))]),
    );
    finish_record(&mut record, offset);
    let (_, data, bitmap) = raw_parts(vec![record]);
    build_from_parts(sized_volume(volume_clusters), data, bitmap)
}

/// Reading the `$Bitmap` of a volume of 2^32 clusters (512 MiB of bits, the most NTFS on Windows
/// has) from a stream of that size.
fn bench_bitmap_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("cluster_bitmap");
    group.sample_size(10);
    let clusters = 1u64 << 32;
    let volume = sized_volume(clusters);
    let bytes = clusters / 8;
    group.throughput(Throughput::Bytes(bytes));
    group.bench_function("read_512_mib", |b| {
        b.iter(|| {
            black_box(
                internals::cluster_bitmap(&volume, bytes, io::repeat(0xAA).take(bytes))
                    .expect("a bitmap"),
            )
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_deleted_scan,
    bench_open_and_allocation,
    bench_stream_reader,
    bench_bitmap_read
);
criterion_main!(benches);
