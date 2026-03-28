use criterion::{criterion_group, criterion_main, Criterion};
use ntfs_reader::{
    file_info::{FileInfo, HashMapCache, VecCache},
    mft::Mft,
    test_utils::test_volume_letter,
    volume::Volume,
};
use std::hint::black_box;

// Configuration constants
const PARTIAL_ITERATION_LIMIT: usize = 1000;
const CACHE_DROP_ITERATION_LIMIT: usize = 10000;

fn open_mft() -> Mft {
    let vol =
        Volume::new(format!("\\\\.\\{}:", test_volume_letter())).expect("Failed to open volume");
    Mft::new(vol).expect("Failed to create MFT")
}

fn bench_file_iteration_no_cache(c: &mut Criterion) {
    let mft = open_mft();
    c.bench_function("file_iteration_no_cache", |b| {
        b.iter(|| {
            let mut counter = 0;
            for file in mft.files() {
                let _info = FileInfo::new(&mft, &file);
                counter += 1;
                if counter >= PARTIAL_ITERATION_LIMIT {}
            }
            black_box(counter)
        });
    });
}

fn bench_file_iteration_hashmap_cache(c: &mut Criterion) {
    let mft = open_mft();
    c.bench_function("file_iteration_hashmap_cache", |b| {
        b.iter(|| {
            let mut cache = HashMapCache::default();
            let mut counter = 0;
            for file in mft.files() {
                let _info = FileInfo::with_cache(&mft, &file, &mut cache);
                counter += 1;
                if counter >= PARTIAL_ITERATION_LIMIT {}
            }
            black_box(counter)
        });
    });
}

fn bench_file_iteration_vec_cache(c: &mut Criterion) {
    let mft = open_mft();
    c.bench_function("file_iteration_vec_cache", |b| {
        b.iter(|| {
            let mut cache = VecCache::default();
            cache.0.resize(mft.max_record as usize, None);
            let mut counter = 0;
            for file in mft.files() {
                let _info = FileInfo::with_cache(&mft, &file, &mut cache);
                counter += 1;
                if counter >= PARTIAL_ITERATION_LIMIT {}
            }
            black_box(counter)
        });
    });
}

fn bench_full_iteration_no_cache(c: &mut Criterion) {
    let mft = open_mft();
    c.bench_function("full_iteration_no_cache", |b| {
        b.iter(|| {
            let mut files = Vec::new();
            for file in mft.files() {
                files.push(FileInfo::new(&mft, &file));
            }
            black_box(files.len())
        });
    });
}

fn bench_full_iteration_hashmap_cache(c: &mut Criterion) {
    let mft = open_mft();
    c.bench_function("full_iteration_hashmap_cache", |b| {
        b.iter(|| {
            let mut cache = HashMapCache::default();
            let mut files = Vec::new();
            for file in mft.files() {
                files.push(FileInfo::with_cache(&mft, &file, &mut cache));
            }
            black_box(files.len())
        });
    });
}

fn bench_full_iteration_vec_cache(c: &mut Criterion) {
    let mft = open_mft();
    c.bench_function("full_iteration_vec_cache", |b| {
        b.iter(|| {
            let mut cache = VecCache::default();
            cache.0.resize(mft.max_record as usize, None);
            let mut files = Vec::new();
            for file in mft.files() {
                files.push(FileInfo::with_cache(&mft, &file, &mut cache));
            }
            black_box(files.len())
        });
    });
}

fn bench_cache_drop_hashmap(c: &mut Criterion) {
    let mft = open_mft();
    c.bench_function("cache_drop_hashmap", |b| {
        b.iter(|| {
            let mut cache = HashMapCache::default();
            let mut counter = 0;
            for file in mft.files() {
                let _info = FileInfo::with_cache(&mft, &file, &mut cache);
                counter += 1;
                if counter >= CACHE_DROP_ITERATION_LIMIT {}
            }
            drop(black_box(cache));
        });
    });
}

fn bench_cache_drop_vec(c: &mut Criterion) {
    let mft = open_mft();
    c.bench_function("cache_drop_vec", |b| {
        b.iter(|| {
            let mut cache = VecCache::default();
            cache.0.resize(mft.max_record as usize, None);
            let mut counter = 0;
            for file in mft.files() {
                let _info = FileInfo::with_cache(&mft, &file, &mut cache);
                counter += 1;
                if counter >= CACHE_DROP_ITERATION_LIMIT {}
            }
            drop(black_box(cache));
        });
    });
}

criterion_group!(
    name = benches;
    config = Criterion::default().sample_size(10);
    targets =
    bench_file_iteration_no_cache,
    bench_file_iteration_hashmap_cache,
    bench_file_iteration_vec_cache,
    bench_full_iteration_no_cache,
    bench_full_iteration_hashmap_cache,
    bench_full_iteration_vec_cache,
    bench_cache_drop_hashmap,
    bench_cache_drop_vec,
);
criterion_main!(benches);
