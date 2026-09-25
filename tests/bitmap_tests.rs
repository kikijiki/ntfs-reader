#![cfg(target_os = "windows")]

//! Checks `ClusterBitmap` against what Windows itself reports for the same volume: the volume's
//! own cluster figures (`FSCTL_GET_NTFS_VOLUME_DATA`, `GetDiskFreeSpaceExW`) and the allocation
//! bitmap NTFS holds in memory (`FSCTL_GET_VOLUME_BITMAP`).
//!
//! The crate reads `$Bitmap` from the raw volume, the ioctl reads NTFS's in-memory copy, and the
//! volume keeps changing underneath both (USN journal, lazy writer). Free-cluster counts are
//! compared against the ioctl's count taken just before and just after: the crate's count must
//! fall within that range, plus a small margin.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;

use ntfs_reader::{ClusterBitmap, Mft, Volume};
use windows::core::HSTRING;
use windows::Win32::Foundation::{ERROR_MORE_DATA, HANDLE};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Ioctl::{
    FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_GET_VOLUME_BITMAP, NTFS_VOLUME_DATA_BUFFER,
};
use windows::Win32::System::IO::DeviceIoControl;

mod common;
use common::{flush_volume, test_volume_letter, TempDirGuard};

/// Slack beyond what changed between the ioctl's two snapshots: the bitmap the crate reads is
/// flushed a moment before or after the one NTFS holds.
const MARGIN: u64 = 64;

fn open_volume(letter: &str) -> File {
    OpenOptions::new()
        .read(true)
        .share_mode(7)
        .open(format!("\\\\.\\{letter}:"))
        .unwrap_or_else(|e| panic!("open volume {letter}: {e}"))
}

fn ntfs_volume_data(volume: &File) -> NTFS_VOLUME_DATA_BUFFER {
    let mut data = NTFS_VOLUME_DATA_BUFFER::default();
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            HANDLE(volume.as_raw_handle()),
            FSCTL_GET_NTFS_VOLUME_DATA,
            None,
            0,
            Some(&mut data as *mut NTFS_VOLUME_DATA_BUFFER as *mut c_void),
            std::mem::size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
            Some(&mut returned),
            None,
        )
    }
    .expect("FSCTL_GET_NTFS_VOLUME_DATA");
    data
}

/// The number of free clusters among the first `total` of the volume, from the ioctl.
fn ioctl_free_clusters(volume: &File, total: u64) -> u64 {
    let input = 0i64.to_le_bytes();
    // The header is the starting LCN and the number of bits, then the bits.
    let mut out = vec![0u8; 16 + total.div_ceil(8) as usize + 8];
    let mut returned = 0u32;
    let result = unsafe {
        DeviceIoControl(
            HANDLE(volume.as_raw_handle()),
            FSCTL_GET_VOLUME_BITMAP,
            Some(input.as_ptr() as *const c_void),
            input.len() as u32,
            Some(out.as_mut_ptr() as *mut c_void),
            out.len() as u32,
            Some(&mut returned),
            None,
        )
    };
    if let Err(e) = result {
        assert_ne!(
            e.code(),
            ERROR_MORE_DATA.to_hresult(),
            "the buffer was sized for every cluster"
        );
        panic!("FSCTL_GET_VOLUME_BITMAP: {e}");
    }
    let starting_lcn = i64::from_le_bytes(out[0..8].try_into().unwrap());
    let bits = i64::from_le_bytes(out[8..16].try_into().unwrap()) as u64;
    assert_eq!(starting_lcn, 0);
    assert!(
        bits >= total,
        "the ioctl returned {bits} bits for a volume of {total} clusters"
    );
    // Only the first `total` bits: what is in the padding of the last byte is nobody's cluster.
    let bytes = &out[16..16 + total.div_ceil(8) as usize];
    let (whole, rest) = ((total / 8) as usize, (total % 8) as u32);
    let mut allocated: u64 = bytes[..whole]
        .iter()
        .map(|byte| u64::from(byte.count_ones()))
        .sum();
    if rest > 0 {
        allocated += u64::from((bytes[whole] & ((1u8 << rest) - 1)).count_ones());
    }
    total - allocated
}

/// The volume's total size in bytes, from `GetDiskFreeSpaceExW`.
fn win32_total_bytes(letter: &str) -> u64 {
    let root = HSTRING::from(format!("{letter}:\\"));
    let mut total = 0u64;
    unsafe { GetDiskFreeSpaceExW(&root, None, Some(&mut total), None) }
        .expect("GetDiskFreeSpaceExW");
    total
}

/// The number of clusters the crate's bitmap says are free.
fn crate_free_clusters(bitmap: &ClusterBitmap) -> u64 {
    (0..bitmap.cluster_count())
        .filter(|&cluster| !bitmap.is_allocated(cluster))
        .count() as u64
}

// The crate's boot-sector-derived cluster size and count are the volume's own; its free-cluster
// count from `$Bitmap` matches what NTFS reports for the same moment.
#[test]
fn the_cluster_bitmap_agrees_with_the_volume_and_its_bitmap_ioctl() {
    let letter = test_volume_letter();
    let volume = open_volume(&letter);
    let data = ntfs_volume_data(&volume);
    let total = data.TotalClusters as u64;
    let cluster_size = u64::from(data.BytesPerCluster);

    // Some allocation on the volume, so the bitmap is not all one state.
    let dir = TempDirGuard::new(format!("{letter}:\\bitmap-oracle")).expect("fixture directory");
    std::fs::write(dir.path().join("allocated.bin"), vec![0x5au8; 8 << 20]).unwrap();

    let before = ioctl_free_clusters(&volume, total);
    flush_volume(&letter);
    let mft = Mft::new(Volume::new(format!("\\\\.\\{letter}:")).expect("open the volume"))
        .expect("load the MFT");
    let bitmap = ClusterBitmap::new(&mft).expect("read the cluster bitmap");
    let after = ioctl_free_clusters(&volume, total);

    // The volume's own figures, not the crate's division.
    assert_eq!(bitmap.cluster_size(), cluster_size, "cluster size");
    assert_eq!(bitmap.cluster_count(), total, "cluster count");
    let win32_total = win32_total_bytes(&letter);
    assert_eq!(
        win32_total / cluster_size,
        bitmap.cluster_count(),
        "GetDiskFreeSpaceExW says the volume is {win32_total} bytes"
    );

    let free = crate_free_clusters(&bitmap);
    let (low, high) = (before.min(after), before.max(after));
    eprintln!(
        "free clusters: ioctl {before} then {after}, crate {free} of {total} (FreeClusters {})",
        data.FreeClusters
    );
    assert!(
        free > 0 && free < total,
        "a bitmap that is all one state proves nothing: {free} free of {total}"
    );
    assert!(
        free.abs_diff(low) <= (high - low) + MARGIN,
        "the crate counts {free} free clusters, the volume {before} before and {after} after"
    );
}

/// The bits `FSCTL_GET_VOLUME_BITMAP` returns for `count` clusters from `first`: the LCN its first
/// bit stands for (the driver rounds `first` down to a multiple of 8) and the bits, allocated true.
fn ioctl_bits(volume: &File, first: u64, count: u64) -> (u64, Vec<bool>) {
    let input = (first as i64).to_le_bytes();
    let mut out = vec![0u8; 16 + (count / 8 + 16) as usize];
    let mut returned = 0u32;
    let result = unsafe {
        DeviceIoControl(
            HANDLE(volume.as_raw_handle()),
            FSCTL_GET_VOLUME_BITMAP,
            Some(input.as_ptr() as *const c_void),
            input.len() as u32,
            Some(out.as_mut_ptr() as *mut c_void),
            out.len() as u32,
            Some(&mut returned),
            None,
        )
    };
    // A buffer that is too short still holds what fits, which is all that is used.
    if let Err(e) = result {
        assert_eq!(
            e.code(),
            ERROR_MORE_DATA.to_hresult(),
            "FSCTL_GET_VOLUME_BITMAP: {e}"
        );
    }
    let start = i64::from_le_bytes(out[0..8].try_into().unwrap()) as u64;
    let valid = i64::from_le_bytes(out[8..16].try_into().unwrap()) as u64;
    let bits = out[16..]
        .iter()
        .flat_map(|byte| (0..8).map(move |bit| byte & (1 << bit) != 0))
        .take(valid.min(count + 16) as usize)
        .collect();
    (start, bits)
}

// Bit order within a byte, and byte order, cannot be seen in a count: a stretch of the bitmap
// around some files is compared cluster by cluster against the driver's own bitmap, on a volume
// where allocated runs have lengths and starts that are not multiples of 8 (small files of odd
// sizes, every other one deleted). The two snapshots are taken at slightly different moments, so
// a cluster may differ only where the driver's own bitmap changed between them.
#[test]
fn the_cluster_bitmap_agrees_with_the_bitmap_ioctl_cluster_by_cluster() {
    const WINDOW: u64 = 64 * 1024;
    let letter = test_volume_letter();
    let volume = open_volume(&letter);
    let total = ntfs_volume_data(&volume).TotalClusters as u64;

    let dir = TempDirGuard::new(format!("{letter}:\\bitmap-bit-order")).expect("fixture directory");
    for index in 0..40usize {
        let size = 5000 + index * 7919;
        std::fs::write(
            dir.path().join(format!("bit-order-{index:02}.bin")),
            vec![0xa5u8; size],
        )
        .unwrap();
    }
    for index in (0..40usize).step_by(2) {
        std::fs::remove_file(dir.path().join(format!("bit-order-{index:02}.bin"))).unwrap();
    }
    flush_volume(&letter);
    let mft = Mft::new(Volume::new(format!("\\\\.\\{letter}:")).expect("open the volume"))
        .expect("load the MFT");
    let bitmap = ClusterBitmap::new(&mft).expect("read the cluster bitmap");

    // Located via the crate's own view (what is under test, so used here only to point at the
    // interesting part of the bitmap).
    let stream = mft
        .files()
        .find(|file| {
            file.hard_links()
                .any(|link| link.to_string() == "bit-order-39.bin")
        })
        .expect("a fixture file is in the MFT")
        .open_stream(None)
        .expect("open it");
    let ntfs_reader::ExtentLocation::Volume { offset } = stream.extents()[0].location else {
        panic!("the fixture file is not stored in clusters");
    };
    let near = offset / bitmap.cluster_size();
    let first = near.saturating_sub(WINDOW / 2) & !7;
    let end = (first + WINDOW).min(total);
    assert!(
        end - first >= WINDOW.min(total),
        "the window is {} clusters",
        end - first
    );

    let (before_start, before) = ioctl_bits(&volume, first, end - first);
    flush_volume(&letter);
    let bitmap_now = ClusterBitmap::new(&mft).expect("read the cluster bitmap again");
    let (after_start, after) = ioctl_bits(&volume, first, end - first);
    assert_eq!((before_start, after_start), (first, first));
    assert!(before.len() as u64 >= end - first && after.len() as u64 >= end - first);
    drop(bitmap);

    let mut differing = 0;
    let mut runs = 0;
    for cluster in first..end {
        let index = (cluster - first) as usize;
        let ours = bitmap_now.is_allocated(cluster);
        if index > 0 && before[index] != before[index - 1] {
            runs += 1;
        }
        if before[index] != after[index] {
            // The driver's own bitmap moved here: either answer is right.
            continue;
        }
        if ours != before[index] {
            differing += 1;
            assert!(
                differing < 20,
                "cluster {cluster} (bit {} of byte {}) is {ours} in the crate and {} in the ioctl",
                cluster % 8,
                cluster / 8,
                before[index]
            );
        }
    }
    assert_eq!(
        differing, 0,
        "clusters where the crate and the ioctl disagree"
    );
    assert!(
        runs >= 20,
        "the stretch has only {runs} changes between allocated and free: it cannot show a bit order"
    );
    eprintln!(
        "bit order: {} clusters from {first} agree, {runs} allocation changes",
        end - first
    );
}
