//! Timing of the per-file accessors on a synthetic MFT: `record_attributes`, `attributes` (the
//! whole logical file, extension records included), `names`/`best_name`, `data_streams`,
//! `resolve_path` with and without a cache, `FileInfo`, and data run decoding. Synthetic input
//! runs without a volume; see `mft_benchmark` for end-to-end, volume-backed numbers.

use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;

mod support;
use support::{build_dataset, fragmented_data_run_record};

use ntfs_reader::{internals, DefaultPathCache, FileInfo, Mft};

const FILE_COUNT: usize = 4_000;
/// One file in four gets a named alternate data stream.
const STREAM_EVERY: usize = 4;
/// One file in five has its `$DATA` (and ADS) in an extension record.
const SPLIT_EVERY: usize = 5;
/// A single MFT record holds at most about 300 runs of this shape (see
/// `fragmented_data_run_record`'s doc comment); stay well clear of that.
const RUN_COUNTS: [usize; 3] = [4, 64, 256];

fn dataset() -> Mft {
    build_dataset(FILE_COUNT, STREAM_EVERY, SPLIT_EVERY)
}

fn bench_record_attributes(c: &mut Criterion) {
    let mft = dataset();
    c.bench_function("record_attributes/full_scan", |b| {
        b.iter(|| {
            for file in mft.files() {
                for attribute in file.record_attributes() {
                    black_box(attribute.attribute_type());
                }
            }
        })
    });
}

/// Unlike `record_attributes`, this also walks extension records (one file in five here), paying
/// for `NtfsFile::records`'s lookup in the sorted extension-record index.
fn bench_attributes(c: &mut Criterion) {
    let mft = dataset();
    c.bench_function("attributes/full_scan", |b| {
        b.iter(|| {
            for file in mft.files() {
                for attribute in file.attributes() {
                    black_box(attribute.attribute_type());
                }
            }
        })
    });
}

fn bench_names(c: &mut Criterion) {
    let mft = dataset();
    let mut group = c.benchmark_group("names");
    group.bench_function("names/full_scan", |b| {
        b.iter(|| {
            for file in mft.files() {
                for name in file.names() {
                    black_box(&name);
                }
            }
        })
    });
    group.bench_function("best_name/full_scan", |b| {
        b.iter(|| {
            for file in mft.files() {
                black_box(file.best_name());
            }
        })
    });
    group.finish();
}

fn bench_data_streams(c: &mut Criterion) {
    let mft = dataset();
    c.bench_function("data_streams/full_scan", |b| {
        b.iter(|| {
            for file in mft.files() {
                for stream in file.data_streams() {
                    black_box(stream);
                }
            }
        })
    });
}

fn bench_file_info(c: &mut Criterion) {
    let mft = dataset();
    c.bench_function("file_info/full_scan", |b| {
        b.iter(|| {
            let mut cache = DefaultPathCache::new();
            for file in mft.files() {
                black_box(FileInfo::with_cache(&file, &mut cache));
            }
        })
    });
}

/// `DIR_DEPTH` levels means every uncached lookup walks that many parent hops; a full scan with
/// a fresh cache resolves each directory once and reuses it for every sibling, as
/// `FileInfo::with_cache` does in a real scan.
fn bench_resolve_path(c: &mut Criterion) {
    let mft = dataset();
    let mut group = c.benchmark_group("resolve_path");
    group.bench_function("no_cache/full_scan", |b| {
        b.iter(|| {
            for file in mft.files() {
                if let Some(name) = file.best_name() {
                    black_box(mft.resolve_path(&name, &mut ()));
                }
            }
        })
    });
    group.bench_function("default_path_cache/full_scan", |b| {
        b.iter(|| {
            let mut cache = DefaultPathCache::new();
            for file in mft.files() {
                if let Some(name) = file.best_name() {
                    black_box(mft.resolve_path(&name, &mut cache));
                }
            }
        })
    });
    group.finish();
}

/// `NtfsAttribute::nonresident_data_runs` alone, over an increasingly fragmented attribute (no
/// `Mft` needed: it only reads the attribute's own bytes and the volume's cluster size).
fn bench_data_run_decoding(c: &mut Criterion) {
    let mut group = c.benchmark_group("data_run_decoding");
    for &run_count in &RUN_COUNTS {
        let (record, volume) = fragmented_data_run_record(run_count);
        group.bench_function(format!("runs_{run_count}"), |b| {
            b.iter(|| {
                let attribute = internals::record_attributes(&record)
                    .expect("valid record")
                    .next()
                    .expect("one attribute");
                black_box(internals::nonresident_data_runs(&attribute, &volume).expect("runs"));
            })
        });
    }
    group.finish();
}

criterion_group!(
    name = benches;
    config = Criterion::default().sample_size(30);
    targets = bench_record_attributes,
        bench_attributes,
        bench_names,
        bench_data_streams,
        bench_file_info,
        bench_resolve_path,
        bench_data_run_decoding
);
criterion_main!(benches);
