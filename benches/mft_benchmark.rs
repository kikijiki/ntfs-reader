//! Timing of `Mft::new` and of `FileInfo` path resolution with and without a `DefaultPathCache`.
//! Needs an elevated shell; the volume comes from `test_volume_letter()` (the stress volume `S:`
//! gives numbers at a million files, see CONTRIBUTING.md). See `cache_memory` for the memory side.
//!
//! Windows-only regardless of the `internals` feature (it only makes the crate's parsing *types*
//! buildable on Linux, not a real `\\.\C:` device): every item below is `#[cfg(windows)]`, with a
//! `#[cfg(not(windows))]` stub `main` instead of failing at run time on a volume that does not
//! exist here. See `journal_synthetic.rs` for the same pattern.

#[cfg(windows)]
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
#[cfg(windows)]
use ntfs_reader::{DefaultPathCache, FileInfo, Mft, PathCache, Volume};
#[cfg(windows)]
use std::hint::black_box;

#[cfg(windows)]
#[path = "../tests/common/mod.rs"]
mod common;
#[cfg(windows)]
use common::test_volume_letter;

#[cfg(windows)]
const LOOKUP_COUNTS: [usize; 3] = [10, 100, 1000];

#[cfg(windows)]
fn open_mft() -> Mft {
    let volume =
        Volume::new(format!("\\\\.\\{}:", test_volume_letter())).expect("Failed to open volume");
    Mft::new(volume).expect("Failed to create MFT")
}

/// Record numbers of `count` files spread evenly over the volume.
#[cfg(windows)]
fn sample(mft: &Mft, count: usize) -> Vec<u64> {
    let files: Vec<u64> = mft.files().map(|file| file.number()).collect();
    let step = (files.len() / count).max(1);
    files.into_iter().step_by(step).take(count).collect()
}

#[cfg(windows)]
fn full_scan(mft: &Mft, cache: &mut impl PathCache) {
    for file in mft.files() {
        black_box(FileInfo::with_cache(&file, cache));
    }
}

#[cfg(windows)]
fn lookup(mft: &Mft, numbers: &[u64], cache: &mut impl PathCache) {
    for file in numbers.iter().filter_map(|&number| mft.record(number)) {
        black_box(FileInfo::with_cache(&file, cache));
    }
}

/// Loading the whole `$MFT`: the volume read, the fixups and the extension-record index.
#[cfg(windows)]
fn bench_mft_new(c: &mut Criterion) {
    c.bench_function("mft_new", |b| b.iter(|| black_box(open_mft())));
}

#[cfg(windows)]
fn bench_full_scan(c: &mut Criterion) {
    let mft = open_mft();
    let mut group = c.benchmark_group("full_scan");
    group.bench_function("none", |b| b.iter(|| full_scan(&mft, &mut ())));
    group.bench_function("default_path_cache", |b| {
        b.iter(|| full_scan(&mft, &mut DefaultPathCache::new()))
    });
    group.finish();
}

/// A fresh cache per iteration: the cost of a cache when few files are resolved, allocation and
/// drop included.
#[cfg(windows)]
fn bench_lookup(c: &mut Criterion) {
    let mft = open_mft();
    let mut group = c.benchmark_group("lookup");
    for count in LOOKUP_COUNTS {
        let numbers = sample(&mft, count);
        group.bench_with_input(BenchmarkId::new("none", count), &numbers, |b, numbers| {
            b.iter(|| lookup(&mft, numbers, &mut ()))
        });
        group.bench_with_input(
            BenchmarkId::new("default_path_cache", count),
            &numbers,
            |b, numbers| b.iter(|| lookup(&mft, numbers, &mut DefaultPathCache::new())),
        );
    }
    group.finish();
}

/// Dropping a cache filled by a full scan, measured on its own.
#[cfg(windows)]
fn bench_cache_drop(c: &mut Criterion) {
    let mft = open_mft();
    c.bench_function("cache_drop", |b| {
        b.iter_batched(
            || {
                let mut cache = DefaultPathCache::new();
                full_scan(&mft, &mut cache);
                cache
            },
            drop,
            BatchSize::LargeInput,
        )
    });
}

#[cfg(windows)]
criterion_group!(
    name = benches;
    config = Criterion::default().sample_size(10);
    targets = bench_mft_new, bench_full_scan, bench_lookup, bench_cache_drop
);
#[cfg(windows)]
criterion_main!(benches);

#[cfg(not(windows))]
fn main() {
    eprintln!("mft_benchmark: windows-only (needs a real volume); skipped on this platform");
}
