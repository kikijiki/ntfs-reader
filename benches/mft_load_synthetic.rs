//! Timing of `Mft::from_parts` (per-record fixups, then indexing extension records), the step
//! `Mft::new` runs after reading a volume's `$MFT` into memory. A change that once made this step
//! about 25x faster went unmeasured; this bench would have caught it. Synthetic input runs
//! without a volume; see `mft_benchmark` for end-to-end, volume-backed numbers (I/O included).
//!
//! `flat`: no extension records. `with_extensions`: every file's `$DATA` lives in an extension
//! record, giving the loader's extension-record index real work. `with_freed`: like
//! `with_extensions`, with every fourth file deleted (base and extension record freed), so the
//! loader also sees freed records.

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use std::hint::black_box;

mod support;
use support::{dataset_raw_parts, dataset_raw_parts_freed};

use ntfs_reader::internals::test_records::build_from_parts;

const RECORD_COUNTS: [usize; 4] = [100, 1_000, 5_000, 20_000];

fn bench_from_parts(c: &mut Criterion) {
    let mut group = c.benchmark_group("mft_from_parts");
    group.sample_size(20);

    for &count in &RECORD_COUNTS {
        let flat = dataset_raw_parts(count, 0, 0);
        group.bench_with_input(
            BenchmarkId::new("flat", count),
            &flat,
            |b, (volume, data, bitmap)| {
                b.iter_batched(
                    || (volume.clone(), data.clone(), bitmap.clone()),
                    |(volume, data, bitmap)| black_box(build_from_parts(volume, data, bitmap)),
                    BatchSize::LargeInput,
                )
            },
        );

        let split = dataset_raw_parts(count, 0, 1);
        group.bench_with_input(
            BenchmarkId::new("with_extensions", count),
            &split,
            |b, (volume, data, bitmap)| {
                b.iter_batched(
                    || (volume.clone(), data.clone(), bitmap.clone()),
                    |(volume, data, bitmap)| black_box(build_from_parts(volume, data, bitmap)),
                    BatchSize::LargeInput,
                )
            },
        );

        let freed = dataset_raw_parts_freed(count, 4);
        group.bench_with_input(
            BenchmarkId::new("with_freed", count),
            &freed,
            |b, (volume, data, bitmap)| {
                b.iter_batched(
                    || (volume.clone(), data.clone(), bitmap.clone()),
                    |(volume, data, bitmap)| black_box(build_from_parts(volume, data, bitmap)),
                    BatchSize::LargeInput,
                )
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_from_parts);
criterion_main!(benches);
