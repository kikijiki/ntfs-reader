// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! [`Volume`]: an NTFS volume opened by path, with its geometry.

use std::io::Read;
use std::mem::size_of;
use std::path::{Path, PathBuf};

use crate::{
    aligned_reader::open_volume,
    api::*,
    errors::{NtfsReaderError, NtfsReaderResult},
};

/// Volume layout derived from the boot sector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VolumeGeometry {
    pub cluster_size: u64,
    pub volume_size: u64,
    pub file_record_size: u64,
    pub mft_position: u64,
}

impl VolumeGeometry {
    /// Parses a raw boot sector (the first 512 bytes of the volume). Only
    /// used by tests below; `Volume::new` already has a parsed `BootSector`
    /// and calls `from_boot_sector` directly.
    #[cfg(test)]
    pub fn from_bytes(bytes: &[u8]) -> NtfsReaderResult<Self> {
        Self::from_boot_sector(&decode_boot_sector(bytes)?)
    }

    /// Computes the volume layout from a parsed boot sector.
    pub fn from_boot_sector(boot_sector: &BootSector) -> NtfsReaderResult<Self> {
        fn invalid(field: &'static str) -> NtfsReaderError {
            NtfsReaderError::InvalidBootSector { field }
        }

        // The OEM id lives inside `crap_0` (offset 3..11); `api::BootSector`
        // keeps the raw layout (spec 006), so read it from there instead of
        // adding a field.
        let oem_id: [u8; 8] = boot_sector.crap_0[3..11].try_into().unwrap();
        if oem_id != *b"NTFS    " {
            return Err(invalid("oem_id"));
        }

        let sector_size = boot_sector.sector_size as u64;
        if !(256..=4096).contains(&sector_size) || !sector_size.is_power_of_two() {
            return Err(invalid("sector_size"));
        }

        // `sectors_per_cluster` above 0x80 encodes a negative power-of-two
        // exponent: 2^(256 - v) sectors per cluster, instead of a plain count.
        let sectors_per_cluster = boot_sector.sectors_per_cluster;
        let cluster_size = if sectors_per_cluster > 0x80 {
            let exponent = 256u32 - sectors_per_cluster as u32;
            1u64.checked_shl(exponent)
                .and_then(|sectors| sectors.checked_mul(sector_size))
        } else {
            (sectors_per_cluster as u64).checked_mul(sector_size)
        }
        .filter(|size| size.is_power_of_two())
        .ok_or_else(|| invalid("sectors_per_cluster"))?;

        let volume_size = boot_sector
            .total_sectors
            .checked_mul(sector_size)
            .ok_or_else(|| invalid("total_sectors"))?;

        // Positive `file_record_size_info` counts clusters; zero or negative
        // encodes 2^(-info) bytes. Either way the result must land on a
        // power of two in 512..=65536; that single check is the gate (no
        // separate bound on the exponent itself).
        let file_record_size_info = boot_sector.file_record_size_info;
        let file_record_size = if file_record_size_info > 0 {
            (file_record_size_info as u64).checked_mul(cluster_size)
        } else {
            file_record_size_info
                .checked_neg()
                .and_then(|exponent| 1u64.checked_shl(exponent as u32))
        }
        .filter(|size| (512..=65536).contains(size) && size.is_power_of_two())
        .ok_or_else(|| invalid("file_record_size_info"))?;

        let mft_position = boot_sector
            .mft_lcn
            .checked_mul(cluster_size)
            .ok_or_else(|| invalid("mft_lcn"))?;

        Ok(Self {
            cluster_size,
            volume_size,
            file_record_size,
            mft_position,
        })
    }
}

/// Copies a [`BootSector`] out of a raw byte buffer. No alignment
/// requirement: the struct is `repr(C, packed)`, so any byte pointer is
/// valid for it (see `docs/architecture.md` on packed on-disk structs).
fn decode_boot_sector(bytes: &[u8]) -> NtfsReaderResult<BootSector> {
    if bytes.len() < size_of::<BootSector>() {
        return Err(NtfsReaderError::InvalidBootSector { field: "length" });
    }
    // SAFETY: `bytes` holds a whole boot sector (checked above), and the struct is a packed
    // `Copy` one of plain integers and byte arrays (alignment 1).
    Ok(unsafe { *(bytes.as_ptr() as *const BootSector) })
}

/// An NTFS volume: its path plus the geometry read from its boot sector.
/// [`Mft::new`](crate::Mft::new) and `Journal::new` both take ownership
/// of one, and reopen the volume by its path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    path: PathBuf,
    cluster_size: u64,
    volume_size: u64,
    file_record_size: u64,
    mft_position: u64,
}

impl Volume {
    /// Opens the volume at `path` and reads its boot sector. `path` is a
    /// Win32 device path such as `\\.\C:` or `\\?\C:`, naming the volume
    /// itself rather than a file on it.
    ///
    /// Opening a raw volume needs an elevated (administrator) process;
    /// without one this fails with [`NtfsReaderError::AccessDenied`]. So does
    /// a path that names a directory (`C:\`) instead of the volume
    /// (`\\.\C:`), elevated or not. A path with a NUL character in it fails
    /// with [`NtfsReaderError::InvalidVolumePath`], before anything is opened.
    pub fn new<P: AsRef<Path>>(path: P) -> NtfsReaderResult<Self> {
        // A NUL ends a Windows path string early, so a path holding one would name a shorter
        // path than the caller wrote (and `Journal::new` reopens the volume by this path).
        if path.as_ref().as_os_str().as_encoded_bytes().contains(&0) {
            return Err(NtfsReaderError::InvalidVolumePath);
        }
        // `NtfsReaderError::from(io::Error)` maps a refused open to `AccessDenied`.
        let mut reader = open_volume(path.as_ref())?;
        let mut boot_sector_bytes = [0u8; size_of::<BootSector>()];
        reader.read_exact(&mut boot_sector_bytes)?;
        let boot_sector = decode_boot_sector(&boot_sector_bytes)?;
        let VolumeGeometry {
            cluster_size,
            volume_size,
            file_record_size,
            mft_position,
        } = VolumeGeometry::from_boot_sector(&boot_sector)?;

        Ok(Volume {
            path: path.as_ref().into(),
            cluster_size,
            volume_size,
            file_record_size,
            mft_position,
        })
    }

    /// The path the volume was opened with. It holds no NUL character.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Size of a cluster in bytes.
    pub fn cluster_size(&self) -> u64 {
        self.cluster_size
    }

    /// Size of the volume in bytes.
    pub fn volume_size(&self) -> u64 {
        self.volume_size
    }

    /// Size of one MFT record in bytes (usually 1024).
    pub fn file_record_size(&self) -> u64 {
        self.file_record_size
    }

    /// Byte offset of the start of the `$MFT` from the start of the volume.
    pub fn mft_position(&self) -> u64 {
        self.mft_position
    }

    /// A `Volume` that was not read from a boot sector, for unit tests and
    /// benches that never touch a real volume. A
    /// `volume_size` of 0 means "unknown, do not bound sizes by it".
    #[cfg(any(test, feature = "internals"))]
    #[doc(hidden)]
    pub fn synthetic(
        path: impl Into<PathBuf>,
        cluster_size: u64,
        volume_size: u64,
        file_record_size: u64,
        mft_position: u64,
    ) -> Self {
        Volume {
            path: path.into(),
            cluster_size,
            volume_size,
            file_record_size,
            mft_position,
        }
    }

    /// Replaces the volume size of a synthetic volume.
    #[cfg(any(test, feature = "internals"))]
    #[doc(hidden)]
    pub fn with_volume_size(mut self, volume_size: u64) -> Self {
        self.volume_size = volume_size;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A NUL ends a Windows path string early, so `CreateFileW` would open a shorter path than the
    // one asked for. It is refused before anything is opened: no volume exists at this path, so
    // any other error means it was not.
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

    #[test]
    fn geometry_rejects_overflowing_volume_size() {
        assert_invalid(
            &boot_sector(512, 8, u64::MAX, 786_432, -10),
            "total_sectors",
        );
    }

    // `Volume::new` has no test for a non-elevated process: the VM runs elevated (see
    // docs/windows-vm-testing.md), so the `PermissionDenied` to `AccessDenied` mapping is only
    // covered by `From<io::Error>`'s own tests.
}
