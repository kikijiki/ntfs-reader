//! Timing of `Mft::from_parts` (per-record fixups, then indexing extension
//! records): the step `Mft::new` runs after reading a volume's `$MFT` into
//! memory. Card 008 made this step about 25x faster and no benchmark caught
//! it either way; this is the one that would have. Synthetic input, so it
//! runs without a volume. See `mft_benchmark` for end-to-end, volume-backed
//! numbers (I/O included).
//!
//! `flat`: no file has an extension record. `with_extensions`: every file's
//! `$DATA` lives in a second, extension record, so `index_extension_records`
//! also has real work to do (see docs/architecture.md's "central rule": a
//! file's attributes can spill into extension records).

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use std::hint::black_box;

#[path = "support/mod.rs"]
mod support;
use support::dataset_raw_parts;

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
    }
    group.finish();
}

criterion_group!(benches, bench_from_parts);
criterion_main!(benches);
