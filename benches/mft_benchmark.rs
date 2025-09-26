#![feature(test)]

extern crate test;

// Configuration constants
const PARTIAL_ITERATION_LIMIT: usize = 1000;
const CACHE_DROP_ITERATION_LIMIT: usize = 10000;

#[cfg(test)]
mod tests {
    use super::*;
    use ntfs_reader::{
        file_info::{FileInfo, HashMapCache, VecCache},
        mft::Mft,
        test_utils::test_volume_letter,
        volume::Volume,
    };
    use test::{black_box, Bencher};

    #[bench]
    fn bench_file_iteration_no_cache(b: &mut Bencher) {
        println!(
            "Starting bench_file_iteration_no_cache (limit: {})",
            PARTIAL_ITERATION_LIMIT
        );

        let vol = Volume::new(format!("\\\\.\\{}:", test_volume_letter()))
            .expect("Failed to open volume");
        let mft = Mft::new(vol).expect("Failed to create MFT");

        b.iter(|| {
            let mut counter = 0;
            mft.iterate_files(|file| {
                let _info = FileInfo::new(&mft, file);
                counter += 1;
                if counter >= PARTIAL_ITERATION_LIMIT {
                    // Limit iteration for reasonable benchmark time
                }
            });
            black_box(counter)
        });
        println!("Completed bench_file_iteration_no_cache");
    }

    #[bench]
    fn bench_file_iteration_hashmap_cache(b: &mut Bencher) {
        println!(
            "Starting bench_file_iteration_hashmap_cache (limit: {})",
            PARTIAL_ITERATION_LIMIT
        );

        let vol = Volume::new(format!("\\\\.\\{}:", test_volume_letter()))
            .expect("Failed to open volume");
        let mft = Mft::new(vol).expect("Failed to create MFT");

        b.iter(|| {
            let mut cache = HashMapCache::default();
            let mut counter = 0;
            mft.iterate_files(|file| {
                let _info = FileInfo::with_cache(&mft, file, &mut cache);
                counter += 1;
                if counter >= PARTIAL_ITERATION_LIMIT {}
            });
            black_box(counter)
        });
        println!("Completed bench_file_iteration_hashmap_cache");
    }

    #[bench]
    fn bench_file_iteration_vec_cache(b: &mut Bencher) {
        println!(
            "Starting bench_file_iteration_vec_cache (limit: {})",
            PARTIAL_ITERATION_LIMIT
        );

        let vol = Volume::new(format!("\\\\.\\{}:", test_volume_letter()))
            .expect("Failed to open volume");
        let mft = Mft::new(vol).expect("Failed to create MFT");

        b.iter(|| {
            let mut cache = VecCache::default();
            cache.0.resize(mft.max_record as usize, None);
            let mut counter = 0;
            mft.iterate_files(|file| {
                let _info = FileInfo::with_cache(&mft, file, &mut cache);
                counter += 1;
                if counter >= PARTIAL_ITERATION_LIMIT {}
            });
            black_box(counter)
        });
        println!("Completed bench_file_iteration_vec_cache");
    }

    #[bench]
    fn bench_full_iteration_no_cache(b: &mut Bencher) {
        println!("Starting bench_full_iteration_no_cache (full iteration)");

        let vol = Volume::new(format!("\\\\.\\{}:", test_volume_letter()))
            .expect("Failed to open volume");
        let mft = Mft::new(vol).expect("Failed to create MFT");

        b.iter(|| {
            let mut files = Vec::new();
            mft.iterate_files(|file| {
                files.push(FileInfo::new(&mft, file));
            });
            black_box(files.len())
        });
        println!("Completed bench_full_iteration_no_cache");
    }

    #[bench]
    fn bench_full_iteration_hashmap_cache(b: &mut Bencher) {
        println!("Starting bench_full_iteration_hashmap_cache (full iteration)");

        let vol = Volume::new(format!("\\\\.\\{}:", test_volume_letter()))
            .expect("Failed to open volume");
        let mft = Mft::new(vol).expect("Failed to create MFT");

        b.iter(|| {
            let mut cache = HashMapCache::default();
            let mut files = Vec::new();
            mft.iterate_files(|file| {
                files.push(FileInfo::with_cache(&mft, file, &mut cache));
            });
            black_box(files.len())
        });
        println!("Completed bench_full_iteration_hashmap_cache");
    }

    #[bench]
    fn bench_full_iteration_vec_cache(b: &mut Bencher) {
        println!("Starting bench_full_iteration_vec_cache (full iteration)");

        let vol = Volume::new(format!("\\\\.\\{}:", test_volume_letter()))
            .expect("Failed to open volume");
        let mft = Mft::new(vol).expect("Failed to create MFT");

        b.iter(|| {
            let mut cache = VecCache::default();
            cache.0.resize(mft.max_record as usize, None);
            let mut files = Vec::new();
            mft.iterate_files(|file| {
                files.push(FileInfo::with_cache(&mft, file, &mut cache));
            });
            black_box(files.len())
        });
        println!("Completed bench_full_iteration_vec_cache");
    }

    #[bench]
    fn bench_cache_drop_hashmap(b: &mut Bencher) {
        println!(
            "Starting bench_cache_drop_hashmap (limit: {})",
            CACHE_DROP_ITERATION_LIMIT
        );

        let vol = Volume::new(format!("\\\\.\\{}:", test_volume_letter()))
            .expect("Failed to open volume");
        let mft = Mft::new(vol).expect("Failed to create MFT");

        b.iter(|| {
            let mut cache = HashMapCache::default();
            // Populate cache
            let mut counter = 0;
            mft.iterate_files(|file| {
                let _info = FileInfo::with_cache(&mft, file, &mut cache);
                counter += 1;
                if counter >= CACHE_DROP_ITERATION_LIMIT {}
            });
            drop(black_box(cache));
        });
        println!("Completed bench_cache_drop_hashmap");
    }

    #[bench]
    fn bench_cache_drop_vec(b: &mut Bencher) {
        println!(
            "Starting bench_cache_drop_vec (limit: {})",
            CACHE_DROP_ITERATION_LIMIT
        );

        let vol =
            Volume::new(format!("\\\\.\\{}:", TEST_VOLUME_LETTER)).expect("Failed to open volume");
        let mft = Mft::new(vol).expect("Failed to create MFT");

        b.iter(|| {
            let mut cache = VecCache::default();
            cache.0.resize(mft.max_record as usize, None);
            // Populate cache
            let mut counter = 0;
            mft.iterate_files(|file| {
                let _info = FileInfo::with_cache(&mft, file, &mut cache);
                counter += 1;
                if counter >= CACHE_DROP_ITERATION_LIMIT {}
            });
            drop(black_box(cache));
        });
        println!("Completed bench_cache_drop_vec");
    }
}
