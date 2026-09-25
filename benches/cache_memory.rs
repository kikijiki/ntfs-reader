//! Heap held by the loaded `Mft` and by a `DefaultPathCache` after resolving paths, measured with
//! a counting allocator. Also one wall-clock pass of `Mft::new` and of a full scan, for per-file
//! costs on a big volume (the stress volume, see CONTRIBUTING.md). Prints Markdown tables.
//! Statistical timing is in `mft_benchmark`. Needs an elevated shell; volume from
//! `test_volume_letter()`.
//!
//! Windows-only regardless of the `internals` feature (see `mft_benchmark.rs`'s top comment):
//! every item below is `#[cfg(windows)]`, with a `#[cfg(not(windows))]` `main` stub.

#[cfg(windows)]
use std::alloc::{GlobalAlloc, Layout, System};
#[cfg(windows)]
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(windows)]
use ntfs_reader::{DefaultPathCache, FileInfo, Mft, PathCache, Volume};
#[cfg(windows)]
use std::time::{Duration, Instant};

#[cfg(windows)]
#[path = "../tests/common/mod.rs"]
mod common;
#[cfg(windows)]
use common::test_volume_letter;

#[cfg(windows)]
struct Counting;

#[cfg(windows)]
static LIVE: AtomicUsize = AtomicUsize::new(0);

#[cfg(windows)]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        System.alloc(layout)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        System.alloc_zeroed(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_add(new_size, Ordering::Relaxed);
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        System.realloc(ptr, layout, new_size)
    }
}

#[cfg(windows)]
#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[cfg(windows)]
fn measure(mft: &Mft, scenario: &str, numbers: &[u64]) {
    let before = LIVE.load(Ordering::Relaxed);
    let mut cache = DefaultPathCache::new();
    for file in numbers.iter().filter_map(|&number| mft.record(number)) {
        drop(FileInfo::with_cache(&file, &mut cache));
    }
    let bytes = LIVE.load(Ordering::Relaxed) - before;
    println!(
        "| {scenario} | {} | {:.2} MiB |",
        cache.len(),
        bytes as f64 / (1024.0 * 1024.0)
    );
}

/// One pass over every file with `cache`. Returns the elapsed time.
#[cfg(windows)]
fn full_scan(mft: &Mft, cache: &mut impl PathCache) -> Duration {
    let start = Instant::now();
    for file in mft.files() {
        drop(FileInfo::with_cache(&file, cache));
    }
    start.elapsed()
}

#[cfg(windows)]
fn mib(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

#[cfg(windows)]
fn main() {
    let letter = test_volume_letter();
    let heap_before = LIVE.load(Ordering::Relaxed);
    let start = Instant::now();
    let volume = Volume::new(format!("\\\\.\\{letter}:")).expect("open volume");
    let mft = Mft::new(volume).expect("read MFT");
    let load_time = start.elapsed();
    let mft_heap = LIVE.load(Ordering::Relaxed) - heap_before;
    let files: Vec<u64> = mft.files().map(|file| file.number()).collect();
    println!(
        "Volume {letter}: {} files, {} records, MFT {:.1} MiB in memory\n",
        files.len(),
        mft.record_count(),
        mib(mft.size_in_memory())
    );

    println!("| Step | Time | Per file | Heap |");
    println!("| --- | --- | --- | --- |");
    let per_file = |time: Duration| {
        format!(
            "{:.2} us",
            time.as_secs_f64() * 1e6 / files.len().max(1) as f64
        )
    };
    println!(
        "| Mft::new | {load_time:.2?} | {} | {:.1} MiB |",
        per_file(load_time),
        mib(mft_heap)
    );
    let time = full_scan(&mft, &mut ());
    println!(
        "| full scan, no cache | {time:.2?} | {} | |",
        per_file(time)
    );
    let before = LIVE.load(Ordering::Relaxed);
    let mut cache = DefaultPathCache::new();
    let time = full_scan(&mft, &mut cache);
    let cache_heap = LIVE.load(Ordering::Relaxed) - before;
    println!(
        "| full scan, DefaultPathCache | {time:.2?} | {} | {:.1} MiB |\n",
        per_file(time),
        mib(cache_heap)
    );
    drop(cache);

    println!("| Files resolved | Directories cached | Heap |");
    println!("| --- | --- | --- |");
    for count in [10, 100, 1000, files.len()] {
        let step = (files.len() / count).max(1);
        let numbers: Vec<u64> = files.iter().copied().step_by(step).take(count).collect();
        let scenario = if count == files.len() {
            format!("all ({count})")
        } else {
            count.to_string()
        };
        measure(&mft, &scenario, &numbers);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("cache_memory: windows-only (needs a real volume); skipped on this platform");
}
