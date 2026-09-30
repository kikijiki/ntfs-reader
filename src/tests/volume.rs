// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use super::*;

struct FlatImage(PathBuf);

impl FlatImage {
    fn new(boot: &[u8], length: u64) -> Self {
        use std::io::Write;
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_IMAGE: AtomicU64 = AtomicU64::new(0);
        let number = NEXT_IMAGE.fetch_add(1, Ordering::Relaxed);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ntfs-reader-volume-image-{}-{nonce}-{number}.img",
            std::process::id()
        ));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("create a unique synthetic image");
        let image = Self(path);
        file.write_all(boot).unwrap();
        file.set_len(length).unwrap();
        image
    }

    fn set_len(&self, length: u64) {
        std::fs::OpenOptions::new()
            .write(true)
            .open(&self.0)
            .unwrap()
            .set_len(length)
            .unwrap();
    }
}

impl Drop for FlatImage {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// A NUL ends a Windows path string early, so `CreateFileW` would open a shorter path than the
// one asked for. This must be rejected before any open attempt, not surfaced as a not-found error.
#[test]
fn a_path_with_an_interior_nul_is_invalid() {
    for path in ["\\\\?\\T:\0evil", "\0", "\\\\.\\T:\0"] {
        let result = Volume::new(path);
        assert!(
            matches!(result, Err(NtfsReaderError::InvalidVolumePath)),
            "{path:?}: {result:?}"
        );
    }
}

/// A 512-byte NTFS boot sector with the given geometry fields.
fn boot_sector(
    sector_size: u16,
    sectors_per_cluster: u8,
    total_sectors: u64,
    mft_lcn: u64,
    file_record_size_info: i8,
) -> [u8; 512] {
    let mut bytes = [0u8; 512];
    bytes[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
    bytes[3..11].copy_from_slice(b"NTFS    ");
    bytes[0x0B..0x0D].copy_from_slice(&sector_size.to_le_bytes());
    bytes[0x0D] = sectors_per_cluster;
    bytes[0x15] = 0xF8;
    bytes[0x28..0x30].copy_from_slice(&total_sectors.to_le_bytes());
    bytes[0x30..0x38].copy_from_slice(&mft_lcn.to_le_bytes());
    bytes[0x38..0x40].copy_from_slice(&2u64.to_le_bytes());
    bytes[0x40] = file_record_size_info as u8;
    bytes[0x44] = 1;
    bytes[510] = 0x55;
    bytes[511] = 0xAA;
    bytes
}

fn typical() -> [u8; 512] {
    boot_sector(512, 8, 1_000_000, 786_432, -10)
}

#[track_caller]
fn assert_invalid(bytes: &[u8], expected_field: &str) {
    match VolumeGeometry::from_bytes(bytes) {
        Err(NtfsReaderError::InvalidBootSector { field }) => assert_eq!(field, expected_field),
        other => panic!("expected InvalidBootSector({expected_field}), got {other:?}"),
    }
}

#[test]
fn geometry_of_typical_volume() {
    let geometry = VolumeGeometry::from_bytes(&typical()).expect("valid boot sector");
    assert_eq!(
        geometry,
        VolumeGeometry {
            cluster_size: 4096,
            volume_size: 512_000_000,
            file_record_size: 1024,
            mft_position: 786_432 * 4096,
        }
    );
}

#[test]
fn geometry_positive_record_size_counts_clusters() {
    // 64 KiB clusters are the largest plain count (0x80 = 128 sectors).
    let geometry = VolumeGeometry::from_bytes(&boot_sector(512, 0x80, 1 << 20, 4, 1))
        .expect("valid boot sector");
    assert_eq!(geometry.cluster_size, 64 * 1024);
    assert_eq!(geometry.file_record_size, 64 * 1024);
    assert_eq!(geometry.mft_position, 4 * 64 * 1024);
}

#[test]
fn geometry_decodes_large_cluster_exponent() {
    // 0xF4: 2^(256 - 244) = 4096 sectors of 512 bytes = 2 MiB.
    let geometry = VolumeGeometry::from_bytes(&boot_sector(512, 0xF4, 1 << 24, 3, -10))
        .expect("valid boot sector");
    assert_eq!(geometry.cluster_size, 2 * 1024 * 1024);
    assert_eq!(geometry.mft_position, 3 * 2 * 1024 * 1024);
    assert_eq!(geometry.file_record_size, 1024);

    // 0xF7 with 4 KiB sectors: 2^9 sectors = 2 MiB.
    let geometry = VolumeGeometry::from_bytes(&boot_sector(4096, 0xF7, 1 << 20, 3, -12))
        .expect("valid boot sector");
    assert_eq!(geometry.cluster_size, 2 * 1024 * 1024);
    assert_eq!(geometry.file_record_size, 4096);
}

#[test]
fn geometry_rejects_invalid_record_size_info() {
    // -128 cannot be negated, -64 is 2^64 bytes and 0 is a one-byte record.
    for info in [-128, -64, 0] {
        assert_invalid(
            &boot_sector(512, 8, 1_000_000, 786_432, info),
            "file_record_size_info",
        );
    }
}

#[test]
fn geometry_rejects_oversized_record() {
    // 64 clusters of 4 KiB = 256 KiB, above the 64 KiB limit.
    assert_invalid(
        &boot_sector(512, 8, 1_000_000, 786_432, 64),
        "file_record_size_info",
    );
}

#[test]
fn geometry_rejects_non_ntfs_oem_id() {
    let mut bytes = typical();
    bytes[3..11].copy_from_slice(b"MSDOS5.0");
    assert_invalid(&bytes, "oem_id");
}

#[test]
fn geometry_rejects_zero_sectors_per_cluster() {
    assert_invalid(
        &boot_sector(512, 0, 1_000_000, 786_432, -10),
        "sectors_per_cluster",
    );
}

#[test]
fn geometry_rejects_bad_sector_size() {
    for sector_size in [0u16, 1, 128, 1000, 8192] {
        assert_invalid(
            &boot_sector(sector_size, 8, 1_000_000, 786_432, -10),
            "sector_size",
        );
    }
}

#[test]
fn geometry_rejects_overflowing_mft_position() {
    assert_invalid(
        &boot_sector(512, 8, 1_000_000, u64::MAX / 2, -10),
        "mft_lcn",
    );
}

// Zero sectors leaves no size to validate against. Tests that need a zero-sized volume use
// `Volume::synthetic`, not a boot sector.
#[test]
fn geometry_rejects_zero_total_sectors() {
    assert_invalid(&boot_sector(512, 8, 0, 786_432, -10), "total_sectors");
}

#[test]
fn geometry_rejects_overflowing_volume_size() {
    assert_invalid(
        &boot_sector(512, 8, u64::MAX, 786_432, -10),
        "total_sectors",
    );
}

// Exercise the same construction path as Volume::new, with bytes and an independent
// backing-device length instead of a real Windows handle.
#[test]
fn volume_rejects_a_boot_size_wildly_larger_than_its_device() {
    let boot = boot_sector(512, 8, 1 << 40, 4, -10);
    let result = Volume::from_reader(Path::new("volume"), boot.as_slice(), 1 << 20);
    assert!(
        matches!(
            result,
            Err(NtfsReaderError::InvalidBootSector {
                field: "total_sectors"
            })
        ),
        "a 1 MiB device must reject a 512 TiB boot size, got {result:?}"
    );
}

#[test]
fn volume_rejects_any_boot_size_above_its_device_length() {
    // Include the first byte/sector above the bound and sizes near both integer limits.
    for (sectors, device_size) in [
        (16, 8191),
        (16, 7680),
        (1 << 54, i64::MAX as u64),
        (u64::MAX / 512, i64::MAX as u64),
    ] {
        let boot = boot_sector(512, 8, sectors, 1, -10);
        let result = Volume::from_reader(Path::new("volume"), boot.as_slice(), device_size);
        assert!(
            matches!(
                result,
                Err(NtfsReaderError::InvalidBootSector {
                    field: "total_sectors"
                })
            ),
            "{sectors} sectors on a {device_size} byte device: {result:?}"
        );
    }
}

#[test]
fn volume_accepts_equal_or_larger_devices_and_keeps_the_boot_size() {
    for sector_size in [512, 4096] {
        let boot = boot_sector(sector_size, 8, 64, 4, -10);
        let size = u64::from(sector_size) * 64;
        // A volume may have one trailing sector or a larger region of unused backing space.
        for device_size in [size, size + u64::from(sector_size), 1 << 40] {
            let volume = Volume::from_reader(Path::new("volume"), boot.as_slice(), device_size)
                .expect("the file system fits in its device");
            assert_eq!(volume.volume_size(), size, "keep the file-system boundary");
            assert_eq!(volume.cluster_size(), u64::from(sector_size) * 8);
            assert_eq!(volume.path(), Path::new("volume"));
        }
    }
}

#[test]
fn device_length_rejects_incomplete_or_nonpositive_replies() {
    for (length, returned) in [
        (4096, 0),
        (4096, 7),
        (4096, 9),
        (0, 8),
        (-1, 8),
        (i64::MIN, 8),
    ] {
        let error = checked_device_length(length, returned)
            .expect_err("a malformed reply must not become a usable device length");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
    for length in [1, 4096, i64::MAX] {
        assert_eq!(checked_device_length(length, 8).unwrap(), length as u64);
    }
}

#[test]
fn ordinary_image_file_opens_and_keeps_its_boot_size() {
    let image = FlatImage::new(&boot_sector(512, 8, 16, 1, -10), 8192);
    for length in [8192, 8704, 65536] {
        image.set_len(length);
        let volume = Volume::new(&image.0).expect("open a flat image using its file length");
        assert_eq!(volume.volume_size(), 8192);
        assert_eq!(volume.cluster_size(), 4096);
        assert_eq!(volume.path(), image.0);
    }
}

#[test]
fn ordinary_image_file_rejects_a_boot_size_larger_than_the_file() {
    // One byte too short, or a huge claim in a small file: the containing disk's
    // capacity must never replace the image's own logical file length.
    for (sectors, length) in [(16, 8191), (1 << 40, 8192)] {
        let image = FlatImage::new(&boot_sector(512, 8, sectors, 1, -10), length);
        let result = Volume::new(&image.0);
        assert!(
            matches!(
                result,
                Err(NtfsReaderError::InvalidBootSector {
                    field: "total_sectors"
                })
            ),
            "{sectors} sectors in a {length} byte image must be refused: {result:?}"
        );
    }
}

// `Volume::new` has no test for a non-elevated process: the test machine runs elevated, so
// the `PermissionDenied` to `AccessDenied` mapping is only covered by `From<io::Error>`'s own
// tests.
